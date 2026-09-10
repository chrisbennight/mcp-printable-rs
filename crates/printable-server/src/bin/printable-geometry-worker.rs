//! Memory-isolated worker for exact assembly CSG and motion analysis.

use std::{ffi::OsString, fs, io, process::ExitCode};

use printable_geom::{AssemblyOptions, analyze_assembly_stl};
use serde_json::json;

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

fn run() -> Result<printable_geom::AssemblyReport, WorkerFailure> {
    let [fixed_path, moving_path, options_path] = exact_args()?;
    let fixed = read_input(&fixed_path, "fixed STL")?;
    let moving = read_input(&moving_path, "moving STL")?;
    let options_bytes = read_input(&options_path, "assembly options")?;
    let options =
        serde_json::from_slice::<AssemblyOptions>(&options_bytes).map_err(|_| WorkerFailure {
            code: "validation",
            message: "assembly worker received invalid options".to_string(),
        })?;
    analyze_assembly_stl(&fixed, &moving, options).map_err(|error| WorkerFailure {
        code: error.code(),
        message: error.to_string(),
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
        message: "assembly worker requires fixed, moving, and options paths".to_string(),
    }
}

fn read_input(path: &OsString, label: &'static str) -> Result<Vec<u8>, WorkerFailure> {
    fs::read(path).map_err(|_| WorkerFailure {
        code: "geometry_worker_io",
        message: format!("assembly worker could not read {label}"),
    })
}
