//! Print preparation for any workspace mesh: an exact solid rebuild in the
//! isolated geometry worker, optional plinth and hollowing, printer checks,
//! and the strict check every prepared mesh must pass before it is written.
//! The `head` and `photo` workflows share these parts.

use std::path::{Path, PathBuf};

use printable_geom::{Hollow, Plinth, PrintChecks, PrintPrepOptions};
use printable_workspace::Workspace;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::head::{PrintProcess, PrinterParams, in_range};
use super::{
    ValidationOptions, ValidationReport, analyze_stl, geometry_blocking, geometry_worker_error,
    geometry_worker_path, run_geometry_worker_process, validate_blender_artifact,
    validate_scad_destination,
};
use crate::{config::Settings, error::ToolError};

/// The geometry worker's flag for print preparation.
pub const FLAG: &str = "--prepare-print";

pub(super) const PLINTH_HEIGHT_MM: std::ops::RangeInclusive<f64> = 2.0..=100.0;
const PLINTH_DIAMETER_MM: std::ops::RangeInclusive<f64> = 10.0..=500.0;
const PLINTH_CHAMFER_MM: std::ops::RangeInclusive<f64> = 0.0..=5.0;
/// A chamfer about two layers deep keeps the first layers, which spread as
/// they are pressed onto the build plate, within the plinth's outline.
const DEFAULT_FDM_CHAMFER_MM: f64 = 0.5;
const HOLLOW_WALL_MM: std::ops::RangeInclusive<f64> = 1.0..=20.0;
const DRAIN_DIAMETER_MM: std::ops::RangeInclusive<f64> = 1.0..=10.0;
const DEFAULT_DRAIN_DIAMETER_MM: f64 = 3.5;
const EDGE_LENGTH_MM: std::ops::RangeInclusive<f64> = 0.05..=10.0;

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct MeshPreparePrintParams {
    /// Workspace STL, millimetres, Z up.
    source_path: String,
    /// Workspace path for the prepared STL.
    path: String,
    /// Replace an existing file at `path`.
    #[serde(default)]
    overwrite: bool,
    /// Rebuilt facet edge, 0.05 to 10 mm; finer detail is smoothed. Default
    /// 0.4% of the longest side, at least 0.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edge_length_mm: Option<f64>,
    /// Remove floating parts; default false keeps and reports them.
    #[serde(default)]
    remove_floating_parts: bool,
    /// Keep inward-facing pieces as cavities; default false fills them.
    #[serde(default)]
    keep_cavities: bool,
    /// Plinth added below, 2 to 100 mm; none by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plinth_height_mm: Option<f64>,
    /// 10 to 500 mm (default 70% of the width).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plinth_diameter_mm: Option<f64>,
    /// Bottom chamfer, 0 to 5 mm (default 0.5 for FDM, else 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plinth_chamfer_mm: Option<f64>,
    /// Hollow with walls 1 to 20 mm and drain holes; solid when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hollow_wall_mm: Option<f64>,
    /// 1 to 10 mm (default 3.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drain_hole_diameter_mm: Option<f64>,
    /// Reports overhangs and walls thinner than the nozzle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    printer: Option<PrinterParams>,
}

/// What the geometry worker reads for one preparation.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRequest {
    /// A private copy of the source STL.
    pub source: PathBuf,
    pub options: PrintPrepOptions,
    /// The overhang angle the strict check's validation report uses.
    pub overhang_angle_degrees: f64,
    /// What the prepared mesh is called in a refusal.
    pub subject: String,
}

/// What the geometry worker returns: the preparation report and the
/// validation report of the written STL, which passed the strict check.
#[derive(Deserialize)]
pub(crate) struct Prepared {
    pub preparation: Value,
    pub validation: Value,
}

/// The plinth a request describes; `default_height_mm` applies when no
/// height is given, and a height of zero means none.
pub(super) fn plinth_options(
    height_mm: Option<f64>,
    diameter_mm: Option<f64>,
    chamfer_mm: Option<f64>,
    default_height_mm: Option<f64>,
    fdm: bool,
) -> Result<Option<Plinth>, ToolError> {
    let height = match height_mm {
        Some(0.0) => None,
        Some(height) => Some(in_range("plinth_height_mm", height, &PLINTH_HEIGHT_MM)?),
        None => default_height_mm,
    };
    let Some(height) = height else {
        if diameter_mm.is_some() || chamfer_mm.is_some() {
            return Err(ToolError::Validation(
                "plinth_diameter_mm and plinth_chamfer_mm require a plinth".into(),
            ));
        }
        return Ok(None);
    };
    let plinth = Plinth {
        height_mm: height,
        diameter_mm: diameter_mm
            .map(|diameter| in_range("plinth_diameter_mm", diameter, &PLINTH_DIAMETER_MM))
            .transpose()?,
        chamfer_mm: match chamfer_mm {
            Some(chamfer) => in_range("plinth_chamfer_mm", chamfer, &PLINTH_CHAMFER_MM)?,
            None if fdm => DEFAULT_FDM_CHAMFER_MM,
            None => 0.0,
        },
    };
    if plinth.chamfer_mm >= plinth.height_mm {
        return Err(ToolError::Validation(
            "plinth_chamfer_mm must be less than the plinth's height".into(),
        ));
    }
    Ok(Some(plinth))
}

/// The hollowing a request describes.
pub(super) fn hollow_options(
    wall_mm: Option<f64>,
    drain_diameter_mm: Option<f64>,
) -> Result<Option<Hollow>, ToolError> {
    match (wall_mm, drain_diameter_mm) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(ToolError::Validation(
            "drain_hole_diameter_mm requires hollow_wall_mm".into(),
        )),
        (Some(wall), diameter) => Ok(Some(Hollow {
            wall_mm: in_range("hollow_wall_mm", wall, &HOLLOW_WALL_MM)?,
            drain_diameter_mm: in_range(
                "drain_hole_diameter_mm",
                diameter.unwrap_or(DEFAULT_DRAIN_DIAMETER_MM),
                &DRAIN_DIAMETER_MM,
            )?,
        })),
    }
}

/// Validate a prepared STL and refuse it unless it is a valid solid known to
/// be free of self-intersections and of shells touching or crossing one
/// another, not merely one where none were found.
pub(crate) fn strict_check(
    bytes: &[u8],
    options: ValidationOptions,
    subject: &str,
) -> Result<ValidationReport, ToolError> {
    let validation = analyze_stl(bytes, options)?;
    if !validation.solid_geometry
        || validation.topology.self_intersections != Some(0)
        || validation.topology.shell_intersections != Some(0)
    {
        let issues: Vec<_> = validation
            .issues
            .iter()
            .map(|issue| issue.message.as_str())
            .collect();
        return Err(geometry_worker_error(
            "print_check_failed",
            format!(
                "the prepared {subject} failed mesh validation and was not written: {}",
                issues.join("; ")
            ),
        ));
    }
    Ok(validation)
}

/// Prepare `source` in the geometry worker, writing the result to `output`
/// only when it passes the strict check, which also runs in the worker so
/// that the full mesh is decoded only within its address-space limit.
/// Blocking; the caller's work limit stops it.
pub(crate) fn run_worker(
    worker_bin: &Path,
    memory_bytes: u64,
    source: &Path,
    (options, overhang_angle_degrees, subject): (PrintPrepOptions, f64, &str),
    output: &Path,
) -> Result<Prepared, ToolError> {
    let staging = tempfile::Builder::new()
        .prefix("printable-print-prep-")
        .tempdir()?;
    let request_path = staging.path().join("request.json");
    std::fs::write(
        &request_path,
        serde_json::to_vec(&WorkerRequest {
            source: source.to_path_buf(),
            options,
            overhang_angle_degrees,
            subject: subject.to_owned(),
        })?,
    )?;
    Ok(serde_json::from_value(run_geometry_worker_process(
        worker_bin,
        memory_bytes,
        &[FLAG.as_ref(), request_path.as_os_str(), output.as_os_str()],
        None,
    )?)?)
}

/// Run in the disposable geometry worker: read the request and source STL,
/// prepare it, check it strictly and write the binary STL to `output`.
pub fn worker_run(request_path: &Path, output: &Path) -> Result<Value, ToolError> {
    let request: WorkerRequest = serde_json::from_slice(&std::fs::read(request_path)?)?;
    let mesh = printable_geom::decode_stl(&std::fs::read(&request.source)?)?;
    let (prepared, report) = printable_geom::prepare_print(mesh, &request.options)?;
    let bytes = printable_geom::bust_stl(&prepared);
    drop(prepared);
    let validation = strict_check(
        &bytes,
        ValidationOptions {
            overhang_angle_degrees: request.overhang_angle_degrees,
            ..ValidationOptions::default()
        },
        &request.subject,
    )?;
    std::fs::write(output, &bytes)?;
    Ok(json!({"preparation": report, "validation": validation}))
}

/// Prepare a workspace STL for printing.
pub(super) async fn prepare_print(
    workspace: std::sync::Arc<Workspace>,
    settings: &Settings,
    params: MeshPreparePrintParams,
) -> Result<Value, ToolError> {
    validate_blender_artifact(&workspace, &params.source_path, ".stl", true)?;
    validate_scad_destination(&workspace, &params.path, ".stl", params.overwrite)?;
    let edge_length_mm = params
        .edge_length_mm
        .map(|edge| in_range("edge_length_mm", edge, &EDGE_LENGTH_MM))
        .transpose()?;
    let printer = params
        .printer
        .as_ref()
        .map(PrinterParams::resolve)
        .transpose()?;
    let fdm = printer.is_some_and(|printer| printer.process == PrintProcess::Fdm);
    let options = PrintPrepOptions {
        edge_length_mm,
        target_triangles: None,
        keep_cavities: params.keep_cavities,
        parts: if params.remove_floating_parts {
            printable_geom::KeptParts::Grounded
        } else {
            printable_geom::KeptParts::All
        },
        plinth: plinth_options(
            params.plinth_height_mm,
            params.plinth_diameter_mm,
            params.plinth_chamfer_mm,
            None,
            fdm,
        )?,
        hollow: hollow_options(params.hollow_wall_mm, params.drain_hole_diameter_mm)?,
        checks: printer.map(|printer| PrintChecks {
            overhang_degrees: printer.maximum_overhang_degrees,
            min_feature_mm: printer.minimum_wall_mm,
        }),
        longest_side_mm: None,
    };
    let overhang_angle_degrees = printer.map_or(
        ValidationOptions::default().overhang_angle_degrees,
        |printer| printer.maximum_overhang_degrees,
    );
    let worker_bin = geometry_worker_path(settings)?;
    let memory_bytes = settings.geometry_worker_memory_bytes;
    geometry_blocking(move || {
        let snapshot = workspace.snapshot_artifact(&params.source_path)?;
        let staging = tempfile::Builder::new()
            .prefix("printable-print-prep-")
            .tempdir()?;
        let output = staging.path().join("prepared.stl");
        let prepared = run_worker(
            &worker_bin,
            memory_bytes,
            snapshot.path(),
            (options, overhang_angle_degrees, "mesh"),
            &output,
        )?;
        drop(snapshot);
        printable_geom::limit::check()?;
        let artifact =
            workspace.commit_generated_artifact(&params.path, &output, params.overwrite)?;
        let mut response = json!({
            "artifact": artifact,
            "units": "millimetres",
            "preparation": prepared.preparation,
            "validation": prepared.validation,
        });
        if let Some(printer) = printer {
            response["printer"] = serde_json::to_value(printer)?;
        }
        Ok::<_, ToolError>(response)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube(low: f64) -> printable_geom::TriangleMesh {
        printable_geom::TriangleMesh {
            vertices: (0..8)
                .map(|corner| [0, 1, 2].map(|axis| low + f64::from((corner >> axis) & 1) * 10.0))
                .collect(),
            triangles: vec![
                [0, 2, 1],
                [1, 2, 3],
                [4, 5, 6],
                [5, 7, 6],
                [0, 1, 4],
                [1, 5, 4],
                [2, 6, 3],
                [3, 6, 7],
                [0, 4, 2],
                [2, 4, 6],
                [1, 3, 5],
                [3, 7, 5],
            ],
        }
    }

    #[test]
    fn the_strict_check_refuses_shells_that_cross_each_other() {
        let mut overlapping = cube(0.0);
        let other = cube(5.0);
        overlapping.triangles.extend(
            other
                .triangles
                .iter()
                .map(|triangle| triangle.map(|index| index + 8)),
        );
        overlapping.vertices.extend(other.vertices);
        let single = strict_check(
            &printable_geom::bust_stl(&cube(0.0)),
            ValidationOptions::default(),
            "mesh",
        );
        assert!(single.is_ok());
        let error = strict_check(
            &printable_geom::bust_stl(&overlapping),
            ValidationOptions::default(),
            "mesh",
        )
        .unwrap_err();
        assert_eq!(error.code(), "print_check_failed");
    }
}
