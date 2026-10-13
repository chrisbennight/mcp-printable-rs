//! Linux process-level shutdown checks with disposable workspaces and engines.
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

use printable_server::projects::{ProjectRequest, dispatch};
use printable_workspace::Workspace;
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(10);
const TEST_BEARER: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[derive(Clone, Copy, Debug)]
enum Service {
    Server,
    Cad,
    Recon,
    Slicer,
}

struct Fixture {
    directory: tempfile::TempDir,
    root: PathBuf,
    engine: PathBuf,
    started: PathBuf,
    release: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::open(Some(&root), None).unwrap();
        dispatch(
            &workspace,
            serde_json::from_value::<ProjectRequest>(json!({"action":"create","params":{
                "project_id":"part","name":"Shutdown fixture"
            }}))
            .unwrap(),
        )
        .unwrap();
        workspace
            .write_artifact("projects/part/source.py", b"retained source", false)
            .unwrap();
        workspace
            .write_artifact("projects/part/source.stl", b"retained mesh", false)
            .unwrap();
        workspace
            .write_artifact("photo.png", b"fake photograph", false)
            .unwrap();
        let started = directory.path().join("started");
        let release = directory.path().join("release");
        let engine = directory.path().join("engine.py");
        let script = format!(
            r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
if '--version' in sys.argv:
    print('2.6.1')
    sys.exit(0)
if '--help' in sys.argv:
    print('OrcaSlicer-2.4.2: --slice --export-3mf')
    sys.exit(0)
if '--check' in sys.argv:
    print(json.dumps({{'missing': []}}))
    sys.exit(0)
started = pathlib.Path({started})
release = pathlib.Path({release})
with started.open('a') as stream:
    stream.write(str(os.getpid()) + '\n')
deadline = time.monotonic() + 120
while not release.exists():
    if time.monotonic() >= deadline:
        sys.exit(4)
    time.sleep(0.01)
if '--outputdir' in sys.argv:
    output = pathlib.Path(sys.argv[sys.argv.index('--outputdir') + 1])
    (output / 'model.gcode.3mf').write_bytes(b'fake3mf')
    (output / 'plate_1.gcode').write_text('M83\n; CHANGE_LAYER\n; FEATURE: Outer wall\nG1 X10 E1\n')
else:
    request = pathlib.Path(sys.argv[-1])
    if request.is_dir():
        output = request / 'output'
        output.mkdir()
        for name in ['model.step', 'model.stl', 'model.glb', 'components.json']:
            (output / name).write_bytes(b'fake artifact')
        (output / 'report.json').write_text(json.dumps({{'valid': True, 'units': 'mm',
            'solid_count': 1, 'solid_volumes_mm3': [6000], 'bounds_mm': {{'size': [10,20,30]}}}}))
    else:
        spec = json.loads(request.read_text())
        pathlib.Path(spec['displacement']).write_bytes(b'fake displacement')
        print(json.dumps({{'result': {{'displacement': {{'offset_mm': 0, 'scale_mm': 1}}}}}}))
"#,
            started = serde_json::to_string(&started).unwrap(),
            release = serde_json::to_string(&release).unwrap(),
        );
        fs::write(&engine, script).unwrap();
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o700)).unwrap();
        for (category, name, fields) in [
            (
                "machine",
                "printer",
                json!({"machine_start_gcode":"G28","nozzle_diameter":["0.4"]}),
            ),
            (
                "process",
                "process",
                json!({"compatible_printers":["printer"],"layer_height":"0.2"}),
            ),
            (
                "filament",
                "filament",
                json!({"compatible_printers":["printer"],"filament_type":["PLA"],
                "textured_plate_temp":["65"],"textured_plate_temp_initial_layer":["65"]}),
            ),
        ] {
            let path = directory.path().join("profiles").join(category);
            fs::create_dir_all(&path).unwrap();
            let mut profile = fields;
            profile["type"] = json!(category);
            profile["name"] = json!(name);
            profile["instantiation"] = json!("true");
            fs::write(
                path.join("profile.json"),
                serde_json::to_vec(&profile).unwrap(),
            )
            .unwrap();
        }
        fs::write(
            directory
                .path()
                .join("profiles/filament/filaments_color_codes.json"),
            b"{\"data\":[],\"total\":0}",
        )
        .unwrap();
        fs::write(directory.path().join("landmarker"), b"fake landmarker").unwrap();
        Self {
            directory,
            root,
            engine,
            started,
            release,
        }
    }

    fn engine_pids(&self) -> Vec<i32> {
        fs::read_to_string(&self.started)
            .unwrap_or_default()
            .lines()
            .map(|pid| pid.parse().unwrap())
            .collect()
    }

    fn engine_is_running(&self) -> bool {
        self.engine_pids().iter().any(|pid| {
            let command = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            command
                .split(|byte| *byte == 0)
                .any(|arg| arg == self.engine.as_os_str().as_encoded_bytes())
        })
    }

    fn kill_owned_engines(&self) {
        for pid in self.engine_pids() {
            let command = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            if command
                .split(|byte| *byte == 0)
                .any(|arg| arg == self.engine.as_os_str().as_encoded_bytes())
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Forced host-process termination cannot run a child's cleanup code.
        // Only kill fixture engines whose current argv still proves ownership.
        self.kill_owned_engines();
    }
}

struct Process {
    child: Child,
    base: String,
    log: PathBuf,
}

impl Process {
    async fn start(service: Service, fixture: &Fixture) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let executable = match service {
            Service::Server => env!("CARGO_BIN_EXE_printable-server"),
            Service::Cad => env!("CARGO_BIN_EXE_printable-cad-worker"),
            Service::Recon => env!("CARGO_BIN_EXE_printable-recon-worker"),
            Service::Slicer => env!("CARGO_BIN_EXE_printable-slicer-worker"),
        };
        let log = fixture
            .directory
            .path()
            .join(format!("{service:?}-{address}.log"));
        let output = fs::File::create(&log).unwrap();
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("PRINTABLE_WORKSPACE_ROOT", &fixture.root)
            .env("PRINTABLE_MCP_BEARER", TEST_BEARER)
            .env("PRINTABLE_HTTP_HOST", "127.0.0.1")
            .env("PRINTABLE_HTTP_PORT", address.port().to_string())
            .env("PRINTABLE_ALLOWED_HOSTS", "127.0.0.1,localhost")
            .env("PRINTABLE_CAD_LISTEN", address.to_string())
            .env("PRINTABLE_RECON_LISTEN", address.to_string())
            .env("PRINTABLE_SLICER_LISTEN", address.to_string())
            .env("PRINTABLE_CAD_PYTHON", "/usr/bin/python3")
            .env("PRINTABLE_CAD_SCRIPT", &fixture.engine)
            .env("PRINTABLE_RECON_PYTHON", "/usr/bin/python3")
            .env("PRINTABLE_RECON_SCRIPT", &fixture.engine)
            .env(
                "PRINTABLE_RECON_LANDMARKER",
                fixture.directory.path().join("landmarker"),
            )
            .env("PRINTABLE_RECON_TIMEOUT_SECONDS", "30")
            .env("PRINTABLE_SLICER_BIN", &fixture.engine)
            .env(
                "PRINTABLE_SLICER_PROFILES",
                fixture.directory.path().join("profiles"),
            )
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .process_group(0);
        let mut process = Self {
            child: command.spawn().unwrap(),
            base: format!("http://{address}"),
            log,
        };
        let http = client();
        tokio::time::timeout(WAIT, async {
            loop {
                assert!(
                    process.child.try_wait().unwrap().is_none(),
                    "startup failed: {}",
                    process.logs()
                );
                if http
                    .get(format!("{}/healthz", process.base))
                    .send()
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("service becomes live");
        process
    }

    fn logs(&self) -> String {
        fs::read_to_string(&self.log).unwrap()
    }

    fn signal(&mut self, signal: i32) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "service already stopped"
        );
        assert_eq!(
            unsafe { libc::kill(self.child.id().try_into().unwrap(), signal) },
            0
        );
    }

    async fn wait(&mut self) -> ExitStatus {
        tokio::time::timeout(WAIT, async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("service did not stop: {}", self.logs()))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            self.child.wait().unwrap();
        }
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(WAIT)
        .build()
        .unwrap()
}

async fn wait_for(predicate: impl Fn() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture reaches expected state");
}

async fn post(base: &str, path: &str, request: Value) -> Value {
    let response = client()
        .post(format!("{base}/{path}"))
        .json(&request)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let result: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{status}: {result}");
    result
}

#[tokio::test]
async fn idle_services_exit_successfully_on_container_and_interactive_stop_signals() {
    for service in [
        Service::Server,
        Service::Cad,
        Service::Recon,
        Service::Slicer,
    ] {
        for signal in [libc::SIGTERM, libc::SIGINT] {
            let fixture = Fixture::new();
            let mut process = Process::start(service, &fixture).await;
            process.signal(signal);
            let status = process.wait().await;
            assert!(
                status.success(),
                "{service:?} signal {signal}: {status} {}",
                process.logs()
            );
            assert!(process.logs().contains("shutdown requested"));
            assert!(!fixture.engine_is_running());
        }
    }
}

#[tokio::test]
async fn foreground_native_requests_drain_and_stop_new_connections() {
    for service in [Service::Cad, Service::Recon] {
        let fixture = Fixture::new();
        let mut process = Process::start(service, &fixture).await;
        let (path, request) = match service {
            Service::Cad => (
                "build",
                json!({"action":"model","params":{"project_id":"part","source":"source.py","output_dir":"completed"}}),
            ),
            Service::Recon => (
                "reconstruct",
                json!({"images":["photo.png"],"displacement_path":"map.png"}),
            ),
            _ => unreachable!(),
        };
        let base = process.base.clone();
        let response = tokio::spawn(async move { post(&base, path, request).await });
        wait_for(|| !fixture.engine_pids().is_empty()).await;
        process.signal(libc::SIGTERM);
        wait_for(|| process.logs().contains("shutdown requested")).await;
        wait_for(|| {
            std::net::TcpStream::connect(process.base.strip_prefix("http://").unwrap()).is_err()
        })
        .await;
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "accepted request is still active"
        );
        fs::write(&fixture.release, b"complete").unwrap();
        let result = response.await.unwrap();
        assert!(process.wait().await.success(), "{}", process.logs());
        wait_for(|| !fixture.engine_is_running()).await;
        if matches!(service, Service::Cad) {
            assert_eq!(result["completion"], "completed");
            let saved = fs::read(fixture.root.join("projects/part/completed/report.json")).unwrap();
            let mut restarted = Process::start(service, &fixture).await;
            let state = post(
                &restarted.base,
                "build",
                json!({"action":"status","params":{"project_id":"part","output_dir":"completed"}}),
            )
            .await;
            assert_eq!(state["status"], "completed");
            assert_eq!(
                fixture.engine_pids().len(),
                1,
                "restart must not replay the build"
            );
            assert_eq!(
                fs::read(fixture.root.join("projects/part/completed/report.json")).unwrap(),
                saved
            );
            restarted.signal(libc::SIGTERM);
            assert!(restarted.wait().await.success());
        } else {
            assert_eq!(
                fs::read(fixture.root.join("map.png")).unwrap(),
                b"fake displacement"
            );
        }
    }
}

#[tokio::test]
async fn stopped_background_work_is_interrupted_after_restart_without_replay() {
    for service in [Service::Cad, Service::Slicer] {
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            let fixture = Fixture::new();
            let mut process = Process::start(service, &fixture).await;
            let (path, request) = match service {
                Service::Cad => (
                    "build",
                    json!({"action":"model","params":{"project_id":"part","source":"source.py","output_dir":"unfinished","background":true}}),
                ),
                Service::Slicer => (
                    "slice",
                    json!({"action":"prepare","params":{"project_id":"part","source":"source.stl","output_dir":"unfinished","build_plate":"Textured PEI Plate",
                    "printer":{"name":"printer"},"process":{"name":"process"},"filaments":[{"name":"filament"}]}}),
                ),
                _ => unreachable!(),
            };
            let admitted = post(&process.base, path, request).await;
            assert!(matches!(
                admitted["status"].as_str(),
                Some("admitted" | "running")
            ));
            wait_for(|| !fixture.engine_pids().is_empty()).await;
            process.signal(signal);
            let status = process.wait().await;
            if signal == libc::SIGTERM {
                assert!(status.success(), "{}", process.logs());
                wait_for(|| !fixture.engine_is_running()).await;
            } else {
                assert_eq!(status.signal(), Some(libc::SIGKILL));
                // Emulate the container's forced cleanup only for owned engines.
                fixture.kill_owned_engines();
                wait_for(|| !fixture.engine_is_running()).await;
            }
            let retained = fixture.root.join("projects/part/unfinished");
            assert!(!retained.join("report.json").exists());
            let request_bytes = fs::read(retained.join("request.json")).unwrap();
            let source = retained.join(if matches!(service, Service::Cad) {
                "inputs/source.py"
            } else {
                "source.stl"
            });
            let source_bytes = fs::read(&source).unwrap();
            let mut restarted = Process::start(service, &fixture).await;
            let state = post(
                &restarted.base,
                path,
                json!({"action":"status","params":{"project_id":"part","output_dir":"unfinished"}}),
            )
            .await;
            assert_eq!(state["status"], "interrupted", "{state}");
            assert_eq!(
                fixture.engine_pids().len(),
                1,
                "restart must not replay admitted work"
            );
            assert_eq!(
                fs::read(retained.join("request.json")).unwrap(),
                request_bytes
            );
            assert_eq!(fs::read(&source).unwrap(), source_bytes);
            assert!(!retained.join("report.json").exists());
            restarted.signal(libc::SIGTERM);
            assert!(restarted.wait().await.success());
        }
    }
}
