//! Explicit part families, supported variants and byte-identified file sets.
//! Records retain selections independently of the rebuildable search index.

use super::{get as project, resolve, revisions::digest};
use crate::error::ToolError;
use printable_workspace::{Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Attribute {
    Text(String),
    Number(f64),
    Boolean(bool),
}
impl Attribute {
    pub fn display(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Number(n) => n.to_string(),
            Self::Boolean(b) => b.to_string(),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttributeType {
    Text,
    Number,
    Boolean,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Axis {
    pub id: String,
    pub label: String,
    pub kind: AttributeType,
    #[serde(default)]
    pub unit: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub path: String,
    pub sha256: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Representation {
    pub id: String,
    pub name: String,
    pub files: Vec<File>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub id: String,
    pub name: String,
    pub attributes: BTreeMap<String, Attribute>,
    pub representations: Vec<Representation>,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub preview: Option<File>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Related {
    pub family_id: String,
    pub description: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Family {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub description: String,
    pub axes: Vec<Axis>,
    pub variants: Vec<Variant>,
    #[serde(default)]
    pub related: Vec<Related>,
    #[serde(default)]
    pub preview: Option<File>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub format_version: u32,
    pub project_id: String,
    pub family: Family,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Response {
    pub revision: String,
    pub record: Record,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PutParams {
    pub project_id: String,
    /// Null for a new family; otherwise the exact revision returned by family.
    pub expected_revision: Option<String>,
    pub family: Family,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    pub project_id: String,
    pub family_id: String,
    #[serde(default)]
    pub revision: Option<String>,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    pub project_id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
fn default_limit() -> usize {
    24
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectParams {
    pub project_id: String,
    pub family_id: String,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub attributes: BTreeMap<String, Attribute>,
    #[serde(default)]
    pub variant_id: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Summary {
    pub id: String,
    pub name: String,
    pub category: String,
    pub description: String,
    pub revision: String,
    pub variants: usize,
    pub preview: Option<File>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Counts {
    pub families: usize,
    pub variants: usize,
    pub classified_files: usize,
    pub distinct_classified_contents: usize,
    pub repeated_contents: usize,
    pub workspace_files: usize,
    pub unclassified_files: usize,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct Listing {
    pub project_id: String,
    pub families: Vec<Summary>,
    pub total: usize,
    pub next_offset: Option<usize>,
    pub counts: Counts,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct Choice {
    pub axis: Axis,
    pub values: Vec<Attribute>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VariantSummary {
    pub id: String,
    pub name: String,
    pub attributes: BTreeMap<String, Attribute>,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct Selection {
    pub project_id: String,
    pub family_id: String,
    pub family_name: String,
    pub revision: String,
    pub options: Vec<Choice>,
    pub variants: Vec<VariantSummary>,
    pub total: usize,
    pub next_offset: Option<usize>,
    pub selected: Option<Variant>,
    pub family_preview: Option<File>,
    pub related: Vec<Related>,
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::Validation(message.into())
}
fn id(value: &str) -> Result<(), ToolError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_'))
    {
        return Err(invalid(
            "family, variant, representation and attribute IDs require lowercase letters, digits, underscores or hyphens",
        ));
    }
    Ok(())
}
fn label(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(invalid(
            "names and labels must be nonempty text without control characters",
        ));
    }
    Ok(())
}
pub fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn root(project_id: &str) -> String {
    format!(".printable/families/{project_id}")
}
fn head(project_id: &str, family_id: &str) -> String {
    format!("{}/{family_id}/head.json", root(project_id))
}
fn revision_path(project_id: &str, family_id: &str, revision: &str) -> String {
    format!("{}/{family_id}/{revision}.json", root(project_id))
}

pub(crate) fn reader_hash(reader: &mut dyn std::io::Read) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
pub fn file_hash(workspace: &Workspace, path: &str) -> Result<String, ToolError> {
    Ok(workspace.with_artifact_reader(path, reader_hash)?.1)
}

fn files(family: &Family) -> impl Iterator<Item = &File> {
    family
        .preview
        .iter()
        .chain(family.variants.iter().flat_map(|v| {
            v.preview
                .iter()
                .chain(v.representations.iter().flat_map(|r| r.files.iter()))
        }))
}
fn validate(family: &Family) -> Result<(), ToolError> {
    id(&family.id)?;
    label(&family.name)?;
    for image in family
        .preview
        .iter()
        .chain(family.variants.iter().filter_map(|v| v.preview.as_ref()))
    {
        if ![".png", ".jpg", ".jpeg", ".webp"]
            .iter()
            .any(|suffix| image.path.ends_with(suffix))
        {
            return Err(invalid("family previews must be PNG, JPEG or WebP files"));
        }
    }
    let mut axes = BTreeSet::new();
    for axis in &family.axes {
        id(&axis.id)?;
        label(&axis.label)?;
        if !axes.insert(&axis.id) {
            return Err(invalid("attribute IDs must be unique"));
        }
        if let Some(unit) = &axis.unit {
            label(unit)?;
            if axis.kind != AttributeType::Number {
                return Err(invalid("only numeric attributes may declare a unit"));
            }
        }
    }
    if family.variants.is_empty() {
        return Err(invalid("a family must contain supported variants"));
    }
    let mut ids = BTreeSet::new();
    let mut combinations = BTreeSet::new();
    let mut paths = BTreeMap::new();
    for variant in &family.variants {
        id(&variant.id)?;
        label(&variant.name)?;
        if !ids.insert(&variant.id) {
            return Err(invalid("variant IDs must be unique"));
        }
        if variant.attributes.keys().collect::<BTreeSet<_>>() != axes {
            return Err(invalid(
                "every variant must specify exactly the declared attributes",
            ));
        }
        for axis in &family.axes {
            let value = &variant.attributes[&axis.id];
            let valid = match (axis.kind, value) {
                (AttributeType::Text, Attribute::Text(s)) => {
                    !s.trim().is_empty() && !s.chars().any(char::is_control)
                }
                (AttributeType::Number, Attribute::Number(n)) => n.is_finite(),
                (AttributeType::Boolean, Attribute::Boolean(_)) => true,
                _ => false,
            };
            if !valid {
                return Err(invalid(
                    "variant attribute value does not match its declared type",
                ));
            }
        }
        // Display canonicalization also treats integer and decimal spellings,
        // including negative zero, as the same numeric selection.
        let combination: Vec<_> = variant
            .attributes
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    match v {
                        Attribute::Number(n) if *n == 0.0 => "0".into(),
                        _ => v.display(),
                    },
                )
            })
            .collect();
        if !combinations.insert(combination) {
            return Err(invalid("supported attribute combinations must be unique"));
        }
        if variant.representations.is_empty() {
            return Err(invalid("each variant needs at least one file set"));
        }
        let mut representations = BTreeSet::new();
        for representation in &variant.representations {
            id(&representation.id)?;
            label(&representation.name)?;
            if !representations.insert(&representation.id) || representation.files.is_empty() {
                return Err(invalid("file sets need unique IDs and nonempty file lists"));
            }
            let mut members = BTreeSet::new();
            if representation
                .files
                .iter()
                .any(|f| !members.insert(&f.path))
            {
                return Err(invalid("a file set cannot repeat the same file"));
            }
        }
    }
    for file in files(family) {
        if !is_digest(&file.sha256) {
            return Err(invalid(
                "file identities require a lowercase SHA-256 digest",
            ));
        }
        if let Some(old) = paths.insert(&file.path, &file.sha256)
            && old != &file.sha256
        {
            return Err(invalid(
                "one path cannot identify different source contents",
            ));
        }
    }
    for related in &family.related {
        id(&related.family_id)?;
        label(&related.description)?;
        if related.family_id == family.id {
            return Err(invalid("a related family must be a different family"));
        }
    }
    Ok(())
}
fn current(
    workspace: &Workspace,
    project_id: &str,
    family_id: &str,
) -> Result<Option<String>, ToolError> {
    match workspace.read_generated_bytes(&head(project_id, family_id)) {
        Ok((_, bytes)) => {
            let revision: String = serde_json::from_slice(&bytes)?;
            if !is_digest(&revision) {
                return Err(invalid("invalid retained family revision"));
            }
            Ok(Some(revision))
        }
        Err(WsError::NotFound(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn get(workspace: &Workspace, params: &GetParams) -> Result<Response, ToolError> {
    project(workspace, &params.project_id)?;
    id(&params.family_id)?;
    let revision = match &params.revision {
        Some(r) => r.clone(),
        None => current(workspace, &params.project_id, &params.family_id)?
            .ok_or_else(|| invalid("part family does not exist"))?,
    };
    if !is_digest(&revision) {
        return Err(invalid("invalid family revision digest"));
    }
    let (_, bytes) = workspace.read_generated_bytes(&revision_path(
        &params.project_id,
        &params.family_id,
        &revision,
    ))?;
    if digest(&bytes) != revision {
        return Err(invalid(
            "retained family definition failed its integrity check",
        ));
    }
    let record: Record = serde_json::from_slice(&bytes)?;
    if record.format_version != 1
        || record.project_id != params.project_id
        || record.family.id != params.family_id
    {
        return Err(invalid("retained family identity is inconsistent"));
    }
    validate(&record.family)?;
    // Validate paths on reads as well as writes; reserved records are not a
    // substitute for the workspace's capability-rooted I/O checks.
    for file in files(&record.family) {
        resolve(workspace, &params.project_id, &file.path)?;
    }
    Ok(Response { revision, record })
}
pub fn put(workspace: &Workspace, params: PutParams) -> Result<Response, ToolError> {
    project(workspace, &params.project_id)?;
    validate(&params.family)?;
    let _lock = workspace.try_lock_reserved(&format!(
        "{}/{}/edit.lock",
        root(&params.project_id),
        params.family.id
    ))?;
    if current(workspace, &params.project_id, &params.family.id)? != params.expected_revision {
        return Err(invalid("family revision changed; reopen it before editing"));
    }
    let mut checked = BTreeSet::new();
    for file in files(&params.family) {
        if checked.insert(&file.path) {
            let path = resolve(workspace, &params.project_id, &file.path)?;
            if file_hash(workspace, &path)? != file.sha256 {
                return Err(invalid(format!("source content changed: {}", file.path)));
            }
        }
    }
    let record = Record {
        format_version: 1,
        project_id: params.project_id.clone(),
        family: params.family,
    };
    let bytes = serde_json::to_vec(&record)?;
    let revision = digest(&bytes);
    let path = revision_path(&params.project_id, &record.family.id, &revision);
    match workspace.write_reserved_artifact(&path, &bytes, false) {
        Ok(_) => (),
        Err(WsError::AlreadyExists(_)) => {
            if workspace.read_generated_bytes(&path)?.1 != bytes {
                return Err(invalid("retained family revision collision"));
            }
        }
        Err(e) => return Err(e.into()),
    }
    workspace.write_reserved_artifact(
        &head(&params.project_id, &record.family.id),
        &serde_json::to_vec(&revision)?,
        true,
    )?;
    Ok(Response { revision, record })
}
pub fn records(workspace: &Workspace, project_id: &str) -> Result<Vec<Response>, ToolError> {
    project(workspace, project_id)?;
    let listing = match workspace.list_artifacts(&root(project_id), usize::MAX) {
        Ok(v) => v,
        Err(WsError::NotFound(_)) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    listing
        .iter()
        .filter(|f| f.path.ends_with("/head.json"))
        .map(|f| {
            let family_id = f
                .path
                .rsplit('/')
                .nth(1)
                .ok_or_else(|| invalid("invalid family head path"))?;
            get(
                workspace,
                &GetParams {
                    project_id: project_id.into(),
                    family_id: family_id.into(),
                    revision: None,
                },
            )
        })
        .collect()
}
pub fn summaries(records: &[Response]) -> Vec<Summary> {
    records
        .iter()
        .map(|r| Summary {
            id: r.record.family.id.clone(),
            name: r.record.family.name.clone(),
            category: r.record.family.category.clone(),
            description: r.record.family.description.clone(),
            revision: r.revision.clone(),
            variants: r.record.family.variants.len(),
            preview: r.record.family.preview.clone(),
        })
        .collect()
}
pub fn search_text(records: &[Response]) -> String {
    records
        .iter()
        .map(|r| {
            let f = &r.record.family;
            let mut words = vec![f.name.clone(), f.category.clone(), f.description.clone()];
            words.extend(
                f.axes
                    .iter()
                    .flat_map(|a| [a.label.clone(), a.unit.clone().unwrap_or_default()]),
            );
            for v in &f.variants {
                words.push(v.name.clone());
                words.extend(v.attributes.values().map(Attribute::display));
            }
            words.join(" ")
        })
        .collect::<Vec<_>>()
        .join(" ")
}
pub(crate) fn classified_paths(records: &[Response]) -> BTreeSet<String> {
    records
        .iter()
        .flat_map(|record| files(&record.record.family))
        .map(|file| file.path.clone())
        .collect()
}

pub fn counts(records: &[Response], workspace_paths: &BTreeSet<String>) -> Counts {
    let mut paths = BTreeMap::new();
    let mut variants = 0;
    for record in records {
        variants += record.record.family.variants.len();
        for file in files(&record.record.family) {
            paths.insert(file.path.clone(), file.sha256.clone());
        }
    }
    let distinct = paths.values().collect::<BTreeSet<_>>().len();
    Counts {
        families: records.len(),
        variants,
        classified_files: paths.len(),
        distinct_classified_contents: distinct,
        repeated_contents: paths.len() - distinct,
        workspace_files: workspace_paths.len(),
        unclassified_files: workspace_paths
            .difference(&paths.keys().cloned().collect())
            .count(),
    }
}
pub fn list(workspace: &Workspace, params: &ListParams) -> Result<Listing, ToolError> {
    if params.limit == 0 || params.limit > 100 {
        return Err(invalid("family pages require a limit from 1 to 100"));
    }
    let records = records(workspace, &params.project_id)?;
    let prefix = format!("projects/{}/", params.project_id);
    let workspace_paths = workspace
        .list_artifacts(prefix.trim_end_matches('/'), usize::MAX)?
        .into_iter()
        .filter_map(|f| f.path.strip_prefix(&prefix).map(str::to_owned))
        .collect();
    let counts = counts(&records, &workspace_paths);
    let words: Vec<_> = params
        .text
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let matched: Vec<_> = records
        .into_iter()
        .filter(|r| {
            let text = search_text(std::slice::from_ref(r)).to_lowercase();
            words.iter().all(|w| text.contains(w))
        })
        .collect();
    let total = matched.len();
    let families = summaries(&matched)
        .into_iter()
        .skip(params.offset)
        .take(params.limit)
        .collect::<Vec<_>>();
    Ok(Listing {
        project_id: params.project_id.clone(),
        next_offset: (params.offset.saturating_add(families.len()) < total)
            .then_some(params.offset.saturating_add(families.len())),
        families,
        total,
        counts,
    })
}
pub fn select(workspace: &Workspace, params: &SelectParams) -> Result<Selection, ToolError> {
    if params.limit == 0 || params.limit > 100 {
        return Err(invalid("variant pages require a limit from 1 to 100"));
    }
    let response = get(
        workspace,
        &GetParams {
            project_id: params.project_id.clone(),
            family_id: params.family_id.clone(),
            revision: params.revision.clone(),
        },
    )?;
    let family = &response.record.family;
    if params
        .attributes
        .keys()
        .any(|key| !family.axes.iter().any(|a| &a.id == key))
    {
        return Err(invalid("selection contains an unknown attribute"));
    }
    let matches = |variant: &&Variant, except: Option<&str>| {
        params.attributes.iter().all(|(key, value)| {
            Some(key.as_str()) == except || variant.attributes.get(key) == Some(value)
        })
    };
    let filtered: Vec<_> = family
        .variants
        .iter()
        .filter(|v| matches(v, None))
        .filter(|v| params.variant_id.as_ref().is_none_or(|id| &v.id == id))
        .collect();
    let selected = if filtered.len() == 1 {
        Some(filtered[0].clone())
    } else {
        None
    };
    if let Some(v) = &selected {
        for file in v
            .representations
            .iter()
            .flat_map(|r| &r.files)
            .chain(v.preview.iter())
        {
            if file_hash(
                workspace,
                &resolve(workspace, &params.project_id, &file.path)?,
            )? != file.sha256
            {
                return Err(invalid(format!("source content changed: {}", file.path)));
            }
        }
    }
    let options = family
        .axes
        .iter()
        .map(|axis| {
            let mut values = Vec::new();
            for v in family
                .variants
                .iter()
                .filter(|v| matches(v, Some(&axis.id)))
            {
                let value = v.attributes[&axis.id].clone();
                if !values.contains(&value) {
                    values.push(value);
                }
            }
            Choice {
                axis: axis.clone(),
                values,
            }
        })
        .collect();
    let total = filtered.len();
    let variants = filtered
        .into_iter()
        .skip(params.offset)
        .take(params.limit)
        .map(|v| VariantSummary {
            id: v.id.clone(),
            name: v.name.clone(),
            attributes: v.attributes.clone(),
        })
        .collect::<Vec<_>>();
    Ok(Selection {
        project_id: params.project_id.clone(),
        family_id: params.family_id.clone(),
        family_name: family.name.clone(),
        revision: response.revision,
        options,
        variants,
        total,
        next_offset: (params.offset.saturating_add(params.limit) < total)
            .then_some(params.offset.saturating_add(params.limit)),
        selected,
        family_preview: family.preview.clone(),
        related: family.related.clone(),
    })
}

#[cfg(test)]
pub(crate) mod tests;
