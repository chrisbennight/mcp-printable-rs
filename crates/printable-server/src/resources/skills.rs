//! Bundled client-run skills. Requests select compiled content, never host paths.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ToolError;

pub const CAMERA_NAME: &str = "inspect-printer-camera";
pub const CAMERA_URI: &str = "printable://skills/inspect-printer-camera/SKILL.md";
pub const CAMERA_DESCRIPTION: &str = "Inspect FDM camera snapshots for spaghetti, collapsed or displaced structures and collision risk using a vision sub-agent; recommend notification or an authorized pause while accepting normal toolhead occlusion.";
pub const CAMERA_BODY: &str = include_str!("../../../../skills/inspect-printer-camera/SKILL.md");

pub const MOLD_NAME: &str = "image-to-mold";
pub const MOLD_URI: &str = "printable://skills/image-to-mold/SKILL.md";
pub const MOLD_DESCRIPTION: &str = "Turn images, logos, or SVG artwork into calibrated relief depth maps and printable positive masters, silicone casting trays, or negative cavities; validate geometry and review requested slices.";
pub const MOLD_BODY: &str = include_str!("../../../../skills/image-to-mold/SKILL.md");

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    /// Exact name returned by skill.list.
    pub name: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SkillRequest {
    List(ListParams),
    Get(GetParams),
}

#[derive(Serialize, JsonSchema)]
pub struct SkillMetadata {
    name: &'static str,
    description: &'static str,
    uri: &'static str,
    /// SHA-256 of the exact bundled UTF-8 SKILL.md, as lowercase hexadecimal.
    sha256: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
pub enum SkillResult {
    List {
        skills: Vec<SkillMetadata>,
    },
    Get {
        skill: SkillMetadata,
        instructions: &'static str,
    },
}

fn camera_metadata() -> SkillMetadata {
    SkillMetadata {
        name: CAMERA_NAME,
        description: CAMERA_DESCRIPTION,
        uri: CAMERA_URI,
        sha256: crate::tools::sha256_hex(CAMERA_BODY.as_bytes()),
    }
}

fn mold_metadata() -> SkillMetadata {
    SkillMetadata {
        name: MOLD_NAME,
        description: MOLD_DESCRIPTION,
        uri: MOLD_URI,
        sha256: crate::tools::sha256_hex(MOLD_BODY.as_bytes()),
    }
}

pub fn dispatch(request: SkillRequest) -> Result<Value, ToolError> {
    let result = match request {
        SkillRequest::List(_) => SkillResult::List {
            skills: vec![camera_metadata(), mold_metadata()],
        },
        SkillRequest::Get(params) if params.name == CAMERA_NAME => SkillResult::Get {
            skill: camera_metadata(),
            instructions: CAMERA_BODY,
        },
        SkillRequest::Get(params) if params.name == MOLD_NAME => SkillResult::Get {
            skill: mold_metadata(),
            instructions: MOLD_BODY,
        },
        SkillRequest::Get(_) => {
            return Err(ToolError::Validation("unknown bundled skill".into()));
        }
    };
    Ok(serde_json::to_value(result)?)
}
