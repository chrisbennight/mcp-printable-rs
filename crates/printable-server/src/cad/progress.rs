//! Native stages and completed files, separate from delivery qualification.

use std::{io::Read, path::Path};

use serde::Deserialize;
use serde_json::{Value, json};

use super::open_native_file;
use crate::error::ToolError;

#[derive(Debug, Deserialize)]
pub(super) struct Progress {
    pub stage: String,
    pub completed_artifacts: Vec<String>,
}

pub(super) fn read(directory: &Path) -> Result<Option<Progress>, ToolError> {
    let file = match open_native_file(&directory.join("progress.json")) {
        Ok(file) => file,
        Err(ToolError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let progress: Progress = serde_json::from_reader(file.take(8193))?;
    if ![
        "reading_source",
        "component_inspection",
        "step_export",
        "stl_export",
        "glb_export",
        "geometry_checks",
        "completed",
    ]
    .contains(&progress.stage.as_str())
        || progress.completed_artifacts.iter().any(|name| {
            ![
                "components.json",
                "measurements.json",
                "model.step",
                "model.stl",
                "model.glb",
            ]
            .contains(&name.as_str())
        })
    {
        return Err(ToolError::Cad(
            "native CAD progress contains an unsupported stage or artifact".into(),
        ));
    }
    Ok(Some(progress))
}

pub(super) fn observe(directory: &Path, state: &mut Value) -> Option<Progress> {
    match read(directory) {
        Ok(Some(progress)) => {
            state["native_stage"] = json!(progress.stage);
            state
                .as_object_mut()
                .expect("CAD state object")
                .remove("progress_error");
            Some(progress)
        }
        Ok(None) => None,
        Err(error) => {
            state["progress_error"] = json!({"code":error.code(),
                "message":"Native CAD progress is unavailable; inspect the retained diagnostics."});
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unusable_progress_is_explicit_without_becoming_a_native_execution_failure() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("progress.json"),
            b"unfinished metadata",
        )
        .unwrap();
        let mut state = json!({"status":"running"});
        assert!(observe(directory.path(), &mut state).is_none());
        assert_eq!(state["status"], "running");
        assert!(state.get("progress_error").is_some());
        assert!(state.get("native_stage").is_none());
        std::fs::write(
            directory.path().join("progress.json"),
            br#"{"stage":"stl_export","completed_artifacts":[]}"#,
        )
        .unwrap();
        assert!(observe(directory.path(), &mut state).is_some());
        assert_eq!(state["native_stage"], "stl_export");
        assert!(state.get("progress_error").is_none());
    }
}
