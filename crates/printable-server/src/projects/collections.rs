//! Named groups of projects, like folders in the library.
//!
//! One server-owned file holds every collection and every membership, so a
//! project belongs to at most one collection by construction, and a change
//! that moves several projects or deletes a collection is a single atomic
//! replacement. Changes hold a cross-process lock; readers take no lock and
//! see either the previous or the next file. Collections are metadata only:
//! no change here moves, renames or deletes a project or its files.

use std::collections::{BTreeMap, BTreeSet};

use printable_workspace::{Workspace, WsError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;

const INDEX_PATH: &str = ".printable/collections/index.json";
const LOCK_PATH: &str = ".printable/collections/update.lock";
const FORMAT_VERSION: u32 = 1;
const MAX_COLLECTIONS: usize = 1000;
const MAX_PROJECTS_PER_REQUEST: usize = 500;
const MAX_ID_CHARS: usize = 128;
const MAX_NAME_CHARS: usize = 256;
const MAX_DESCRIPTION_CHARS: usize = 4096;
/// Projects named on a collection summary, enough for a card.
const SAMPLE_PROJECTS: usize = 4;
/// Conflicting projects named in one refusal.
const MAX_LISTED_CONFLICTS: usize = 20;

/// The stored collections and memberships.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    format_version: u32,
    /// Increases with every accepted change.
    revision: u64,
    collections: BTreeMap<String, Collection>,
    /// Project ID to the ID of the one collection it belongs to.
    memberships: BTreeMap<String, String>,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            revision: 0,
            collections: BTreeMap::new(),
            memberships: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Collection {
    pub name: String,
    pub description: String,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

/// The collection a project belongs to, as project responses report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CollectionRef {
    pub collection_id: String,
    pub name: String,
}

impl Index {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn collection(&self, id: &str) -> Option<&Collection> {
        self.collections.get(id)
    }

    /// Every collection, by ID.
    pub fn collections(&self) -> &BTreeMap<String, Collection> {
        &self.collections
    }

    /// The collection a project belongs to.
    pub fn membership(&self, project_id: &str) -> Option<CollectionRef> {
        let id = self.memberships.get(project_id)?;
        let collection = self.collections.get(id)?;
        Some(CollectionRef {
            collection_id: id.clone(),
            name: collection.name.clone(),
        })
    }

    /// The projects in a collection, by project ID.
    pub fn members(&self, id: &str) -> Vec<String> {
        self.memberships
            .iter()
            .filter(|(_, collection)| collection.as_str() == id)
            .map(|(project, _)| project.clone())
            .collect()
    }

    /// Project ID to collection ID for every grouped project.
    pub fn memberships(&self) -> &BTreeMap<String, String> {
        &self.memberships
    }

    /// Every project that belongs to some collection.
    pub fn grouped(&self) -> Vec<String> {
        self.memberships.keys().cloned().collect()
    }

    /// Project counts by collection ID; empty collections are absent.
    pub fn counts(&self) -> BTreeMap<&str, usize> {
        let mut counts = BTreeMap::new();
        for collection in self.memberships.values() {
            *counts.entry(collection.as_str()).or_default() += 1;
        }
        counts
    }

    fn valid(&self) -> bool {
        self.format_version == FORMAT_VERSION
            && self.collections.len() <= MAX_COLLECTIONS
            && self
                .collections
                .iter()
                .all(|(id, collection)| check_id(id).is_ok() && check_text(collection).is_ok())
            && self.memberships.iter().all(|(project, collection)| {
                super::validate_id(project).is_ok() && self.collections.contains_key(collection)
            })
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::Validation(message.into())
}

fn check_id(id: &str) -> Result<(), ToolError> {
    if id.is_empty()
        || id.len() > MAX_ID_CHARS
        || !id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-'))
    {
        return Err(invalid(format!(
            "collection_id must be 1 to {MAX_ID_CHARS} lowercase letters, digits, underscores, or hyphens"
        )));
    }
    Ok(())
}

fn check_name(name: &str) -> Result<(), ToolError> {
    if name.trim().is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(invalid(format!(
            "a collection name must be 1 to {MAX_NAME_CHARS} characters without control characters"
        )));
    }
    Ok(())
}

fn check_description(description: &str) -> Result<(), ToolError> {
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(invalid(format!(
            "a collection description can have at most {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    Ok(())
}

fn check_text(collection: &Collection) -> Result<(), ToolError> {
    check_name(&collection.name)?;
    check_description(&collection.description)
}

/// Reads of a file that a change replaced while it was being read.
const READ_ATTEMPTS: usize = 3;

/// Read the collections. A workspace without the file has none.
///
/// Each change replaces the file, which can make a concurrent read report
/// that it changed while reading; such a read is tried again a few times.
pub fn read(workspace: &Workspace) -> Result<Index, ToolError> {
    let mut attempt = 1;
    let bytes = loop {
        match workspace.read_generated_bytes(INDEX_PATH) {
            Ok((_, bytes)) => break bytes,
            Err(WsError::NotFound(_)) => return Ok(Index::default()),
            Err(WsError::ChangedWhileReading) if attempt < READ_ATTEMPTS => attempt += 1,
            Err(error) => return Err(error.into()),
        }
    };
    let index: Index = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("the collections file is unreadable or from a newer version"))?;
    if !index.valid() {
        return Err(invalid(
            "the collections file is inconsistent or from a newer version",
        ));
    }
    Ok(index)
}

/// Apply one edit under the collections lock and store the result.
///
/// The lock serialises cooperating server processes, and `expected_revision`
/// refuses an edit prepared from collections that have since changed. An
/// edit that changes nothing is not stored and keeps the revision.
fn change<T>(
    workspace: &Workspace,
    expected_revision: Option<u64>,
    edit: impl FnOnce(&mut Index) -> Result<T, ToolError>,
) -> Result<(Index, T), ToolError> {
    let _lock = workspace
        .try_lock_reserved(LOCK_PATH)
        .map_err(|error| match error {
            WsError::Io(ref io) if io.kind() == std::io::ErrorKind::WouldBlock => invalid(
                "collections are being changed by another request; read them again before retrying",
            ),
            other => other.into(),
        })?;
    let mut index = read(workspace)?;
    if let Some(expected) = expected_revision
        && expected != index.revision
    {
        return Err(invalid(format!(
            "collections changed: expected revision {expected}, found {}; read them again before changing them",
            index.revision
        )));
    }
    let before = index.clone();
    let outcome = edit(&mut index)?;
    if index != before {
        index.revision += 1;
        if !index.valid() {
            return Err(invalid(
                "the change would leave the collections inconsistent",
            ));
        }
        workspace.write_reserved_artifact(INDEX_PATH, &serde_json::to_vec(&index)?, true)?;
    }
    Ok((index, outcome))
}

fn unknown(id: &str) -> ToolError {
    invalid(format!(
        "there is no collection {id:?}; list collections to find its ID"
    ))
}

/// Distinct, valid, existing project IDs in request order.
fn existing_projects(workspace: &Workspace, ids: Vec<String>) -> Result<Vec<String>, ToolError> {
    let ids = distinct(ids)?;
    let mut missing = Vec::new();
    for id in &ids {
        super::validate_id(id)?;
        match super::get(workspace, id) {
            Ok(_) => {}
            Err(ToolError::Workspace(WsError::NotFound(_))) => missing.push(id.as_str()),
            Err(error) => return Err(error),
        }
    }
    if !missing.is_empty() {
        return Err(invalid(format!(
            "no project has these IDs: {}",
            listed(&missing)
        )));
    }
    Ok(ids)
}

fn distinct(ids: Vec<String>) -> Result<Vec<String>, ToolError> {
    if ids.is_empty() || ids.len() > MAX_PROJECTS_PER_REQUEST {
        return Err(invalid(format!(
            "project_ids must name 1 to {MAX_PROJECTS_PER_REQUEST} projects"
        )));
    }
    let mut seen = BTreeSet::new();
    Ok(ids
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect())
}

fn listed(ids: &[&str]) -> String {
    let mut text = ids
        .iter()
        .take(MAX_LISTED_CONFLICTS)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if ids.len() > MAX_LISTED_CONFLICTS {
        text.push_str(&format!(" and {} more", ids.len() - MAX_LISTED_CONFLICTS));
    }
    text
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateParams {
    /// Stable collection identifier: lowercase letters, digits, underscores, or hyphens.
    pub collection_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Existing projects to place in the new collection. A project already
    /// in another collection is refused; use add_projects with move.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_ids: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    pub collection_id: String,
    /// Member projects to return.
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
}

fn default_limit() -> usize {
    100
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateParams {
    pub collection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The `revision` read; the change is refused if collections changed since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddParams {
    pub collection_id: String,
    /// 1 to 500 existing project IDs.
    pub project_ids: Vec<String>,
    /// Move projects that are in another collection. Without it, any such
    /// project refuses the whole request.
    #[serde(default, rename = "move")]
    pub move_projects: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveParams {
    pub collection_id: String,
    /// 1 to 500 project IDs to make ungrouped.
    pub project_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteParams {
    pub collection_id: String,
    /// Make the collection's projects ungrouped. Without it, a collection
    /// that still has projects is not deleted. Projects are never deleted.
    #[serde(default)]
    pub ungroup_projects: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CollectionRequest {
    /// Create a collection, optionally with existing projects.
    Create(CreateParams),
    /// One collection and its projects.
    Get(GetParams),
    /// Every collection with its project count and a few project names.
    List(ListParams),
    /// Rename or describe a collection.
    Update(UpdateParams),
    /// Put existing projects in a collection, all or none.
    AddProjects(AddParams),
    /// Make projects in a collection ungrouped.
    RemoveProjects(RemoveParams),
    /// Delete a collection; never its projects.
    Delete(DeleteParams),
}

/// A project named in a collection response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Member {
    pub project_id: String,
    /// Null when the project's record no longer exists or cannot be read.
    pub name: Option<String>,
    pub description: Option<String>,
    /// True when the project's record no longer exists; remove_projects
    /// takes it out of the collection.
    pub missing: bool,
}

/// A project named on a collection card: its ID and name only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Sample {
    pub project_id: String,
    /// Null when the project's record no longer exists or cannot be read.
    pub name: Option<String>,
}

/// A collection as a card shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Summary {
    pub collection_id: String,
    pub name: String,
    pub description: String,
    pub project_count: usize,
    /// Up to four member projects, by project ID.
    pub sample_projects: Vec<Sample>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Moved {
    pub project_id: String,
    /// The collection the project left.
    pub from: String,
}

/// The outcome of a change.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ChangeResponse {
    /// The collections' revision after the change.
    pub revision: u64,
    pub collection_id: String,
    /// The collection after the change; null once deleted.
    pub collection: Option<Summary>,
    /// Projects that were ungrouped and joined the collection.
    pub added: Vec<String>,
    /// Projects that left another collection to join this one.
    pub moved: Vec<Moved>,
    /// Projects that became ungrouped.
    pub removed: Vec<String>,
    pub deleted: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GetResponse {
    pub revision: u64,
    pub collection: Summary,
    /// Member projects by project ID.
    pub projects: Vec<Member>,
    pub requested_limit: usize,
    pub returned: usize,
    pub limit_reached: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListResponse {
    pub revision: u64,
    /// By name, then ID.
    pub collections: Vec<Summary>,
    /// Projects in no collection.
    pub ungrouped_project_count: usize,
    pub requested_limit: usize,
    pub returned: usize,
    pub limit_reached: bool,
}

/// A member as responses name it. Reading a member's record never fails a
/// response: a change has already been stored when its response is built, so
/// an unreadable record is reported with a null name and logged.
fn member(workspace: &Workspace, project_id: &str) -> Member {
    let unnamed = |missing| Member {
        project_id: project_id.to_owned(),
        name: None,
        description: None,
        missing,
    };
    match super::get(workspace, project_id) {
        Ok(project) => Member {
            project_id: project.project_id,
            name: Some(project.name),
            description: Some(project.description),
            missing: false,
        },
        Err(ToolError::Workspace(WsError::NotFound(_))) => unnamed(true),
        Err(error) => {
            tracing::warn!(
                project_id,
                code = error.code(),
                "a collection member's record could not be read"
            );
            unnamed(false)
        }
    }
}

fn summary(
    workspace: &Workspace,
    index: &Index,
    id: &str,
    count: usize,
) -> Result<Summary, ToolError> {
    let collection = index.collections.get(id).ok_or_else(|| unknown(id))?;
    let sample_projects = index
        .members(id)
        .iter()
        .take(SAMPLE_PROJECTS)
        .map(|project| {
            let member = member(workspace, project);
            Sample {
                project_id: member.project_id,
                name: member.name,
            }
        })
        .collect();
    Ok(Summary {
        collection_id: id.to_owned(),
        name: collection.name.clone(),
        description: collection.description.clone(),
        project_count: count,
        sample_projects,
        created_unix_ms: collection.created_unix_ms,
        updated_unix_ms: collection.updated_unix_ms,
    })
}

fn current_summary(workspace: &Workspace, index: &Index, id: &str) -> Result<Summary, ToolError> {
    summary(workspace, index, id, index.members(id).len())
}

/// Place projects in a collection. Without `move_projects`, a project in
/// another collection refuses the whole edit before anything changes.
fn place(
    index: &mut Index,
    id: &str,
    projects: &[String],
    move_projects: bool,
) -> Result<(Vec<String>, Vec<Moved>), ToolError> {
    let elsewhere = in_other_collections(index, id, projects);
    if !move_projects && !elsewhere.is_empty() {
        return Err(invalid(format!(
            "these projects are in another collection; set move to move them: {}",
            listed(&elsewhere)
        )));
    }
    let mut added = Vec::new();
    let mut moved = Vec::new();
    for project in projects {
        match index.memberships.insert(project.clone(), id.to_owned()) {
            None => added.push(project.clone()),
            Some(from) if from != id => moved.push(Moved {
                project_id: project.clone(),
                from,
            }),
            Some(_) => {}
        }
    }
    Ok((added, moved))
}

fn touch(index: &mut Index, ids: impl IntoIterator<Item = String>) {
    let now = super::unix_ms();
    for id in ids {
        if let Some(collection) = index.collections.get_mut(&id) {
            collection.updated_unix_ms = now.max(collection.created_unix_ms);
        }
    }
}

fn create(workspace: &Workspace, params: CreateParams) -> Result<ChangeResponse, ToolError> {
    check_id(&params.collection_id)?;
    check_name(&params.name)?;
    check_description(&params.description)?;
    let projects = if params.project_ids.is_empty() {
        Vec::new()
    } else {
        existing_projects(workspace, params.project_ids)?
    };
    let id = params.collection_id;
    let (index, (added, moved)) = change(workspace, None, |index| {
        match index.collections.get(&id) {
            // A repeated creation returns the collection unchanged when its
            // projects are all still members. Placing them again could undo a
            // later change by someone else, so anything else is refused.
            Some(existing)
                if existing.name == params.name && existing.description == params.description =>
            {
                let absent: Vec<&str> = projects
                    .iter()
                    .filter(|project| index.memberships.get(*project) != Some(&id))
                    .map(String::as_str)
                    .collect();
                if !absent.is_empty() {
                    return Err(invalid(format!(
                        "collection {id:?} already exists without these projects; use add_projects to add them: {}",
                        listed(&absent)
                    )));
                }
                return Ok((Vec::new(), Vec::new()));
            }
            Some(_) => {
                return Err(invalid(
                    "collection_id already exists with a different name or description; use update or choose another ID",
                ));
            }
            None => {
                if index.collections.len() >= MAX_COLLECTIONS {
                    return Err(invalid(format!(
                        "a workspace can have at most {MAX_COLLECTIONS} collections"
                    )));
                }
                let now = super::unix_ms();
                index.collections.insert(
                    id.clone(),
                    Collection {
                        name: params.name,
                        description: params.description,
                        created_unix_ms: now,
                        updated_unix_ms: now,
                    },
                );
            }
        }
        let placed = place(index, &id, &projects, false)?;
        if !placed.0.is_empty() {
            touch(index, [id.clone()]);
        }
        Ok(placed)
    })?;
    Ok(ChangeResponse {
        revision: index.revision,
        collection: Some(current_summary(workspace, &index, &id)?),
        collection_id: id,
        added,
        moved,
        removed: Vec::new(),
        deleted: false,
    })
}

fn get(workspace: &Workspace, params: GetParams) -> Result<GetResponse, ToolError> {
    check_id(&params.collection_id)?;
    if params.limit == 0 {
        return Err(WsError::InvalidLimit.into());
    }
    let index = read(workspace)?;
    let collection = current_summary(workspace, &index, &params.collection_id)?;
    let projects: Vec<Member> = index
        .members(&params.collection_id)
        .iter()
        .take(params.limit)
        .map(|project| member(workspace, project))
        .collect();
    Ok(GetResponse {
        revision: index.revision,
        collection,
        requested_limit: params.limit,
        returned: projects.len(),
        limit_reached: projects.len() == params.limit,
        projects,
    })
}

fn list(workspace: &Workspace, params: ListParams) -> Result<ListResponse, ToolError> {
    if params.limit == 0 {
        return Err(WsError::InvalidLimit.into());
    }
    let index = read(workspace)?;
    let counts = index.counts();
    let mut ids: Vec<(&String, &Collection)> = index.collections.iter().collect();
    ids.sort_by(|(a_id, a), (b_id, b)| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a_id.cmp(b_id))
    });
    let collections: Vec<Summary> = ids
        .into_iter()
        .take(params.limit)
        .map(|(id, _)| {
            summary(
                workspace,
                &index,
                id,
                counts.get(id.as_str()).copied().unwrap_or(0),
            )
        })
        .collect::<Result<_, _>>()?;
    let ungrouped_project_count = super::ids(workspace)?
        .iter()
        .filter(|project| !index.memberships.contains_key(*project))
        .count();
    Ok(ListResponse {
        revision: index.revision,
        ungrouped_project_count,
        requested_limit: params.limit,
        returned: collections.len(),
        limit_reached: collections.len() == params.limit,
        collections,
    })
}

fn update(workspace: &Workspace, params: UpdateParams) -> Result<ChangeResponse, ToolError> {
    check_id(&params.collection_id)?;
    if params.name.is_none() && params.description.is_none() {
        return Err(invalid("collection update requires name or description"));
    }
    if let Some(name) = &params.name {
        check_name(name)?;
    }
    if let Some(description) = &params.description {
        check_description(description)?;
    }
    let id = params.collection_id;
    let (index, ()) = change(workspace, params.expected_revision, |index| {
        let collection = index.collections.get_mut(&id).ok_or_else(|| unknown(&id))?;
        let before = collection.clone();
        if let Some(name) = params.name {
            collection.name = name;
        }
        if let Some(description) = params.description {
            collection.description = description;
        }
        if *collection != before {
            touch(index, [id.clone()]);
        }
        Ok(())
    })?;
    Ok(ChangeResponse {
        revision: index.revision,
        collection: Some(current_summary(workspace, &index, &id)?),
        collection_id: id,
        added: Vec::new(),
        moved: Vec::new(),
        removed: Vec::new(),
        deleted: false,
    })
}

fn add_projects(workspace: &Workspace, params: AddParams) -> Result<ChangeResponse, ToolError> {
    check_id(&params.collection_id)?;
    let projects = existing_projects(workspace, params.project_ids)?;
    let id = params.collection_id;
    let (index, (added, moved)) = change(workspace, params.expected_revision, |index| {
        if !index.collections.contains_key(&id) {
            return Err(unknown(&id));
        }
        let placed = place(index, &id, &projects, params.move_projects)?;
        if !placed.0.is_empty() || !placed.1.is_empty() {
            let left = placed.1.iter().map(|moved| moved.from.clone());
            touch(index, std::iter::once(id.clone()).chain(left));
        }
        Ok(placed)
    })?;
    Ok(ChangeResponse {
        revision: index.revision,
        collection: Some(current_summary(workspace, &index, &id)?),
        collection_id: id,
        added,
        moved,
        removed: Vec::new(),
        deleted: false,
    })
}

/// Projects in `ids` that belong to a collection other than `id`.
fn in_other_collections<'a>(index: &Index, id: &str, ids: &'a [String]) -> Vec<&'a str> {
    ids.iter()
        .filter(|project| index.memberships.get(*project).is_some_and(|c| c != id))
        .map(String::as_str)
        .collect()
}

fn remove_projects(
    workspace: &Workspace,
    params: RemoveParams,
) -> Result<ChangeResponse, ToolError> {
    check_id(&params.collection_id)?;
    let projects = distinct(params.project_ids)?;
    for project in &projects {
        super::validate_id(project)?;
    }
    let id = params.collection_id;
    let (index, removed) = change(workspace, params.expected_revision, |index| {
        if !index.collections.contains_key(&id) {
            return Err(unknown(&id));
        }
        let elsewhere = in_other_collections(index, &id, &projects);
        if !elsewhere.is_empty() {
            return Err(invalid(format!(
                "these projects are in another collection, not {id:?}: {}",
                listed(&elsewhere)
            )));
        }
        // A member can always leave, even when its record no longer exists;
        // any other ID must name an existing project, so a mistyped ID
        // refuses the request.
        let mut unknown_projects = Vec::new();
        for project in projects
            .iter()
            .filter(|p| !index.memberships.contains_key(*p))
        {
            match super::get(workspace, project) {
                Ok(_) => {}
                Err(ToolError::Workspace(WsError::NotFound(_))) => {
                    unknown_projects.push(project.as_str());
                }
                Err(error) => return Err(error),
            }
        }
        if !unknown_projects.is_empty() {
            return Err(invalid(format!(
                "no project has these IDs: {}",
                listed(&unknown_projects)
            )));
        }
        let removed: Vec<String> = projects
            .iter()
            .filter(|project| index.memberships.remove(*project).is_some())
            .cloned()
            .collect();
        if !removed.is_empty() {
            touch(index, [id.clone()]);
        }
        Ok(removed)
    })?;
    Ok(ChangeResponse {
        revision: index.revision,
        collection: Some(current_summary(workspace, &index, &id)?),
        collection_id: id,
        added: Vec::new(),
        moved: Vec::new(),
        removed,
        deleted: false,
    })
}

fn delete(workspace: &Workspace, params: DeleteParams) -> Result<ChangeResponse, ToolError> {
    check_id(&params.collection_id)?;
    let id = params.collection_id;
    let (index, removed) = change(workspace, params.expected_revision, |index| {
        if !index.collections.contains_key(&id) {
            return Err(unknown(&id));
        }
        let members = index.members(&id);
        if !members.is_empty() && !params.ungroup_projects {
            return Err(invalid(format!(
                "collection {id:?} still has {} projects; set ungroup_projects to delete it and leave them ungrouped",
                members.len()
            )));
        }
        for project in &members {
            index.memberships.remove(project);
        }
        index.collections.remove(&id);
        Ok(members)
    })?;
    Ok(ChangeResponse {
        revision: index.revision,
        collection_id: id,
        collection: None,
        added: Vec::new(),
        moved: Vec::new(),
        removed,
        deleted: true,
    })
}

/// Create a project in a collection while holding the collections lock.
///
/// The collection and any other membership of `project_id` are checked under
/// the lock before `create` runs, so a refused request creates nothing and no
/// concurrent change can delete the collection in between. A project already
/// in that collection is accepted, so repeating a creation request succeeds.
pub(super) fn create_project(
    workspace: &Workspace,
    collection_id: &str,
    project_id: &str,
    create: impl FnOnce() -> Result<(super::Project, bool), ToolError>,
) -> Result<(super::Project, CollectionRef), ToolError> {
    check_id(collection_id)?;
    let created = std::cell::Cell::new(false);
    let outcome = change(workspace, None, |index| {
        if !index.collections.contains_key(collection_id) {
            return Err(unknown(collection_id));
        }
        if let Some(other) = index.memberships.get(project_id)
            && other != collection_id
        {
            return Err(invalid(format!(
                "project {project_id:?} already belongs to collection {other:?}; use the collection tool to move it"
            )));
        }
        // Repeating a creation returns the existing project. It is placed
        // only when this call wrote it: an existing project outside the
        // collection may have been moved or ungrouped since, and placing it
        // again would undo that.
        let (project, written) = create()?;
        if !written {
            if index.memberships.get(project_id).map(String::as_str) != Some(collection_id) {
                return Err(invalid(format!(
                    "project {project_id:?} already exists outside collection {collection_id:?}; use add_projects to add it"
                )));
            }
            return Ok(project);
        }
        created.set(true);
        let placed = place(
            index,
            collection_id,
            std::slice::from_ref(&project.project_id),
            false,
        )?;
        if !placed.0.is_empty() {
            touch(index, [collection_id.to_owned()]);
        }
        Ok(project)
    });
    // The project and the collections are separate files. If storing the
    // collections fails after the project was written, say so, and that
    // add_projects places it.
    let (index, project) = outcome.map_err(|error| {
        if created.get() {
            invalid(format!(
                "project {project_id:?} was created but not placed in collection {collection_id:?} ({error}); use add_projects to place it"
            ))
        } else {
            error
        }
    })?;
    let collection = index
        .membership(project_id)
        .ok_or_else(|| invalid("the project was not placed in the collection"))?;
    Ok((project, collection))
}

/// The members of a collection, refusing an unknown collection.
pub fn members_of(index: &Index, collection_id: &str) -> Result<Vec<String>, ToolError> {
    check_id(collection_id)?;
    if !index.collections.contains_key(collection_id) {
        return Err(unknown(collection_id));
    }
    Ok(index.members(collection_id))
}

pub fn dispatch(
    workspace: &Workspace,
    request: CollectionRequest,
) -> Result<serde_json::Value, ToolError> {
    Ok(match request {
        CollectionRequest::Create(params) => serde_json::to_value(create(workspace, params)?)?,
        CollectionRequest::Get(params) => serde_json::to_value(get(workspace, params)?)?,
        CollectionRequest::List(params) => serde_json::to_value(list(workspace, params)?)?,
        CollectionRequest::Update(params) => serde_json::to_value(update(workspace, params)?)?,
        CollectionRequest::AddProjects(params) => {
            serde_json::to_value(add_projects(workspace, params)?)?
        }
        CollectionRequest::RemoveProjects(params) => {
            serde_json::to_value(remove_projects(workspace, params)?)?
        }
        CollectionRequest::Delete(params) => serde_json::to_value(delete(workspace, params)?)?,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn workspace(projects: &[&str]) -> (tempfile::TempDir, Workspace) {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        for id in projects {
            super::super::create(
                &workspace,
                super::super::CreateParams {
                    project_id: (*id).into(),
                    name: format!("Project {id}"),
                    description: String::new(),
                    adopt_existing: false,
                    collection_id: None,
                },
            )
            .unwrap();
        }
        (root, workspace)
    }

    fn call(workspace: &Workspace, action: &str, params: Value) -> Result<Value, ToolError> {
        let request = serde_json::from_value(json!({"action": action, "params": params}))
            .map_err(|error| ToolError::Validation(error.to_string()))?;
        let value = dispatch(workspace, request)?;
        // Every success matches the advertised output contract.
        let schema = crate::tools::output::action_schema("collection", action).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&value), "{action}: {value}");
        let whole = Value::Object(crate::tools::output::schema("collection").as_ref().clone());
        assert!(jsonschema::validator_for(&whole).unwrap().is_valid(&value));
        Ok(value)
    }

    fn project(workspace: &Workspace, action: &str, params: Value) -> Result<Value, ToolError> {
        let request = serde_json::from_value(json!({"action": action, "params": params}))
            .map_err(|error| ToolError::Validation(error.to_string()))?;
        super::super::dispatch(workspace, request)
    }

    fn listed_ids(value: &Value) -> Vec<String> {
        value["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|project| project["project_id"].as_str().unwrap().to_owned())
            .collect()
    }

    fn stored(workspace: &Workspace) -> Option<Vec<u8>> {
        match workspace.read_generated_bytes(INDEX_PATH) {
            Ok((_, bytes)) => Some(bytes),
            Err(WsError::NotFound(_)) => None,
            Err(error) => panic!("{error}"),
        }
    }

    #[test]
    fn a_workspace_without_collections_lists_every_project_as_ungrouped() {
        let (_root, workspace) = workspace(&["clip", "desk"]);
        let listed = call(&workspace, "list", json!({})).unwrap();
        assert_eq!(listed["collections"], json!([]));
        assert_eq!(listed["ungrouped_project_count"], 2);
        assert_eq!(listed["revision"], 0);
        assert!(stored(&workspace).is_none());
        let projects = project(&workspace, "list", json!({"ungrouped": true})).unwrap();
        assert_eq!(listed_ids(&projects), ["clip", "desk"]);
        assert_eq!(projects["projects"][0]["collection"], Value::Null);
    }

    #[test]
    fn existing_projects_are_grouped_moved_and_ungrouped_without_touching_their_files() {
        let (_root, workspace) = workspace(&["mount-a", "mount-b", "mount-c", "vase"]);
        workspace
            .write_artifact("projects/mount-a/mount.stl", b"solid", false)
            .unwrap();
        let record = workspace
            .read_generated_bytes(".printable/projects/mount-a.json")
            .unwrap()
            .1;
        let created = call(
            &workspace,
            "create",
            json!({"collection_id": "makera-z1", "name": "Makera Z1 Multiboard",
                   "project_ids": ["mount-a", "mount-b"]}),
        )
        .unwrap();
        assert_eq!(created["added"], json!(["mount-a", "mount-b"]));
        assert_eq!(created["collection"]["project_count"], 2);
        assert_eq!(created["revision"], 1);
        // Repeating the same creation changes nothing.
        let again = call(
            &workspace,
            "create",
            json!({"collection_id": "makera-z1", "name": "Makera Z1 Multiboard",
                   "project_ids": ["mount-a"]}),
        )
        .unwrap();
        assert_eq!(
            (again["revision"].clone(), again["added"].clone()),
            (json!(1), json!([]))
        );
        call(
            &workspace,
            "create",
            json!({"collection_id": "decor", "name": "Decor"}),
        )
        .unwrap();

        // A project in another collection refuses the whole request.
        let before = stored(&workspace);
        let refused = call(
            &workspace,
            "add_projects",
            json!({"collection_id": "decor", "project_ids": ["vase", "mount-b"]}),
        )
        .unwrap_err();
        assert!(refused.to_string().contains("mount-b"), "{refused}");
        assert_eq!(stored(&workspace), before);
        let moved = call(
            &workspace,
            "add_projects",
            json!({"collection_id": "decor", "project_ids": ["vase", "mount-b"], "move": true}),
        )
        .unwrap();
        assert_eq!(moved["added"], json!(["vase"]));
        assert_eq!(
            moved["moved"],
            json!([{"project_id": "mount-b", "from": "makera-z1"}])
        );
        // Repeating the original creation cannot undo the move: it is
        // refused, and the move stands.
        let before = stored(&workspace);
        let repeated = call(
            &workspace,
            "create",
            json!({"collection_id": "makera-z1", "name": "Makera Z1 Multiboard",
                   "project_ids": ["mount-a", "mount-b"]}),
        )
        .unwrap_err();
        assert!(repeated.to_string().contains("add_projects"), "{repeated}");
        assert_eq!(stored(&workspace), before);
        let replay = call(
            &workspace,
            "create",
            json!({"collection_id": "makera-z1", "name": "Makera Z1 Multiboard",
                   "project_ids": ["mount-a"]}),
        )
        .unwrap();
        assert_eq!(replay["revision"], moved["revision"]);

        let members = project(&workspace, "list", json!({"collection_id": "makera-z1"})).unwrap();
        assert_eq!(listed_ids(&members), ["mount-a"]);
        let ungrouped = project(&workspace, "list", json!({"ungrouped": true})).unwrap();
        assert_eq!(listed_ids(&ungrouped), ["mount-c"]);
        let shown = project(&workspace, "get", json!({"project_id": "mount-a"})).unwrap();
        assert_eq!(
            shown["collection"],
            json!({"collection_id": "makera-z1", "name": "Makera Z1 Multiboard"})
        );

        // Removing a project that belongs elsewhere is refused.
        assert!(
            call(
                &workspace,
                "remove_projects",
                json!({"collection_id": "makera-z1", "project_ids": ["vase"]}),
            )
            .is_err()
        );
        let removed = call(
            &workspace,
            "remove_projects",
            json!({"collection_id": "makera-z1", "project_ids": ["mount-a", "mount-c"]}),
        )
        .unwrap();
        assert_eq!(removed["removed"], json!(["mount-a"]));
        assert_eq!(removed["collection"]["project_count"], 0);

        assert_eq!(
            workspace
                .read_generated_bytes(".printable/projects/mount-a.json")
                .unwrap()
                .1,
            record
        );
        assert_eq!(
            workspace
                .read_generated_bytes("projects/mount-a/mount.stl")
                .unwrap()
                .1,
            b"solid"
        );
    }

    #[test]
    fn unknown_projects_and_collections_change_nothing() {
        let (_root, workspace) = workspace(&["clip"]);
        call(
            &workspace,
            "create",
            json!({"collection_id": "desk", "name": "Desk"}),
        )
        .unwrap();
        let before = stored(&workspace);
        for (action, params) in [
            (
                "add_projects",
                json!({"collection_id": "desk", "project_ids": ["clip", "ghost"]}),
            ),
            (
                "add_projects",
                json!({"collection_id": "none", "project_ids": ["clip"]}),
            ),
            (
                "add_projects",
                json!({"collection_id": "desk", "project_ids": []}),
            ),
            (
                "create",
                json!({"collection_id": "other", "name": "Other", "project_ids": ["ghost"]}),
            ),
            (
                "create",
                json!({"collection_id": "desk", "name": "Renamed"}),
            ),
            ("create", json!({"collection_id": "Bad ID", "name": "Bad"})),
            ("create", json!({"collection_id": "blank", "name": "  "})),
            ("update", json!({"collection_id": "desk"})),
            ("update", json!({"collection_id": "none", "name": "x"})),
            ("delete", json!({"collection_id": "none"})),
            (
                "remove_projects",
                json!({"collection_id": "none", "project_ids": ["clip"]}),
            ),
            (
                "remove_projects",
                json!({"collection_id": "desk", "project_ids": ["clip", "clpi"]}),
            ),
        ] {
            assert!(
                call(&workspace, action, params.clone()).is_err(),
                "{action} {params}"
            );
        }
        assert_eq!(stored(&workspace), before);
        let unknown = project(&workspace, "list", json!({"collection_id": "none"})).unwrap_err();
        assert!(unknown.to_string().contains("no collection"), "{unknown}");
        assert!(
            project(
                &workspace,
                "list",
                json!({"collection_id": "desk", "ungrouped": true})
            )
            .is_err()
        );
    }

    #[test]
    fn deleting_keeps_projects_and_needs_consent_while_projects_remain() {
        let (_root, workspace) = workspace(&["clip", "hook"]);
        call(
            &workspace,
            "create",
            json!({"collection_id": "desk", "name": "Desk", "project_ids": ["clip", "hook"]}),
        )
        .unwrap();
        let refused = call(&workspace, "delete", json!({"collection_id": "desk"})).unwrap_err();
        assert!(
            refused.to_string().contains("ungroup_projects"),
            "{refused}"
        );
        let deleted = call(
            &workspace,
            "delete",
            json!({"collection_id": "desk", "ungroup_projects": true}),
        )
        .unwrap();
        assert_eq!(deleted["deleted"], true);
        assert_eq!(deleted["collection"], Value::Null);
        assert_eq!(deleted["removed"], json!(["clip", "hook"]));
        let index = read(&workspace).unwrap();
        assert!(index.collections().is_empty() && index.grouped().is_empty());
        assert_eq!(
            listed_ids(&project(&workspace, "list", json!({})).unwrap()),
            ["clip", "hook"]
        );
    }

    #[test]
    fn stale_and_concurrent_changes_are_refused() {
        let (_root, workspace) = workspace(&["clip"]);
        call(
            &workspace,
            "create",
            json!({"collection_id": "desk", "name": "Desk"}),
        )
        .unwrap();
        let stale = call(
            &workspace,
            "update",
            json!({"collection_id": "desk", "name": "Desk parts", "expected_revision": 0}),
        )
        .unwrap_err();
        assert!(stale.to_string().contains("expected revision 0"), "{stale}");
        let held = workspace.try_lock_reserved(LOCK_PATH).unwrap();
        let busy = call(
            &workspace,
            "add_projects",
            json!({"collection_id": "desk", "project_ids": ["clip"]}),
        )
        .unwrap_err();
        assert!(busy.to_string().contains("another request"), "{busy}");
        drop(held);
        let updated = call(
            &workspace,
            "update",
            json!({"collection_id": "desk", "name": "Desk parts", "expected_revision": 1}),
        )
        .unwrap();
        assert_eq!(updated["revision"], 2);
        assert_eq!(updated["collection"]["name"], "Desk parts");
    }

    #[test]
    fn a_damaged_collections_file_is_reported_not_guessed() {
        let (_root, workspace) = workspace(&["clip"]);
        let dangling = json!({"format_version": 1, "revision": 3, "collections": {},
                              "memberships": {"clip": "gone"}});
        workspace
            .write_reserved_artifact(INDEX_PATH, &serde_json::to_vec(&dangling).unwrap(), false)
            .unwrap();
        let error = call(&workspace, "list", json!({})).unwrap_err();
        assert!(error.to_string().contains("inconsistent"), "{error}");
        assert!(project(&workspace, "get", json!({"project_id": "clip"})).is_err());
    }

    #[test]
    fn new_projects_can_start_in_a_collection_and_refusals_create_nothing() {
        let (_root, workspace) = workspace(&["clip"]);
        call(
            &workspace,
            "create",
            json!({"collection_id": "desk", "name": "Desk"}),
        )
        .unwrap();
        let params = json!({"project_id": "hook", "name": "Hook", "collection_id": "desk"});
        let created = project(&workspace, "create", params.clone()).unwrap();
        assert_eq!(created["collection"]["collection_id"], "desk");
        // Repeating the request returns the same project in the same collection.
        assert_eq!(
            project(&workspace, "create", params.clone()).unwrap(),
            created
        );
        // Once ungrouped, a repeated request is refused instead of undoing that.
        call(
            &workspace,
            "remove_projects",
            json!({"collection_id": "desk", "project_ids": ["hook"]}),
        )
        .unwrap();
        let replay = project(&workspace, "create", params).unwrap_err();
        assert!(replay.to_string().contains("add_projects"), "{replay}");
        assert_eq!(read(&workspace).unwrap().membership("hook"), None);
        call(
            &workspace,
            "add_projects",
            json!({"collection_id": "desk", "project_ids": ["hook"]}),
        )
        .unwrap();
        let missing = project(
            &workspace,
            "create",
            json!({"project_id": "shelf", "name": "Shelf", "collection_id": "none"}),
        )
        .unwrap_err();
        assert!(missing.to_string().contains("no collection"), "{missing}");
        assert!(
            workspace
                .read_generated_bytes(".printable/projects/shelf.json")
                .is_err()
        );
        call(
            &workspace,
            "create",
            json!({"collection_id": "other", "name": "Other"}),
        )
        .unwrap();
        let elsewhere = project(
            &workspace,
            "create",
            json!({"project_id": "hook", "name": "Hook", "collection_id": "other"}),
        )
        .unwrap_err();
        // A concurrent collection change refuses the request before the
        // project is written.
        let held = workspace.try_lock_reserved(LOCK_PATH).unwrap();
        let busy = project(
            &workspace,
            "create",
            json!({"project_id": "shelf", "name": "Shelf", "collection_id": "desk"}),
        )
        .unwrap_err();
        assert!(busy.to_string().contains("another request"), "{busy}");
        assert!(
            workspace
                .read_generated_bytes(".printable/projects/shelf.json")
                .is_err()
        );
        drop(held);
        assert!(
            elsewhere.to_string().contains("already belongs"),
            "{elsewhere}"
        );
    }

    #[test]
    fn project_lists_skip_update_locks_and_filter_before_the_limit() {
        let (_root, workspace) = workspace(&["a", "b", "c", "d"]);
        // An update leaves the project's lock file beside the records.
        project(
            &workspace,
            "update",
            json!({"project_id": "a", "expected_record_version": 1, "description": "x"}),
        )
        .unwrap();
        assert_eq!(
            listed_ids(&project(&workspace, "list", json!({})).unwrap()),
            ["a", "b", "c", "d"]
        );
        call(
            &workspace,
            "create",
            json!({"collection_id": "late", "name": "Late", "project_ids": ["c", "d"]}),
        )
        .unwrap();
        let page = project(
            &workspace,
            "list",
            json!({"collection_id": "late", "limit": 1}),
        )
        .unwrap();
        assert_eq!(listed_ids(&page), ["c"]);
        assert_eq!(page["limit_reached"], true);
    }

    #[test]
    fn collection_pages_list_members_and_sort_cards_by_name() {
        let (_root, workspace) = workspace(&["a", "b", "c"]);
        call(
            &workspace,
            "create",
            json!({"collection_id": "zeta", "name": "zeta"}),
        )
        .unwrap();
        call(
            &workspace,
            "create",
            json!({"collection_id": "alpha", "name": "Alpha", "project_ids": ["a", "b"]}),
        )
        .unwrap();
        let listed = call(&workspace, "list", json!({})).unwrap();
        let names: Vec<&str> = listed["collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|card| card["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Alpha", "zeta"]);
        assert_eq!(listed["ungrouped_project_count"], 1);
        assert_eq!(
            listed["collections"][0]["sample_projects"][0]["name"],
            "Project a"
        );
        let got = call(
            &workspace,
            "get",
            json!({"collection_id": "alpha", "limit": 1}),
        )
        .unwrap();
        assert_eq!(
            got["projects"],
            json!([{"project_id": "a", "name": "Project a", "description": "", "missing": false}])
        );
        assert_eq!(got["limit_reached"], true);
        assert_eq!(got["collection"]["project_count"], 2);
    }
}
