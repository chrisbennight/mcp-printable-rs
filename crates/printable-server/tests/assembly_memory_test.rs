//! Linux process measurements for assembly preparation through the public tool.
//! A file-inspecting worker isolates preparation from geometry computation;
//! real clearance and worker-resource contracts are covered in tools_test.

#![cfg(target_os = "linux")]

use std::{
    io::{BufWriter, Write},
    os::unix::fs::PermissionsExt,
    process::Command,
    sync::Arc,
    time::Instant,
};

use printable_blender::{BlenderClient, ClientOptions};
use printable_server::{config::Settings, mcp::PrintableServer};
use printable_workspace::Workspace;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

fn peak_rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmHWM:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .expect("Linux reports peak resident memory")
}

#[test]
fn assembly_preparation_memory_remains_bounded() {
    let Some(size_mib) = std::env::var("PRINTABLE_TEST_ASSEMBLY_MIB").ok() else {
        let executable = std::env::current_exe().unwrap();
        let mut measurements = Vec::new();
        for size in [8, 64] {
            let output = Command::new(&executable)
                .args([
                    "--exact",
                    "assembly_preparation_memory_remains_bounded",
                    "--nocapture",
                ])
                .env("PRINTABLE_TEST_ASSEMBLY_MIB", size.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            let sample: Value = serde_json::from_str(
                stdout
                    .lines()
                    .find_map(|line| line.strip_prefix("ASSEMBLY_MEMORY="))
                    .expect("child measurement"),
            )
            .unwrap();
            measurements.push(sample);
        }
        println!("ASSEMBLY_MEMORY_EVIDENCE={}", json!(measurements));
        for sample in &measurements {
            assert!(
                sample["server_peak_growth_kib"].as_u64().unwrap() < 16 * 1024,
                "server preparation retained whole models: {sample}"
            );
            assert_eq!(
                sample["temporary_input_bytes"].as_u64().unwrap(),
                2 * sample["input_bytes_each"].as_u64().unwrap(),
                "redundant input copies: {sample}"
            );
        }
        return;
    };

    let size_mib: u64 = size_mib.parse().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let tetrahedra = (size_mib * 1024 * 1024 - 84) / 200;
    let input_bytes = 84 + tetrahedra * 200;
    for name in ["fixed.stl", "moving.stl"] {
        let mut file = BufWriter::new(std::fs::File::create(root.join(name)).unwrap());
        let mut header = [0_u8; 80];
        header[..15].copy_from_slice(b"assembly-memory");
        file.write_all(&header).unwrap();
        file.write_all(&u32::try_from(tetrahedra * 4).unwrap().to_le_bytes())
            .unwrap();
        for index in 0..tetrahedra {
            let x = (index * 2) as f32;
            let vertices = [[x, 0., 0.], [x + 1., 0., 0.], [x, 1., 0.], [x, 0., 1.]];
            for face in [[0, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]] {
                file.write_all(&[0_u8; 12]).unwrap();
                for vertex in face {
                    for coordinate in vertices[vertex] {
                        file.write_all(&coordinate.to_le_bytes()).unwrap();
                    }
                }
                file.write_all(&[0_u8; 2]).unwrap();
            }
        }
        file.flush().unwrap();
    }
    let worker = temporary.path().join("worker");
    let root_json = serde_json::to_string(&root).unwrap();
    let source = format!(
        r#"#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
started = time.monotonic()
root = Path({root_json})
fixed, moving, options = map(Path, sys.argv[1:])
inputs = [fixed, moving]
root.joinpath('worker.started').touch()
time.sleep(0.2)
snapshots = list((root / '.printable/storage/scratch').glob('*/payload/*.stl'))
unique_inputs = {{path.resolve() for path in inputs + snapshots}}
temporary_bytes = sum(path.stat().st_size for path in unique_inputs)
# Replace public filenames after preparation. The worker still observes the
# original immutable bytes; replacement must not influence this analysis.
for name in ['fixed.stl', 'moving.stl']:
    replacement = root / (name + '.replacement')
    replacement.write_bytes(b'replaced public input')
    replacement.replace(root / name)
for path in inputs:
    with path.open('rb') as stream:
        assert stream.read(15) == b'assembly-memory'
        stream.seek(-16, os.SEEK_END)
        assert len(stream.read()) == 16
peak_rss = next(int(line.split()[1]) for line in Path('/proc/self/status').read_text().splitlines() if line.startswith('VmHWM:'))
print(json.dumps({{'report': {{'input_bytes': [path.stat().st_size for path in inputs], 'temporary_input_bytes': temporary_bytes, 'worker_peak_rss_kib': peak_rss, 'worker_inspection_ms': (time.monotonic() - started) * 1000}}}}))
"#
    );
    std::fs::write(&worker, source).unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    let settings = Settings::from_lookup(|name| match name {
        "PRINTABLE_MCP_BEARER" => Some("0".repeat(64)),
        "PRINTABLE_WORKSPACE_ROOT" => Some(root.to_str().unwrap().to_owned()),
        "PRINTABLE_GEOMETRY_WORKER_BIN" => Some(worker.to_str().unwrap().to_owned()),
        _ => None,
    })
    .unwrap();
    let workspace = Arc::new(Workspace::open(Some(&root), None).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _runtime_guard = runtime.enter();
    let server = PrintableServer::new(
        workspace.clone(),
        Arc::new(BlenderClient::new("127.0.0.1", 9, ClientOptions::default())),
        Arc::new(settings),
    );
    let before = peak_rss_kib();
    let started = Instant::now();
    let result = runtime
        .block_on(async {
            let work = server.invoke_tool(
                CallToolRequestParams::new("analyze_assembly").with_arguments(
                    json!({"fixed_path": "fixed.stl", "moving_path": "moving.stl"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            );
            let observation = async {
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while !root.join("worker.started").exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("isolated worker starts");
                let status = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    server.invoke_tool(CallToolRequestParams::new("status")),
                )
                .await
                .expect("unrelated status remains responsive")
                .unwrap();
                assert_eq!(status.is_error, Some(false));
            };
            let (result, ()) = tokio::join!(work, observation);
            result
        })
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let payload = result.structured_content.unwrap();
    let report = &payload["report"];
    assert_eq!(report["input_bytes"], json!([input_bytes, input_bytes]));
    for artifact in ["fixed_artifact", "moving_artifact"] {
        assert_eq!(payload[artifact]["size_bytes"], input_bytes);
    }
    assert_eq!(
        std::fs::read(root.join("fixed.stl")).unwrap(),
        b"replaced public input"
    );
    assert_eq!(
        workspace
            .storage_usage()
            .unwrap()
            .by_class
            .get("temporary")
            .copied()
            .unwrap_or(0),
        0,
        "snapshots are cleaned after the worker exits"
    );
    let peak = peak_rss_kib();
    println!(
        "ASSEMBLY_MEMORY={}",
        json!({
            "input_mib_each": size_mib,
        "input_bytes_each": input_bytes,
            "server_peak_before_kib": before,
            "server_peak_after_kib": peak,
            "server_peak_growth_kib": peak.saturating_sub(before),
            "temporary_input_bytes": report["temporary_input_bytes"],
            "worker_peak_rss_kib": report["worker_peak_rss_kib"],
            "worker_inspection_ms": report["worker_inspection_ms"],
            "preparation_and_process_ms": started.elapsed().as_secs_f64() * 1000.0 - report["worker_inspection_ms"].as_f64().unwrap()
        })
    );
}
