//! Search public model repositories and save chosen files into projects.
//!
//! The library is disabled unless `PRINTABLE_LIBRARY_SITES` names at least one
//! site, because it is the only Printable feature that contacts the public
//! internet. Every saved file is streamed into managed scratch space, hashed,
//! then committed atomically, and a `source.json` record keeps its origin and
//! license beside it.

use std::{collections::BTreeSet, fmt::Write as _, io::Write as _, sync::Arc};

use model_library::{
    Client, Credentials, FileStream, LibraryError, ModelDetail, SearchQuery, SearchSort, Site,
    Token,
};
use printable_workspace::{ALLOWED_ARTIFACT_SUFFIXES, ArtifactMeta, ManagedScratch, Workspace};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use tokio::sync::Semaphore;

use crate::{config::SettingsError, error::ToolError};

const SITES_SETTING: &str = "PRINTABLE_LIBRARY_SITES";
const THINGIVERSE_TOKEN_SETTING: &str = "PRINTABLE_THINGIVERSE_TOKEN";
/// Library requests served at once, so a busy agent cannot flood a site.
const CONCURRENT_REQUESTS: usize = 2;
/// Files one download request may save.
const MAXIMUM_FILES: usize = 50;
const MANIFEST: &str = "source.json";
const MAXIMUM_STEM_CHARACTERS: usize = 100;
/// The longest file or directory name Linux file systems accept.
const MAXIMUM_COMPONENT_BYTES: usize = 255;

fn first_page() -> u32 {
    1
}

fn default_limit() -> u32 {
    10
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    pub site: Site,
    /// Keywords, 1 to 200 characters.
    pub query: String,
    #[serde(default)]
    pub sort: SearchSort,
    /// One-based page number.
    #[serde(default = "first_page")]
    #[schemars(range(min = 1, max = 1000))]
    pub page: u32,
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 50))]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    pub site: Site,
    /// Numeric model ID from search, or a model page link.
    pub model: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DownloadParams {
    pub site: Site,
    /// Numeric model ID from search, or a model page link.
    pub model: String,
    /// `file_id` values listed by `get`.
    #[schemars(length(min = 1, max = 50))]
    pub files: Vec<String>,
    pub project_id: String,
    /// Project-relative directory; defaults to `library/<site>-<model_id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// Replace existing files and `source.json` in the directory.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LibraryRequest {
    Search(SearchParams),
    Get(GetParams),
    Download(DownloadParams),
    /// Sign in to the server's Bambu Lab account for MakerWorld downloads.
    SignIn(SignInParams),
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignInParams {
    /// Only `makerworld` uses sign-in.
    pub site: Site,
    /// The code the user received by email or reads from their authenticator
    /// app. Omit it to start a new sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl LibraryRequest {
    fn site(&self) -> Site {
        match self {
            Self::Search(params) => params.site,
            Self::Get(params) => params.site,
            Self::Download(params) => params.site,
            Self::SignIn(params) => params.site,
        }
    }
}

/// Configured model sites and the bounded client that reaches them.
pub struct LibraryService {
    client: Client,
    sites: BTreeSet<Site>,
    gate: Semaphore,
    makerworld: account::MakerWorldAccount,
}

impl std::fmt::Debug for LibraryService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LibraryService")
            .field("sites", &self.sites)
            .finish_non_exhaustive()
    }
}

/// Read the library settings. No network request is made here.
pub fn configure(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Option<Arc<LibraryService>>, SettingsError> {
    let Some(list) = get(SITES_SETTING).filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let sites = list
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| Site::from_id(name).ok_or(SettingsError::Library(SITES_SETTING)))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if sites.is_empty() {
        return Err(SettingsError::Library(SITES_SETTING));
    }
    let token = |key: &'static str| {
        get(key)
            .filter(|value| !value.is_empty())
            .map(|value| Token::new(value).map_err(|_| SettingsError::Library(key)))
            .transpose()
    };
    let credentials = Credentials {
        makerworld: None,
        thingiverse: token(THINGIVERSE_TOKEN_SETTING)?,
    };
    if sites.contains(&Site::Thingiverse) && credentials.thingiverse.is_none() {
        return Err(SettingsError::Library(THINGIVERSE_TOKEN_SETTING));
    }
    let makerworld = account::MakerWorldAccount::configure(get)?;
    let client = Client::new(credentials).map_err(|_| SettingsError::Library(SITES_SETTING))?;
    Ok(Some(Arc::new(LibraryService::new(
        client, sites, makerworld,
    ))))
}

impl LibraryService {
    pub(crate) fn new(
        client: Client,
        sites: BTreeSet<Site>,
        makerworld: account::MakerWorldAccount,
    ) -> Self {
        client.set_token(Site::MakerWorld, makerworld.initial_token());
        Self {
            client,
            sites,
            gate: Semaphore::new(CONCURRENT_REQUESTS),
            makerworld,
        }
    }

    /// Which sites can search and download. Reports token presence only.
    pub fn report(&self) -> Value {
        let sites: Vec<_> = self
            .sites
            .iter()
            .map(|site| {
                json!({
                    "site": site.id(),
                    "search": true,
                    "download": self.downloads_available(*site),
                })
            })
            .collect();
        let mut report = json!({"configured": true, "sites": sites});
        if self.sites.contains(&Site::MakerWorld) {
            report["makerworld_sign_in"] = self.makerworld.report();
        }
        report
    }

    fn downloads_available(&self, site: Site) -> bool {
        site != Site::MakerWorld || self.makerworld.downloads_available()
    }

    pub async fn dispatch(
        &self,
        request: LibraryRequest,
        workspace: &Arc<Workspace>,
        max_file_bytes: u64,
    ) -> Result<Value, ToolError> {
        let site = request.site();
        if !self.sites.contains(&site) {
            let enabled: Vec<_> = self.sites.iter().map(|site| site.id()).collect();
            return Err(ToolError::Validation(format!(
                "{site} is not enabled on this server; enabled sites: {}",
                enabled.join(", ")
            )));
        }
        let _permit = self
            .gate
            .acquire()
            .await
            .map_err(|_| ToolError::Validation("the model library is shutting down".into()))?;
        match request {
            LibraryRequest::Search(params) => {
                let page = self
                    .client
                    .search(
                        site,
                        &SearchQuery {
                            text: params.query,
                            page: params.page,
                            limit: params.limit,
                            sort: params.sort,
                        },
                    )
                    .await?;
                Ok(serde_json::to_value(page)?)
            }
            LibraryRequest::Get(params) => {
                let detail = self.client.model(site, &params.model).await?;
                let mut value = serde_json::to_value(detail)?;
                value["downloads_available"] = json!(self.downloads_available(site));
                Ok(value)
            }
            LibraryRequest::Download(params) => {
                self.download(params, workspace, max_file_bytes).await
            }
            LibraryRequest::SignIn(params) => {
                if site != Site::MakerWorld {
                    return Err(ToolError::Validation(format!(
                        "{site} does not use sign-in"
                    )));
                }
                self.makerworld
                    .sign_in(&self.client, params.code.as_deref())
                    .await
            }
        }
    }

    async fn download(
        &self,
        params: DownloadParams,
        workspace: &Arc<Workspace>,
        max_file_bytes: u64,
    ) -> Result<Value, ToolError> {
        let site = params.site;
        let model_id = model_library::model_id(site, &params.model)?;
        let mut seen = BTreeSet::new();
        let file_ids: Vec<String> = params
            .files
            .into_iter()
            .filter(|id| seen.insert(id.clone()))
            .collect();
        if file_ids.is_empty() || file_ids.len() > MAXIMUM_FILES {
            return Err(ToolError::Validation(format!(
                "files must list 1 to {MAXIMUM_FILES} file_id values from get"
            )));
        }
        let directory = params
            .directory
            .unwrap_or_else(|| format!("library/{}-{model_id}", site.id()));
        // Components below a missing directory are only checked when created,
        // after any sign-in, so their limits are enforced here.
        if directory.split('/').any(|component| {
            component.len() > MAXIMUM_COMPONENT_BYTES || component.chars().any(char::is_control)
        }) {
            return Err(ToolError::Validation(format!(
                "directory components must be at most {MAXIMUM_COMPONENT_BYTES} bytes without control characters"
            )));
        }
        let resolve = |name: &str| {
            crate::projects::resolve(
                workspace,
                &params.project_id,
                &format!("{directory}/{name}"),
            )
        };
        let manifest_path = resolve(MANIFEST)?;
        if !self.downloads_available(site) {
            return Err(LibraryError::CredentialsRequired { site }.into());
        }

        let detail = self.client.model(site, &model_id).await?;
        // A site file named like the record is renamed rather than replacing it.
        let mut names = BTreeSet::from([MANIFEST.to_owned()]);
        let mut plan = Vec::with_capacity(file_ids.len());
        for file_id in &file_ids {
            let file = detail
                .files
                .iter()
                .find(|file| &file.file_id == file_id)
                .ok_or_else(|| {
                    ToolError::Validation(format!(
                        "{site} model {model_id} lists no file {file_id}; use the file_id values from get"
                    ))
                })?;
            let name = unique_name(local_name(&file.name)?, file_id, &mut names);
            plan.push((file, resolve(&name)?));
        }
        // Only an absent destination is free. Any other answer, such as a
        // path through an existing file, means the commit cannot succeed.
        for path in plan
            .iter()
            .map(|(_, path)| path)
            .chain(std::iter::once(&manifest_path))
        {
            match workspace.stat_artifact(path) {
                Err(printable_workspace::WsError::NotFound(_)) => {}
                Err(error) => return Err(error.into()),
                Ok(_) if params.overwrite => {}
                Ok(_) => {
                    return Err(ToolError::Validation(format!(
                        "{path} already exists; set overwrite or choose another directory"
                    )));
                }
            }
        }

        // Sign in only once the request is known to be valid, so a mistake
        // never costs the user a sign-in code.
        let mut generation = if site == Site::MakerWorld {
            self.makerworld.ensure(&self.client).await?
        } else {
            0
        };

        // Receive every file before committing any, so a failed transfer
        // leaves the project unchanged.
        let mut staged = Vec::with_capacity(plan.len());
        for (file, path) in plan {
            let stream = match self
                .client
                .open_file(&detail, &file.file_id, max_file_bytes)
                .await
            {
                // Bambu Lab rejected the token, or a concurrent request
                // discarded it: sign in again once and retry.
                Err(
                    LibraryError::Authentication {
                        site: Site::MakerWorld,
                    }
                    | LibraryError::CredentialsRequired {
                        site: Site::MakerWorld,
                    },
                ) if self.makerworld.can_sign_in() => {
                    self.makerworld.invalidate(&self.client, generation).await;
                    generation = self.makerworld.ensure(&self.client).await?;
                    self.client
                        .open_file(&detail, &file.file_id, max_file_bytes)
                        .await?
                }
                result => result?,
            };
            staged.push((
                file,
                path,
                receive(workspace, stream, max_file_bytes).await?,
            ));
        }

        let mut saved = Vec::with_capacity(staged.len());
        for (file, path, received) in staged {
            let workspace_for_commit = Arc::clone(workspace);
            let overwrite = params.overwrite;
            let attempted = path.clone();
            let committed = tokio::task::spawn_blocking(move || {
                workspace_for_commit
                    .commit_generated_artifact_bounded(
                        &path,
                        &received.path,
                        overwrite,
                        max_file_bytes,
                    )
                    .map(|artifact| (artifact, received.sha256))
            })
            .await
            .map_err(|error| ToolError::Io(std::io::Error::other(error)))?;
            match committed {
                Ok((artifact, sha256)) => saved.push(SavedFile {
                    file_id: file.file_id.clone(),
                    name: file.name.clone(),
                    artifact,
                    sha256,
                }),
                Err(error) => return Err(incomplete(&saved, &attempted, error)),
            }
        }

        let manifest = json!({
            "format_version": 1,
            "site": site,
            "model_id": detail.model_id,
            "url": detail.url,
            "title": detail.title,
            "designer": detail.designer,
            "license": detail.license,
            "retrieved_unix_ms": unix_ms(),
            "files": saved.iter().map(|file| json!({
                "file_id": file.file_id,
                "name": file.name,
                "path": file.artifact.path,
                "size_bytes": file.artifact.size_bytes,
                "sha256": file.sha256,
            })).collect::<Vec<_>>(),
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        let manifest_artifact = workspace
            .write_artifact(&manifest_path, &manifest_bytes, params.overwrite)
            .map_err(|error| incomplete(&saved, &manifest_path, error))?;
        Ok(download_result(&detail, &saved, manifest_artifact))
    }
}

struct SavedFile {
    file_id: String,
    name: String,
    artifact: ArtifactMeta,
    sha256: String,
}

fn download_result(detail: &ModelDetail, saved: &[SavedFile], manifest: ArtifactMeta) -> Value {
    json!({
        "site": detail.site,
        "model_id": detail.model_id,
        "title": detail.title,
        "url": detail.url,
        "designer": detail.designer,
        "license": detail.license,
        "files": saved.iter().map(|file| json!({
            "file_id": file.file_id,
            "name": file.name,
            "artifact": file.artifact,
            "sha256": file.sha256,
        })).collect::<Vec<_>>(),
        "manifest": manifest,
    })
}

/// A failure after some files were committed names them, so the caller can
/// inspect or remove them rather than assume nothing changed. A failed commit
/// can still have placed its file before a later step failed, so its path is
/// named as possibly written unless the destination already existed.
fn incomplete(
    saved: &[SavedFile],
    attempted: &str,
    error: printable_workspace::WsError,
) -> ToolError {
    let mut changes = Vec::new();
    if !saved.is_empty() {
        let paths: Vec<_> = saved
            .iter()
            .map(|file| file.artifact.path.as_str())
            .collect();
        changes.push(format!("saved {}", paths.join(", ")));
    }
    if !matches!(error, printable_workspace::WsError::AlreadyExists(_)) {
        changes.push(format!("{attempted} may have been written"));
    }
    if changes.is_empty() {
        return error.into();
    }
    ToolError::LibraryIncomplete(format!("{} before failing: {error}", changes.join("; ")))
}

struct Received {
    path: std::path::PathBuf,
    sha256: String,
    /// Keeps the received bytes, and their storage charge, until commit.
    _scratch: Arc<ManagedScratch>,
}

/// Bytes written to scratch space per storage admission.
const WRITE_BLOCK_BYTES: usize = 1024 * 1024;

/// Stream one file into managed scratch space and hash it. A declared size is
/// reserved up front; otherwise each block is admitted against the workspace
/// budget as it is written, so the charge follows what actually arrives.
async fn receive(
    workspace: &Arc<Workspace>,
    mut stream: FileStream,
    max_file_bytes: u64,
) -> Result<Received, ToolError> {
    let declared = stream
        .declared_size()
        .filter(|size| *size <= max_file_bytes);
    let staging = Arc::clone(workspace);
    let (scratch, file) = tokio::task::spawn_blocking(move || {
        let scratch = staging.scratch(declared.unwrap_or(0), "library_download")?;
        let file = tempfile::NamedTempFile::new_in(scratch.path())?;
        Ok::<_, ToolError>((Arc::new(scratch), file))
    })
    .await
    .map_err(|error| ToolError::Io(std::io::Error::other(error)))??;
    let mut writer = Some(file.reopen()?);
    let mut digest = Sha256::new();
    let mut block = Vec::with_capacity(WRITE_BLOCK_BYTES);
    let mut finished = false;
    while !finished {
        match stream.chunk().await? {
            Some(chunk) => {
                digest.update(&chunk);
                block.extend_from_slice(&chunk);
                if block.len() < WRITE_BLOCK_BYTES {
                    continue;
                }
            }
            None => finished = true,
        }
        let owner = Arc::clone(&scratch);
        let mut file = writer.take().expect("writer returned after each block");
        let pending = std::mem::replace(&mut block, Vec::with_capacity(WRITE_BLOCK_BYTES));
        let admit = declared.is_none();
        writer = Some(
            tokio::task::spawn_blocking(move || {
                let _admission = if admit && !pending.is_empty() {
                    owner.admit_growth(pending.len() as u64)?
                } else {
                    None
                };
                file.write_all(&pending)?;
                if finished {
                    file.sync_all()?;
                }
                Ok::<_, ToolError>(file)
            })
            .await
            .map_err(|error| ToolError::Io(std::io::Error::other(error)))??,
        );
    }
    drop(writer);
    let sha256 = digest
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    let path = file
        .into_temp_path()
        .keep()
        .map_err(|error| ToolError::Io(error.error))?;
    Ok(Received {
        path,
        sha256,
        _scratch: scratch,
    })
}

/// A safe local file name for a site's file name: the final path component,
/// a lowercase allowed extension, and a stem of letters, digits, `-`, `_`
/// and `.` that cannot start with a dot.
fn local_name(name: &str) -> Result<String, ToolError> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let (stem, extension) = base.rsplit_once('.').ok_or_else(|| {
        ToolError::Validation(format!("{name} has no file extension and cannot be saved"))
    })?;
    let suffix = format!(".{}", extension.to_ascii_lowercase());
    if !ALLOWED_ARTIFACT_SUFFIXES.contains(&suffix.as_str()) {
        return Err(ToolError::Validation(format!(
            "{name} has unsupported type {suffix}; allowed: {}",
            ALLOWED_ARTIFACT_SUFFIXES.join(", ")
        )));
    }
    let mut clean = String::new();
    for character in stem.chars() {
        let character = if character.is_ascii_alphanumeric() || matches!(character, '-' | '.') {
            character
        } else {
            '_'
        };
        if character == '_' && clean.ends_with('_') {
            continue;
        }
        clean.push(character);
    }
    let clean: String = clean
        .trim_matches(['.', '_', '-'])
        .chars()
        .take(MAXIMUM_STEM_CHARACTERS)
        .collect();
    let stem = if clean.is_empty() { "file" } else { &clean };
    Ok(format!("{stem}{suffix}"))
}

/// Keep names unique within one download by adding the file ID on collision.
fn unique_name(name: String, file_id: &str, taken: &mut BTreeSet<String>) -> String {
    if taken.insert(name.clone()) {
        return name;
    }
    let (stem, suffix) = name.rsplit_once('.').unwrap_or((name.as_str(), ""));
    let id: String = file_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let mut candidate = format!("{stem}-{id}.{suffix}");
    let mut counter = 2;
    while !taken.insert(candidate.clone()) {
        candidate = format!("{stem}-{id}-{counter}.{suffix}");
        counter += 1;
    }
    candidate
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

mod account;
#[cfg(test)]
mod tests;
