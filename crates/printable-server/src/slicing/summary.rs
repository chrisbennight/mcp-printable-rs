//! Card-ready summaries of completed slices: when the slice finished, what it
//! was sliced for, its filaments with colours, and its plates.

use printable_workspace::{Workspace, WsError};
use serde_json::{Value, json};

use super::package;
use crate::error::ToolError;

pub const FILE: &str = "summary.json";
const PACKAGE: &str = "model.gcode.3mf";

/// Summarise the completed slice in workspace directory `output`.
///
/// `finished_unix_ms` comes from the terminal state when the worker recorded
/// it; older slices fall back to `state_modified_ms` and are marked estimated.
pub fn summarize(
    workspace: &Workspace,
    output: &str,
    state: &Value,
    state_modified_ms: u64,
) -> Result<Value, ToolError> {
    if state["status"] != "completed" {
        return Err(ToolError::Validation(
            "only completed slices have a summary".into(),
        ));
    }
    let package_path = format!("{output}/{PACKAGE}");
    let snapshot = workspace.snapshot_artifact(&package_path)?;
    let facts = package::facts(snapshot.path())?;
    let setup = &state["setup"];
    let configured = setup["filaments"].as_array().cloned().unwrap_or_default();
    let filaments = (0..configured.len().max(facts.filaments.len()))
        .map(|slot| {
            let chosen = configured.get(slot).unwrap_or(&Value::Null);
            let packaged = facts.filaments.get(slot);
            // Resolved Orca profiles store some values as one-element arrays.
            let pick = |value: &Value, fallback: Option<&String>| {
                value
                    .as_str()
                    .or_else(|| value.get(0).and_then(Value::as_str))
                    .map(str::to_owned)
                    .or_else(|| fallback.cloned())
            };
            json!({
                "slot": slot,
                "profile": pick(&chosen["profile"]["name"], packaged.and_then(|f| f.profile.as_ref())),
                "filament_type": pick(&chosen["profile"]["filament_type"], packaged.and_then(|f| f.filament_type.as_ref())),
                "colour": pick(&chosen["colour"], packaged.and_then(|f| f.colour.as_ref())),
            })
        })
        .collect::<Vec<_>>();
    let recorded = state["finished_unix_ms"].as_u64();
    Ok(json!({
        "format_version": 1,
        "slice": state["slice"],
        "finished_unix_ms": recorded.unwrap_or(state_modified_ms),
        "finished_estimated": recorded.is_none(),
        "package": package_path,
        "build_plate": setup["build_plate"],
        "printer": setup["printer"],
        "process": setup["process"]["name"],
        "printer_profile": facts.printer_profile,
        "process_profile": facts.process_profile,
        "plates": facts.plates,
        "plate_count": facts.plates.len(),
        "thumbnails": facts.thumbnails,
        "filaments": filaments,
        "filaments_truncated": facts.filaments_truncated,
    }))
}

/// Record the summary of a slice that has just completed. An existing summary
/// is kept: the slice directory is immutable once its state is terminal.
pub fn write(workspace: &Workspace, output: &str, state: &Value) -> Result<(), ToolError> {
    let summary = summarize(workspace, output, state, crate::projects::unix_ms())?;
    match workspace.write_generated_bytes(
        &format!("{output}/{FILE}"),
        &serde_json::to_vec(&summary)?,
        false,
    ) {
        Ok(_) | Err(WsError::AlreadyExists(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// The recorded summary, or one computed now for slices that predate it.
pub fn read_or_summarize(
    workspace: &Workspace,
    output: &str,
    state: &Value,
    state_modified_ms: u64,
) -> Result<Value, ToolError> {
    match workspace.read_generated_bytes(&format!("{output}/{FILE}")) {
        Ok((_, bytes)) => Ok(serde_json::from_slice(&bytes)?),
        Err(WsError::NotFound(_)) => summarize(workspace, output, state, state_modified_ms),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slicing::package::fixture;

    fn completed(finished: Option<u64>) -> Value {
        let mut state = json!({
            "status": "completed",
            "slice": {"project_id": "desk", "output_dir": "slice-1"},
            "setup": {
                "build_plate": "Textured PEI Plate",
                "printer": {"name": "Bambu Lab X1 Carbon 0.4 nozzle"},
                "process": {"name": "0.20mm Standard @BBL X1C"},
                "filaments": [{"slice_material_index": 0, "colour": null,
                    "profile": {"name": "Bambu PLA Basic @BBL X1C", "filament_type": ["PLA"]}}],
            },
        });
        if let Some(finished) = finished {
            state["finished_unix_ms"] = json!(finished);
        }
        state
    }

    fn workspace_with_package() -> (tempfile::TempDir, Workspace) {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(Some(root.path()), None).unwrap();
        let settings =
            json!({"filament_colour": ["#00aa00", "#ffffff"], "filament_type": ["ABS", "PETG"],
            "filament_settings_id": ["Bambu PLA Basic @BBL X1C", "Bambu PETG HF @BBL X1C"]})
            .to_string();
        let thumbnail = fixture::png(2, 2);
        let package = fixture::package(&[
            ("Metadata/plate_1.gcode", b"G28"),
            ("Metadata/plate_2.gcode", b"G28"),
            ("Metadata/plate_1.png", &thumbnail),
            ("Metadata/project_settings.config", settings.as_bytes()),
        ]);
        workspace
            .write_artifact("projects/desk/slice-1/model.gcode.3mf", &package, false)
            .unwrap();
        (root, workspace)
    }

    #[test]
    fn summaries_merge_configured_and_packaged_filaments() {
        let (_root, workspace) = workspace_with_package();
        let summary =
            summarize(&workspace, "projects/desk/slice-1", &completed(Some(42)), 7).unwrap();
        assert_eq!(summary["finished_unix_ms"], 42);
        assert_eq!(summary["finished_estimated"], false);
        assert_eq!(summary["plate_count"], 2);
        assert_eq!(summary["thumbnails"], json!([1]));
        assert_eq!(summary["filaments"][0]["colour"], "#00AA00");
        assert_eq!(
            summary["filaments"][0]["profile"],
            "Bambu PLA Basic @BBL X1C"
        );
        assert_eq!(summary["filaments"][0]["filament_type"], "PLA");
        assert_eq!(summary["filaments"][1]["filament_type"], "PETG");
        assert_eq!(summary["process"], "0.20mm Standard @BBL X1C");
    }

    #[test]
    fn older_slices_are_estimated_and_recorded_summaries_are_kept() {
        let (_root, workspace) = workspace_with_package();
        let output = "projects/desk/slice-1";
        let estimated = read_or_summarize(&workspace, output, &completed(None), 7).unwrap();
        assert_eq!(estimated["finished_unix_ms"], 7);
        assert_eq!(estimated["finished_estimated"], true);
        write(&workspace, output, &completed(Some(42))).unwrap();
        write(&workspace, output, &completed(Some(99))).unwrap();
        let recorded = read_or_summarize(&workspace, output, &completed(None), 7).unwrap();
        assert_eq!(recorded["finished_unix_ms"], 42);
        let mut failed = completed(Some(1));
        failed["status"] = json!("failed");
        assert!(summarize(&workspace, output, &failed, 7).is_err());
    }
}
