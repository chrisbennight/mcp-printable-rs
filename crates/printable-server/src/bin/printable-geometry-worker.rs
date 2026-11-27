//! Memory-isolated worker for exact assembly CSG, motion analysis, and
//! print-ready bust and mesh preparation.
//!
//! `printable-geometry-worker <fixed.stl> <moving.stl> <options.json>` analyses
//! an assembly; `printable-geometry-worker --bust <request.json> <output.stl>`
//! writes a closed, upright bust; `--prepare-print <request.json> <output.stl>`
//! rebuilds a mesh as a printable solid; `printable-geometry-worker --relief
//! <request.json> <output-directory>` builds a relief from analysis maps;
//! `--lithophane <request.json> <output-directory>` builds a lithophane. All
//! print `{"report": ...}` or `{"error": {"code", "message"}}` on stdout.

use std::{ffi::OsString, fs, io, process::ExitCode};

use printable_geom::{AssemblyOptions, BustRequest, analyze_assembly_stl, bust_stl, prepare_bust};
use printable_server::tools::{bake_texture, lithophane, print_prep, relief};
use serde_json::{Value, json};

const BUST_FLAG: &str = "--bust";

struct WorkerFailure {
    code: &'static str,
    message: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(report) => {
            match serde_json::to_writer(io::stdout().lock(), &json!({ "report": report })) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::from(3),
            }
        }
        Err(error) => {
            let written = serde_json::to_writer(
                io::stdout().lock(),
                &json!({
                    "error": {
                        "code": error.code,
                        "message": error.message,
                    }
                }),
            )
            .is_ok();
            if written {
                ExitCode::from(2)
            } else {
                ExitCode::from(3)
            }
        }
    }
}

fn run() -> Result<Value, WorkerFailure> {
    let [first, second, third] = exact_args()?;
    if first == "--inspect-layout" {
        let bytes = read_input(&second, "layout request")?;
        let request: printable_server::slicing::layout::WorkerRequest =
            serde_json::from_slice(&bytes).map_err(|_| WorkerFailure {
                code: "validation",
                message: "invalid layout worker request".into(),
            })?;
        request.options.validate().map_err(|error| WorkerFailure {
            code: error.code(),
            message: error.to_string(),
        })?;
        if let Some(placement) = &request.placement {
            let limit = printable_geom::limit::WorkLimit::new(
                std::time::Duration::from_secs(request.options.timeout_seconds),
                || false,
            );
            limit
                .apply(|| {
                    let archive = printable_server::slicing::model::read_archive(
                        &request.source,
                        request.options.max_xml_bytes,
                        || limit.check().is_ok(),
                    )?;
                    printable_server::slicing::placement::write_snapshot(
                        &request.source,
                        &placement.destination,
                        &archive,
                        &placement.controls,
                    )
                })
                .map_err(|error: printable_server::error::ToolError| WorkerFailure {
                    code: error.code(),
                    message: error.to_string(),
                })?;
        }
        let inspected = request
            .placement
            .as_ref()
            .map_or(request.source.as_path(), |placement| {
                placement.destination.as_path()
            });
        let mut report =
            printable_server::slicing::layout::inspect_source(inspected, &request.options)
                .map_err(|error| WorkerFailure {
                    code: error.code(),
                    message: error.to_string(),
                })?;
        if let Some(placement) = &request.placement {
            report["layout_edit"] = json!({"source_sha256":placement.controls.source_sha256,
                "controls":placement.controls});
        }
        let mut file = fs::File::create(&third).map_err(|_| WorkerFailure {
            code: "geometry_worker_io",
            message: "cannot create layout report".into(),
        })?;
        serde_json::to_writer(&mut file, &report).map_err(|_| WorkerFailure {
            code: "geometry_worker_io",
            message: "cannot write layout report".into(),
        })?;
        return Ok(json!({"report_written":true}));
    }
    if first == BUST_FLAG {
        return run_bust(&second, &third);
    }
    if first == print_prep::FLAG {
        return print_prep::worker_run(second.as_ref(), third.as_ref()).map_err(|error| {
            WorkerFailure {
                code: error.code(),
                message: match error {
                    // The parent adds its own prefix to a worker failure.
                    printable_server::error::ToolError::GeometryWorker { message, .. } => message,
                    other => other.to_string(),
                },
            }
        });
    }
    if first == relief::FLAG {
        return relief::worker_run(second.as_ref(), third.as_ref()).map_err(|error| {
            WorkerFailure {
                code: error.code,
                message: error.message,
            }
        });
    }
    if first == lithophane::FLAG {
        return lithophane::worker_run(second.as_ref(), third.as_ref()).map_err(|error| {
            WorkerFailure {
                code: error.code,
                message: error.message,
            }
        });
    }
    if first == bake_texture::FLAG {
        return bake_texture::worker_run(second.as_ref(), third.as_ref()).map_err(|error| {
            WorkerFailure {
                code: error.code(),
                message: error.to_string(),
            }
        });
    }
    run_assembly(&first, &second, &third)
}

fn run_bust(request_path: &OsString, output_path: &OsString) -> Result<Value, WorkerFailure> {
    let request_bytes = read_input(request_path, "bust request")?;
    let request =
        serde_json::from_slice::<BustRequest>(&request_bytes).map_err(|_| WorkerFailure {
            code: "validation",
            message: "geometry worker received an invalid bust request".to_string(),
        })?;
    let (mesh, report) = prepare_bust(&request).map_err(|error| WorkerFailure {
        code: error.code(),
        message: error.to_string(),
    })?;
    fs::write(output_path, bust_stl(&mesh)).map_err(|_| WorkerFailure {
        code: "geometry_worker_io",
        message: "geometry worker could not write the bust STL".to_string(),
    })?;
    serde_json::to_value(report).map_err(|_| WorkerFailure {
        code: "geometry",
        message: "geometry worker could not encode the bust report".to_string(),
    })
}

fn run_assembly(
    fixed_path: &OsString,
    moving_path: &OsString,
    options_path: &OsString,
) -> Result<Value, WorkerFailure> {
    let fixed = read_input(fixed_path, "fixed STL")?;
    let moving = read_input(moving_path, "moving STL")?;
    let options_bytes = read_input(options_path, "assembly options")?;
    let options =
        serde_json::from_slice::<AssemblyOptions>(&options_bytes).map_err(|_| WorkerFailure {
            code: "validation",
            message: "assembly worker received invalid options".to_string(),
        })?;
    let report = analyze_assembly_stl(&fixed, &moving, options).map_err(|error| WorkerFailure {
        code: error.code(),
        message: error.to_string(),
    })?;
    serde_json::to_value(report).map_err(|_| WorkerFailure {
        code: "geometry",
        message: "assembly worker could not encode its report".to_string(),
    })
}

fn exact_args() -> Result<[OsString; 3], WorkerFailure> {
    let mut args = std::env::args_os();
    let _program = args.next();
    let values = [args.next(), args.next(), args.next()];
    let [Some(fixed), Some(moving), Some(options)] = values else {
        return Err(invalid_args());
    };
    if args.next().is_some() {
        return Err(invalid_args());
    }
    Ok([fixed, moving, options])
}

fn invalid_args() -> WorkerFailure {
    WorkerFailure {
        code: "validation",
        message: "geometry worker requires three arguments".to_string(),
    }
}

fn read_input(path: &OsString, label: &'static str) -> Result<Vec<u8>, WorkerFailure> {
    fs::read(path).map_err(|_| WorkerFailure {
        code: "geometry_worker_io",
        message: format!("geometry worker could not read {label}"),
    })
}
