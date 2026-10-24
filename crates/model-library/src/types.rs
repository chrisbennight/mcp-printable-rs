//! Site-independent search results, model details and file listings.
//!
//! Every text field originates on a public website and is untrusted. Values
//! are bounded and stripped of markup before they reach these types.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A supported public model repository.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Site {
    MakerWorld,
    Printables,
    Thingiverse,
}

impl Site {
    pub const ALL: [Site; 3] = [Site::MakerWorld, Site::Printables, Site::Thingiverse];

    /// The lowercase identifier used in settings and tool parameters.
    pub fn id(self) -> &'static str {
        match self {
            Site::MakerWorld => "makerworld",
            Site::Printables => "printables",
            Site::Thingiverse => "thingiverse",
        }
    }

    /// Parse the lowercase identifier used in settings and tool parameters.
    pub fn from_id(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|site| site.id() == value)
    }
}

impl fmt::Display for Site {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Site::MakerWorld => "MakerWorld",
            Site::Printables => "Printables",
            Site::Thingiverse => "Thingiverse",
        })
    }
}

/// Result ordering. Each site maps these to its nearest native ordering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    #[default]
    Relevance,
    Popular,
    Newest,
}

/// One page of a keyword search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    /// One-based page number.
    pub page: u32,
    /// Results per page.
    pub limit: u32,
    pub sort: SearchSort,
}

/// The largest page size any site accepts.
pub const MAXIMUM_SEARCH_LIMIT: u32 = 50;

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelSummary {
    pub site: Site,
    /// The site's model identifier, accepted by `get` and `download`.
    pub model_id: String,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub designer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloads: Option<u64>,
    /// Whether the site reports that the model must be bought or redeemed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paid: Option<bool>,
}

/// One page of search hits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchPage {
    pub site: Site,
    pub query: String,
    pub page: u32,
    pub limit: u32,
    /// The site's reported match count, when it reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    pub models: Vec<ModelSummary>,
    /// The next page number, or null when the site reports no more results.
    pub next_page: Option<u32>,
}

/// What a downloadable file contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// Geometry or source such as STL, 3MF, STEP, OBJ or OpenSCAD.
    Model,
    /// A slicer project with a designer's print settings, such as a MakerWorld print profile.
    PrintProfile,
    /// Printer-specific sliced output such as G-code.
    Sliced,
    /// Anything else the site lists, such as instructions or images.
    Other,
}

/// One file a model offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteFile {
    /// The identifier `download` accepts in `files`.
    pub file_id: String,
    pub name: String,
    pub kind: FileKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Folder or print-profile details the site shows beside the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Site-provided download location, checked against the site's allowed
    /// file hosts before use; never shown to callers.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) location: Option<String>,
}

/// A model's published license.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct License {
    pub name: String,
    /// Whether the site reports that remixing is allowed, when it says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remixing_allowed: Option<bool>,
}

/// Everything needed to choose and download a model's files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelDetail {
    pub site: Site,
    pub model_id: String,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub designer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<License>,
    /// Plain-text summary, bounded and stripped of markup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paid: Option<bool>,
    pub files: Vec<RemoteFile>,
    /// Site-specific value needed to request downloads; never shown to callers.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) download_key: Option<String>,
}
