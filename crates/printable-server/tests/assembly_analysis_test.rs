//! Linux measurements and ownership checks for isolated assembly analysis.

#![cfg(target_os = "linux")]

use std::{
    io::{BufWriter, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
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

fn cube(path: &Path, subdivisions: u32, shift_x: f32) {
    let faces = [
        ([0., 0., 0.], [0., 1., 0.], [1., 0., 0.]),
        ([0., 0., 1.], [1., 0., 0.], [0., 1., 0.]),
        ([0., 0., 0.], [0., 0., 1.], [0., 1., 0.]),
        ([1., 0., 0.], [0., 1., 0.], [0., 0., 1.]),
        ([0., 0., 0.], [1., 0., 0.], [0., 0., 1.]),
        ([0., 1., 0.], [0., 0., 1.], [1., 0., 0.]),
    ];
    let mut output = BufWriter::new(std::fs::File::create(path).unwrap());
    output.write_all(&[0_u8; 80]).unwrap();
    output
        .write_all(&(12 * subdivisions * subdivisions).to_le_bytes())
        .unwrap();
    for (origin, u, v) in faces {
        let point = |i: u32, j: u32| {
            std::array::from_fn::<_, 3, _>(|axis| {
                origin[axis]
                    + u[axis] * i as f32 / subdivisions as f32
                    + v[axis] * j as f32 / subdivisions as f32
                    + if axis == 0 { shift_x } else { 0. }
            })
        };
        for i in 0..subdivisions {
            for j in 0..subdivisions {
                let points = [
                    point(i, j),
                    point(i + 1, j),
                    point(i + 1, j + 1),
                    point(i, j + 1),
                ];
                for triangle in [[0, 1, 2], [0, 2, 3]] {
                    output.write_all(&[0_u8; 12]).unwrap();
                    for vertex in triangle {
                        for coordinate in points[vertex] {
                            output.write_all(&coordinate.to_le_bytes()).unwrap();
                        }
                    }
                    output.write_all(&[0_u8; 2]).unwrap();
                }
            }
        }
    }
    output.flush().unwrap();
}

fn worker_script(path: &Path, source: &str) {
    std::fs::write(path, source).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn server(root: &Path, worker: &Path) -> (PrintableServer, Arc<Workspace>) {
    let settings = Settings::from_lookup(|name| match name {
        "PRINTABLE_MCP_BEARER" => Some("0".repeat(64)),
        "PRINTABLE_WORKSPACE_ROOT" => Some(root.to_str().unwrap().to_owned()),
        "PRINTABLE_GEOMETRY_WORKER_BIN" => Some(worker.to_str().unwrap().to_owned()),
        _ => None,
    })
    .unwrap();
    let workspace = Arc::new(Workspace::open(Some(root), None).unwrap());
    let server = PrintableServer::new(
        Arc::clone(&workspace),
        Arc::new(BlenderClient::new("127.0.0.1", 9, ClientOptions::default())),
        Arc::new(settings),
    );
    (server, workspace)
}

fn analysis_request() -> CallToolRequestParams {
    CallToolRequestParams::new("analyze_assembly").with_arguments(
        json!({"fixed_path": "fixed.stl", "moving_path": "moving.stl"})
            .as_object()
            .unwrap()
            .clone(),
    )
}

#[test]
fn increasing_tessellation_preserves_real_worker_measurements() {
    let Ok(subdivisions) = std::env::var("PRINTABLE_TEST_ASSEMBLY_SUBDIVISIONS") else {
        let executable = std::env::current_exe().unwrap();
        let mut measurements = Vec::new();
        // Separate processes prevent an earlier trial's high-water mark from
        // hiding later server allocations. Powers of two have exact f32 grids.
        for subdivisions in [2, 8, 32] {
            for case in ["separated", "overlapping"] {
                for trial in 1..=3 {
                    let output = Command::new(&executable)
                        .args([
                            "--exact",
                            "increasing_tessellation_preserves_real_worker_measurements",
                            "--nocapture",
                        ])
                        .env(
                            "PRINTABLE_TEST_ASSEMBLY_SUBDIVISIONS",
                            subdivisions.to_string(),
                        )
                        .env("PRINTABLE_TEST_ASSEMBLY_CASE", case)
                        .env("PRINTABLE_TEST_ASSEMBLY_TRIAL", trial.to_string())
                        .output()
                        .unwrap();
                    assert!(
                        output.status.success(),
                        "{}\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let stdout = String::from_utf8(output.stdout).unwrap();
                    let sample: Value = serde_json::from_str(
                        stdout
                            .lines()
                            .find_map(|line| line.strip_prefix("ASSEMBLY_ANALYSIS="))
                            .expect("child measurement"),
                    )
                    .unwrap();
                    measurements.push(sample);
                }
            }
        }
        println!("ASSEMBLY_ANALYSIS_EVIDENCE={}", json!(measurements));
        return;
    };
    let subdivisions: u32 = subdivisions.parse().unwrap();
    let case = std::env::var("PRINTABLE_TEST_ASSEMBLY_CASE").unwrap();
    let separated = case == "separated";
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    cube(&root.join("fixed.stl"), subdivisions, 0.);
    cube(
        &root.join("moving.stl"),
        subdivisions,
        if separated { 2. } else { 0.5 },
    );
    let input_bytes = std::fs::metadata(root.join("fixed.stl")).unwrap().len();
    let worker = temporary.path().join("measured-worker");
    let real_worker =
        serde_json::to_string(env!("CARGO_BIN_EXE_printable-geometry-worker")).unwrap();
    let root_json = serde_json::to_string(&root).unwrap();
    worker_script(
        &worker,
        &format!(
            r#"#!/usr/bin/env python3
import json, resource, subprocess, sys, time
from pathlib import Path
root = Path({root_json})
inputs = list(map(Path, sys.argv[1:3]))
snapshots = list((root / '.printable/storage/scratch').glob('*/payload/*.stl'))
temporary_bytes = sum(path.stat().st_size for path in {{path.resolve() for path in inputs + snapshots}})
limit = resource.getrlimit(resource.RLIMIT_AS)[0]
started = time.monotonic()
completed = subprocess.run([{real_worker}, *sys.argv[1:]], capture_output=True, check=False)
elapsed_ms = (time.monotonic() - started) * 1000
if completed.returncode != 0:
    sys.stdout.buffer.write(completed.stdout)
    sys.exit(completed.returncode)
envelope = json.loads(completed.stdout)
envelope['report']['measurement'] = {{'worker_analysis_process_ms': elapsed_ms, 'worker_peak_rss_kib': resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss, 'worker_address_space_limit_bytes': limit, 'temporary_input_bytes': temporary_bytes}}
print(json.dumps(envelope))
"#
        ),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _runtime_guard = runtime.enter();
    let (server, workspace) = server(&root, &worker);
    let before = peak_rss_kib();
    let started = Instant::now();
    let result = runtime
        .block_on(server.invoke_tool(analysis_request()))
        .unwrap();
    let total_ms = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let payload = result.structured_content.unwrap();
    let report = &payload["report"];
    for part in ["fixed", "moving"] {
        assert!(
            (report[part]["volume_mm3"].as_f64().unwrap() - 1.).abs() < 1e-6,
            "{report}"
        );
        assert_eq!(report[part]["triangles"], 12 * subdivisions * subdivisions);
    }
    let static_report = &report["static_analysis"];
    assert_eq!(
        static_report["relation"],
        if separated {
            "separated"
        } else {
            "interfering"
        }
    );
    assert!(
        (static_report["clearance_mm"].as_f64().unwrap() - if separated { 1. } else { 0. }).abs()
            < 1e-6
    );
    assert!(
        (static_report["interference_volume_mm3"].as_f64().unwrap()
            - if separated { 0. } else { 0.5 })
        .abs()
            < 1e-6
    );
    assert_eq!(
        workspace
            .storage_usage()
            .unwrap()
            .by_class
            .get("temporary")
            .copied()
            .unwrap_or(0),
        0
    );
    let measurement = &report["measurement"];
    assert_eq!(
        measurement["worker_address_space_limit_bytes"],
        1024_u64 * 1024 * 1024
    );
    println!(
        "ASSEMBLY_ANALYSIS={}",
        json!({
            "subdivisions": subdivisions,
            "triangles_each": 12 * subdivisions * subdivisions,
            "input_bytes_each": input_bytes,
            "case": case,
            "trial": std::env::var("PRINTABLE_TEST_ASSEMBLY_TRIAL").unwrap(),
            "server_peak_before_kib": before,
            "server_peak_after_kib": peak_rss_kib(),
            "server_peak_growth_kib": peak_rss_kib().saturating_sub(before),
            "total_ms": total_ms,
            "preparation_and_process_overhead_ms": total_ms - measurement["worker_analysis_process_ms"].as_f64().unwrap(),
            "worker": measurement,
            "fixed_volume_mm3": report["fixed"]["volume_mm3"],
            "moving_volume_mm3": report["moving"]["volume_mm3"],
            "clearance_mm": static_report["clearance_mm"],
            "interference_volume_mm3": static_report["interference_volume_mm3"],
        })
    );
}

#[tokio::test]
async fn cancelled_waiter_keeps_assembly_snapshots_and_lane_owned() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    cube(&root.join("fixed.stl"), 2, 0.);
    cube(&root.join("moving.stl"), 2, 2.);
    let worker = temporary.path().join("controlled-worker");
    let root_json = serde_json::to_string(&root).unwrap();
    worker_script(
        &worker,
        &format!(
            r#"#!/usr/bin/env python3
import json, sys, time
from pathlib import Path
root = Path({root_json})
with root.joinpath('worker-starts').open('a') as marker:
    marker.write('started\n')
started = time.monotonic()
while not root.joinpath('worker-release').exists():
    if time.monotonic() - started > 5:
        sys.exit(2)
    time.sleep(0.005)
for path in map(Path, sys.argv[1:3]):
    assert path.is_file() and path.stat().st_size == 2484
print(json.dumps({{'report': {{'snapshots_available': True}}}}))
"#
        ),
    );
    let (server, workspace) = server(&root, &worker);
    let first = tokio::spawn({
        let server = server.clone();
        async move { server.invoke_tool(analysis_request()).await }
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !root.join("worker-starts").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("first worker starts");
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let preview = workspace.cleanup_preview().unwrap();
    assert_eq!(preview.len(), 2);
    assert!(preview.iter().all(|entry| entry.state == "protected"));
    let ids = preview
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    assert!(
        workspace
            .cleanup_scratch(&ids)
            .unwrap()
            .iter()
            .all(|entry| entry.state == "protected")
    );
    let second = tokio::spawn({
        let server = server.clone();
        async move { server.invoke_tool(analysis_request()).await }
    });
    // Give the second call an opportunity to acquire the lane. The first
    // worker still owns it even though its requesting future was cancelled.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!second.is_finished());
    assert_eq!(
        std::fs::read_to_string(root.join("worker-starts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    std::fs::write(root.join("worker-release"), b"release").unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("worker-starts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(
        workspace
            .storage_usage()
            .unwrap()
            .by_class
            .get("temporary")
            .copied()
            .unwrap_or(0),
        0
    );
}
