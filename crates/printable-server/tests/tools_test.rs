//! Behavioural coverage for the tool catalog, driving `tools::dispatch` directly
//! (no MCP wire framing): workspace list/read/write round-trip, the single-shot
//! write cap, the chunked-upload round-trip and its bounds (unknown upload,
//! oversized chunk, large streamed artifact, concurrent-upload cap), and
//! `printable_status` against a FakeAddon (up) and a closed port (down).
//!
//! Side-effect free: workspaces are tempdirs, Blender is faked or absent, and
//! all sockets bind loopback.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use image::{ImageFormat, Rgb, RgbImage};
use rmcp::model::{CallToolRequestParams, ContentBlock};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use printable_blender::fake_addon::{FakeAddon, ResponseSpec};
use printable_blender::{BlenderClient, ClientOptions};
use printable_server::config::{BearerSecret, Settings};
use printable_server::error::ToolError;
use printable_server::mcp::PrintableServer;
use printable_server::tools::{TOOLS, dispatch, lookup};
use printable_server::upload::{CHUNK_MAX_DECODED, MAX_CONCURRENT_UPLOADS, UploadRegistry};
use printable_workspace::{MAX_TRANSFER_BYTES, Workspace, WsError};

#[path = "../src/bin/smoke/workflow.rs"]
mod workflow;

struct WorkflowCall(&'static str);

fn workflow_call(operation: &'static str) -> WorkflowCall {
    WorkflowCall(operation)
}

impl WorkflowCall {
    fn with_arguments(self, arguments: serde_json::Map<String, Value>) -> CallToolRequestParams {
        let (name, arguments) = workflow::request(self.0, Value::Object(arguments)).unwrap();
        CallToolRequestParams::new(name).with_arguments(arguments.as_object().unwrap().clone())
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode(data_base64: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(data_base64.as_str().expect("data_base64 is a string"))
        .expect("valid base64")
}

fn rgb_png(width: u32, height: u32, color: Rgb<u8>) -> Vec<u8> {
    let image = RgbImage::from_pixel(width, height, color);
    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut output, ImageFormat::Png)
        .expect("encode test PNG");
    output.into_inner()
}

fn rgb_png_with_total_size(width: u32, height: u32, total_size: usize) -> Vec<u8> {
    let png = rgb_png(width, height, Rgb([20, 80, 160]));
    let payload_len = total_size
        .checked_sub(png.len() + 12)
        .expect("requested PNG size leaves room for an ancillary chunk");
    let iend_offset = png.len() - 12;
    let mut output = Vec::with_capacity(total_size);
    output.extend_from_slice(&png[..iend_offset]);
    output.extend_from_slice(
        &u32::try_from(payload_len)
            .expect("test ancillary chunk fits PNG length")
            .to_be_bytes(),
    );
    output.extend_from_slice(b"vpAg");
    output.resize(output.len() + payload_len, 0);
    let mut crc = crc32fast::Hasher::new();
    crc.update(&output[iend_offset + 4..]);
    output.extend_from_slice(&crc.finalize().to_be_bytes());
    output.extend_from_slice(&png[iend_offset..]);
    assert_eq!(output.len(), total_size);
    output
}

fn binary_stl(vertices: &[[f32; 3]], triangles: &[[usize; 3]]) -> Vec<u8> {
    let mut bytes = vec![0; 80];
    bytes.extend_from_slice(&(triangles.len() as u32).to_le_bytes());
    for triangle in triangles {
        for component in [0.0_f32; 3] {
            bytes.extend_from_slice(&component.to_le_bytes());
        }
        for index in triangle {
            for component in vertices[*index] {
                bytes.extend_from_slice(&component.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
    }
    bytes
}

fn cube_stl(size: f32) -> Vec<u8> {
    cube_stl_at(size, [0.0; 3])
}

fn cube_stl_at(size: f32, offset: [f32; 3]) -> Vec<u8> {
    let [x, y, z] = offset;
    let vertices = [
        [x, y, z],
        [x + size, y, z],
        [x + size, y + size, z],
        [x, y + size, z],
        [x, y, z + size],
        [x + size, y, z + size],
        [x + size, y + size, z + size],
        [x, y + size, z + size],
    ];
    binary_stl(
        &vertices,
        &[
            [0, 2, 1],
            [0, 3, 2],
            [4, 5, 6],
            [4, 6, 7],
            [0, 1, 5],
            [0, 5, 4],
            [1, 2, 6],
            [1, 6, 5],
            [2, 3, 7],
            [2, 7, 6],
            [3, 0, 4],
            [3, 4, 7],
        ],
    )
}

fn bounds_json(minimum: [f64; 3], maximum: [f64; 3]) -> Value {
    let dimensions = [
        maximum[0] - minimum[0],
        maximum[1] - minimum[1],
        maximum[2] - minimum[2],
    ];
    let center = [
        (minimum[0] + maximum[0]) * 0.5,
        (minimum[1] + maximum[1]) * 0.5,
        (minimum[2] + maximum[2]) * 0.5,
    ];
    json!({
        "minimum": minimum,
        "maximum": maximum,
        "dimensions": dimensions,
        "center": center,
        "diagonal": dimensions.iter().map(|value| value * value).sum::<f64>().sqrt(),
        "coordinate_space": "world",
        "unit": "blender_unit",
    })
}

fn render_bounds_json() -> Value {
    bounds_json([-10.0, -20.0, -30.0], [10.0, 20.0, 30.0])
}

fn product_view_presentation_json(presentation: &Value, view: &Value, bounds: &Value) -> Value {
    let profile = presentation["profile"].as_str().expect("profile");
    let direction = view["direction"].as_array().expect("view direction");
    let direction = [
        direction[0].as_f64().expect("direction x"),
        direction[1].as_f64().expect("direction y"),
        direction[2].as_f64().expect("direction z"),
    ];
    let magnitude = direction[0].hypot(direction[1]).hypot(direction[2]);
    let shading = presentation
        .get("surface_shading")
        .and_then(Value::as_str)
        .unwrap_or(if profile == "engineering" {
            "preserve"
        } else {
            "smooth_by_angle"
        });
    let (camera_type, lens, ortho_scale, sensor_width) = if profile == "engineering" {
        ("orthographic", Value::Null, json!(80.0), Value::Null)
    } else {
        (
            "perspective",
            json!(if profile == "studio_neutral" {
                70.0
            } else {
                85.0
            }),
            Value::Null,
            json!(36.0),
        )
    };
    json!({
        "profile": profile,
        "camera": {
            "type": camera_type,
            "behavior": "profile",
            "azimuth_degrees": direction[1].atan2(direction[0]).to_degrees(),
            "elevation_degrees": (direction[2] / magnitude).clamp(-1.0, 1.0).asin().to_degrees(),
            "position": [60.0, 60.0, 60.0],
            "target": bounds["center"],
            "lens_mm": lens,
            "ortho_scale": ortho_scale,
            "sensor_width_mm": sensor_width,
            "clip_start": 0.01,
            "clip_end": 1000.0
        },
        "materials": {
            "overrides": presentation
                .get("materials")
                .cloned()
                .unwrap_or_else(|| json!([]))
        },
        "shading": {
            "mode": shading,
            "angle_degrees": if shading == "smooth_by_angle" {
                Some(30.0)
            } else {
                None
            },
            "presentation_only": true
        },
        "framing": {
            "margin_percent": 15.0,
            "bounds": bounds,
            "instance_count": 1
        },
        "source_state_verified": true,
        "cleanup_verified": true
    })
}

async fn render_views_fake(
    root: std::path::PathBuf,
) -> (FakeAddon, Arc<std::sync::Mutex<Vec<Value>>>) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let fake = FakeAddon::spawn(move |command, params| {
        assert_eq!(command, "render_views");
        captured
            .lock()
            .expect("request capture lock")
            .push(params.clone());
        let width = params["width"].as_u64().expect("width");
        let height = params["height"].as_u64().expect("height");
        let rendered = params["views"]
            .as_array()
            .expect("views array")
            .iter()
            .enumerate()
            .map(|(index, view)| {
                let path = view["path"].as_str().expect("view path");
                let bytes = rgb_png(
                    width as u32,
                    height as u32,
                    Rgb([50u8.wrapping_add((index as u8).wrapping_mul(5)), 20, 180]),
                );
                let destination = root.join(path);
                std::fs::create_dir_all(destination.parent().expect("view parent"))
                    .expect("create view parent");
                std::fs::write(&destination, &bytes).expect("write rendered view");
                json!({
                    "path": path,
                    "label": view["label"],
                    "size_bytes": bytes.len(),
                    "media_type": "image/png",
                    "width": width,
                    "height": height,
                })
            })
            .collect::<Vec<_>>();
        let cycles = params["engine"] == "CYCLES";
        let bounds = render_bounds_json();
        let mut result = json!({
            "views": rendered,
            "engine": if cycles { "CYCLES" } else { "BLENDER_EEVEE_NEXT" },
            "render_device": if cycles { "CPU" } else { "GRAPHICS" },
            "graphics_backend": null,
            "samples": if cycles {
                params.get("samples").cloned().unwrap_or(json!(128))
            } else {
                Value::Null
            },
            "bounds": bounds,
        });
        if let Some(presentation) = params.get("presentation") {
            result["presentation"] = json!({
                "profile": presentation["profile"],
                "views": params["views"]
                    .as_array()
                    .expect("views")
                    .iter()
                    .map(|view| product_view_presentation_json(presentation, view, &bounds))
                    .collect::<Vec<_>>(),
            });
        }
        printable_blender::fake_addon::ResponseSpec::Success {
            result,
            addon_version: Some("0.2.5".to_string()),
        }
    })
    .await;
    (fake, requests)
}

fn diagnostic_result(root: &std::path::Path, params: &Value) -> Value {
    let path = params["path"].as_str().expect("diagnostic path");
    let width = params["width"].as_u64().expect("width");
    let height = params["height"].as_u64().expect("height");
    let bytes = rgb_png(width as u32, height as u32, Rgb([220, 40, 20]));
    let destination = root.join(path);
    std::fs::create_dir_all(destination.parent().expect("diagnostic parent"))
        .expect("create diagnostic parent");
    std::fs::write(&destination, &bytes).expect("write diagnostic source");
    let mode = params["mode"].as_str().expect("diagnostic mode");
    let source_bounds = render_bounds_json();
    let (analysis, rendered_bounds) = if mode == "cross_section" {
        let axis = params["axis"].as_str().expect("cross-section axis");
        let axis_index = match axis {
            "X" => 0,
            "Y" => 1,
            "Z" => 2,
            other => panic!("unexpected cross-section axis: {other}"),
        };
        let position = params
            .get("position")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let mut rendered_maximum = [10.0, 20.0, 30.0];
        rendered_maximum[axis_index] = position;
        (
            json!({
            "axis": params["axis"],
            "position": position,
            "source_instances": 2,
            "evaluated_vertices": 16,
            "evaluated_edges": 24,
            "evaluated_faces": 12,
            "evaluated_loops": 36,
            "copied_attribute_values": 100,
            "section_faces": 2,
            "section_area": 800.0,
            }),
            bounds_json([-10.0, -20.0, -30.0], rendered_maximum),
        )
    } else {
        (
            json!({
                "build_direction": params["build_direction"],
                "overhang_angle_degrees": params["overhang_angle_degrees"],
                "source_instances": 2,
                "evaluated_vertices": 16,
                "evaluated_edges": 24,
                "evaluated_faces": 12,
                "evaluated_loops": 36,
                "copied_attribute_values": 100,
                "categories": {
                    "supported": {"faces": 6, "area": 1000.0},
                    "warning": {"faces": 4, "area": 500.0},
                    "severe": {"faces": 2, "area": 250.0},
                },
            }),
            source_bounds.clone(),
        )
    };
    let mut objects = params.get("objects").cloned().unwrap_or(Value::Null);
    if let Some(objects) = objects.as_array_mut() {
        objects.sort_by(|left, right| {
            left.as_str()
                .expect("object name")
                .cmp(right.as_str().expect("object name"))
        });
    }
    json!({
        "path": path,
        "size_bytes": bytes.len(),
        "media_type": "image/png",
        "width": width,
        "height": height,
        "engine": "BLENDER_EEVEE_NEXT",
        "render_device": "GRAPHICS",
        "graphics_backend": null,
        "samples": null,
        "mode": mode,
        "objects": objects,
        "source_bounds": source_bounds,
        "rendered_bounds": rendered_bounds,
        "analysis": analysis,
    })
}

async fn render_diagnostic_fake(
    root: std::path::PathBuf,
) -> (FakeAddon, Arc<std::sync::Mutex<Vec<Value>>>) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let fake = FakeAddon::spawn(move |command, params| {
        assert_eq!(command, "render_diagnostic");
        captured
            .lock()
            .expect("request capture lock")
            .push(params.clone());
        printable_blender::fake_addon::ResponseSpec::Success {
            result: diagnostic_result(&root, &params),
            addon_version: Some("0.2.5".to_string()),
        }
    })
    .await;
    (fake, requests)
}

fn test_geometry_worker() -> PathBuf {
    let worker = option_env!("CARGO_BIN_EXE_printable-geometry-worker")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let executable = std::env::current_exe().expect("integration test executable");
            let dependencies = executable.parent().expect("test dependency directory");
            assert_eq!(
                dependencies.file_name().and_then(|name| name.to_str()),
                Some("deps")
            );
            dependencies
                .parent()
                .expect("Cargo profile directory")
                .join(format!(
                    "printable-geometry-worker{}",
                    std::env::consts::EXE_SUFFIX
                ))
        });
    assert!(
        worker.is_file(),
        "Cargo must build the geometry worker for integration tests: {}",
        worker.display()
    );
    worker
}

fn settings(blender_host: &str, blender_port: u16) -> Settings {
    Settings {
        http_host: "127.0.0.1".to_string(),
        http_port: 0,
        blender_host: blender_host.to_string(),
        blender_port,
        render_worker_host: None,
        render_worker_port: 9876,
        workspace_root: None,
        workspace_budget_bytes: None,
        blender_workspace_root: None,
        openscad_bin: None,
        cad_endpoint: None,
        slicer_endpoint: None,
        printers: None,
        flame: None,
        recon: None,
        scad_concurrency: 2,
        ffmpeg_bin: PathBuf::from("ffmpeg"),
        render_job_queue_depth: 16,
        geometry_worker_bin: Some(test_geometry_worker()),
        geometry_worker_memory_bytes: 1024 * 1024 * 1024,
        file_upload_max_bytes: 1024 * 1024 * 1024,
        mcp_bearer: BearerSecret::parse(
            concat!(
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef"
            )
            .to_string(),
        )
        .expect("test bearer is valid"),
        allowed_hosts: Vec::new(),
        allowed_origins: Vec::new(),
        download_base_url: None,
    }
}

fn settings_with_openscad(binary: PathBuf) -> Settings {
    let mut config = settings("127.0.0.1", 9);
    config.openscad_bin = Some(binary);
    config
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

struct FakeOpenScad {
    _directory: tempfile::TempDir,
    binary: PathBuf,
    captured_first_source: PathBuf,
    captured_source: PathBuf,
    captured_caller: PathBuf,
    captured_kit: PathBuf,
    captured_args: PathBuf,
}

fn fake_openscad() -> FakeOpenScad {
    let directory = tempfile::tempdir().expect("fake OpenSCAD tempdir");
    let stl_fixture = directory.path().join("cube.stl");
    let png_fixture = directory.path().join("render.png");
    let captured_first_source = directory.path().join("captured-first.scad");
    let captured_source = directory.path().join("captured.scad");
    let captured_caller = directory.path().join("captured-caller.scad");
    let captured_kit = directory.path().join("captured-product-v1.scad");
    let captured_args = directory.path().join("captured-args.txt");
    std::fs::write(&stl_fixture, cube_stl(10.0)).expect("write STL fixture");
    std::fs::write(&png_fixture, rgb_png(32, 32, Rgb([20, 80, 160]))).expect("write PNG fixture");

    let binary = directory.path().join("fake-openscad");
    let script = format!(
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" >> {}\noutput=\nsource=\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    -o) output=$2; shift 2 ;;\n    -D|--camera|--imgsize) shift 2 ;;\n    --preview|--render|--autocenter|--viewall|--colorscheme=*) shift ;;\n    *) source=$1; shift ;;\n  esac\ndone\nif [ ! -f {} ]; then cp \"$source\" {}; fi\ncp \"$source\" {}\nsource_dir=${{source%/*}}\nif [ -f \"$source_dir/caller.scad\" ]; then cp \"$source_dir/caller.scad\" {}; fi\nif [ -f \"$source_dir/product_v1.scad\" ]; then cp \"$source_dir/product_v1.scad\" {}; fi\ncase \"$output\" in\n  *.stl) cp {} \"$output\" ;;\n  *.png) cp {} \"$output\" ;;\n  *.svg) printf '%s' '<svg xmlns=\"http://www.w3.org/2000/svg\"><path d=\"M0 0L10 0L10 10Z\"/></svg>' > \"$output\" ;;\n  *) exit 9 ;;\nesac\nprintf '%s' 'fake-openscad'\n",
        shell_quote(&captured_args),
        shell_quote(&captured_first_source),
        shell_quote(&captured_first_source),
        shell_quote(&captured_source),
        shell_quote(&captured_caller),
        shell_quote(&captured_kit),
        shell_quote(&stl_fixture),
        shell_quote(&png_fixture),
    );
    std::fs::write(&binary, script).expect("write fake OpenSCAD");
    let mut permissions = std::fs::metadata(&binary)
        .expect("fake OpenSCAD metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&binary, permissions).expect("make fake OpenSCAD executable");
    FakeOpenScad {
        _directory: directory,
        binary,
        captured_first_source,
        captured_source,
        captured_caller,
        captured_kit,
        captured_args,
    }
}

fn product_design_profile(maximum_overhang_degrees: f64) -> Value {
    json!({
        "kit": "product_v1",
        "manufacturing": {
            "nozzle_diameter_mm": 0.4,
            "layer_height_mm": 0.2,
            "minimum_wall_mm": 2.0,
            "moving_clearance_mm": 0.35,
            "maximum_overhang_degrees": maximum_overhang_degrees,
        },
        "form": {
            "primary_radius_mm": 3.0,
            "secondary_radius_mm": 1.5,
            "edge_break_mm": 0.5,
            "transition_length_mm": 8.0,
        },
    })
}

fn client(host: &str, port: u16) -> BlenderClient {
    BlenderClient::new(host.to_string(), port, ClientOptions::default())
}

/// An `Arc`-wrapped workspace — `dispatch` takes `&Arc<Workspace>`.
fn workspace(root: Option<&std::path::Path>) -> Arc<Workspace> {
    Arc::new(Workspace::open(root, None).expect("open workspace"))
}

/// A fresh chunked-upload registry — `dispatch` takes `&Arc<UploadRegistry>`.
/// One registry must be reused across a begin/chunk/commit sequence.
fn uploads() -> Arc<UploadRegistry> {
    Arc::new(UploadRegistry::new())
}

/// A loopback port that is reserved then released, so a connect is refused fast.
async fn closed_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve a port");
    l.local_addr().expect("addr").port()
}

#[test]
fn catalog_schemas_defaults_and_annotations_are_explicit() {
    for tool in TOOLS {
        let schema = serde_json::to_value((tool.schema)()).expect("schema serializes");
        assert_eq!(schema["type"], json!("object"), "tool={}", tool.name);
    }

    let list = lookup("printable_workspace_list").expect("workspace list tool");
    let list_schema = serde_json::to_value((list.schema)()).expect("list schema serializes");
    assert_eq!(list_schema["properties"]["path"]["default"], json!("."));
    assert_eq!(list_schema["properties"]["limit"]["default"], json!(1000));

    let publish = lookup("printable_workspace_publish").expect("workspace publish tool");
    let publish_schema =
        serde_json::to_value((publish.schema)()).expect("publish schema serializes");
    assert_eq!(publish_schema["required"], json!(["path"]));
    assert_eq!((publish.annotations)().read_only_hint, Some(true));
    assert_eq!((publish.annotations)().destructive_hint, Some(false));
    assert_eq!((publish.annotations)().idempotent_hint, Some(false));
    assert_eq!((publish.annotations)().open_world_hint, Some(false));

    let scene = lookup("printable_scene_get").expect("scene tool");
    let scene_schema = serde_json::to_value((scene.schema)()).expect("scene schema serializes");
    assert_eq!(scene_schema["properties"]["limit"]["default"], json!(100));
    let validator = jsonschema::validator_for(&scene_schema).unwrap();
    for limit in [3000_usize, usize::MAX] {
        assert!(validator.is_valid(&json!({"limit":limit})));
    }
    assert!(!validator.is_valid(&json!({"limit":0})));
    assert_eq!((scene.annotations)().read_only_hint, Some(true));
    assert_eq!((scene.annotations)().destructive_hint, Some(false));
    assert_eq!((scene.annotations)().idempotent_hint, Some(true));
    assert_eq!((scene.annotations)().open_world_hint, Some(false));

    let clear = lookup("printable_scene_clear").expect("scene clear tool");
    assert_eq!((clear.annotations)().read_only_hint, Some(false));
    assert_eq!((clear.annotations)().destructive_hint, Some(true));
    assert_eq!((clear.annotations)().idempotent_hint, Some(false));
    assert_eq!((clear.annotations)().open_world_hint, Some(false));

    let primitive = lookup("printable_primitive_create").expect("primitive tool");
    let primitive_schema =
        serde_json::to_value((primitive.schema)()).expect("primitive schema serializes");
    for field in ["vertices", "segments", "ring_count"] {
        assert_eq!(
            primitive_schema["properties"][field]["minimum"],
            json!(3),
            "field={field}"
        );
    }

    let rotation = lookup("printable_rigid_rotation_animate").expect("rigid rotation tool");
    let rotation_schema =
        serde_json::to_value((rotation.schema)()).expect("rotation schema serializes");
    assert_eq!(
        rotation_schema["required"],
        json!([
            "objects",
            "controller_name",
            "pivot",
            "axis",
            "angle_degrees"
        ])
    );
    assert_eq!(
        rotation_schema["properties"]["objects"]["minItems"],
        json!(1)
    );
    assert_eq!(
        rotation_schema["properties"]["objects"]["maxItems"],
        Value::Null
    );
    assert_eq!(
        rotation_schema["properties"]["frame_start"]["default"],
        json!(1)
    );
    assert_eq!(
        rotation_schema["properties"]["frame_end"]["default"],
        json!(250)
    );
    assert_eq!((rotation.annotations)().read_only_hint, Some(false));
    assert_eq!((rotation.annotations)().destructive_hint, Some(true));
    assert_eq!((rotation.annotations)().idempotent_hint, Some(false));
    assert_eq!((rotation.annotations)().open_world_hint, Some(false));
    assert!(
        rotation
            .description
            .contains("preserves every target's world transform")
    );
    assert!(rotation.description.contains("parent or children"));
    assert!(rotation.description.contains("object animation data"));
    assert!(rotation.description.contains("rigid-body simulation"));

    let execute = lookup("printable_blender_execute").expect("execute tool");
    let execute_schema =
        serde_json::to_value((execute.schema)()).expect("execute schema serializes");
    assert_eq!(execute_schema["properties"]["code"]["minLength"], json!(1));
    assert_eq!(
        execute_schema["properties"]["timeout_seconds"]["default"],
        json!(120.0)
    );
    assert!(
        execute_schema["properties"]["timeout_seconds"]
            .get("maximum")
            .is_none()
    );
    assert_eq!((execute.annotations)().read_only_hint, Some(false));
    assert_eq!((execute.annotations)().destructive_hint, Some(true));
    assert_eq!((execute.annotations)().idempotent_hint, Some(false));
    assert_eq!((execute.annotations)().open_world_hint, Some(true));
    assert!(execute.description.contains("unknown mutation outcome"));
    assert!(execute.description.contains("never automatically retry"));

    let render = lookup("printable_render_preview").expect("render preview tool");
    let render_schema = serde_json::to_value((render.schema)()).expect("render schema serializes");
    assert_eq!(render_schema["properties"]["width"]["default"], json!(512));
    assert_eq!(render_schema["properties"]["width"]["maximum"], Value::Null);
    assert_eq!(render_schema["properties"]["height"]["default"], json!(512));
    assert_eq!(
        render_schema["properties"]["engine"]["default"],
        json!("EEVEE")
    );
    assert_eq!(
        render_schema["properties"]["timeout_seconds"]["default"],
        json!(3600.0)
    );
    assert!(
        render_schema["properties"]["timeout_seconds"]
            .get("maximum")
            .is_none()
    );
    assert_eq!(
        render_schema["properties"]["include_inline"]["default"],
        json!(true)
    );
    assert_eq!((render.annotations)().read_only_hint, Some(false));
    assert_eq!((render.annotations)().destructive_hint, Some(true));
    assert_eq!((render.annotations)().idempotent_hint, Some(false));
    assert_eq!((render.annotations)().open_world_hint, Some(false));
    assert!(render.description.contains("workspace artifacts"));

    let product = lookup("printable_render_product").expect("product render tool");
    let product_schema =
        serde_json::to_value((product.schema)()).expect("product render schema serializes");
    assert_eq!(
        product_schema["properties"]["width"]["default"],
        json!(1024)
    );
    assert_eq!(
        product_schema["properties"]["height"]["default"],
        json!(768)
    );
    assert_eq!(
        product_schema["properties"]["objects"]["minItems"],
        json!(1)
    );
    assert_eq!(
        product_schema["properties"]["presentation"]["$ref"],
        json!("#/$defs/ProductPresentation")
    );
    assert_eq!((product.annotations)().read_only_hint, Some(false));
    assert_eq!((product.annotations)().destructive_hint, Some(true));
    assert!(product.description.contains("fixed 15% bounds framing"));
    assert!(
        product
            .description
            .contains("never adds a render-only bevel")
    );

    let gallery = lookup("printable_render_gallery").expect("render gallery tool");
    let gallery_schema =
        serde_json::to_value((gallery.schema)()).expect("gallery schema serializes");
    assert_eq!(gallery_schema["properties"]["width"]["default"], json!(512));
    assert_eq!(
        gallery_schema["properties"]["views"]["maxItems"],
        Value::Null
    );
    assert_eq!(gallery_schema["properties"]["columns"]["default"], json!(3));
    assert_eq!(
        gallery_schema["properties"]["include_inline"]["default"],
        json!(true)
    );

    let dimensions = lookup("printable_render_dimensions").expect("dimensions tool");
    let dimensions_schema =
        serde_json::to_value((dimensions.schema)()).expect("dimensions schema serializes");
    assert_eq!(
        dimensions_schema["properties"]["width"]["default"],
        json!(512)
    );
    assert_eq!(
        dimensions_schema["properties"]["include_inline"]["default"],
        json!(true)
    );

    let cross_section = lookup("printable_render_cross_section").expect("cross-section tool");
    let cross_section_schema =
        serde_json::to_value((cross_section.schema)()).expect("cross-section schema serializes");
    assert_eq!(
        cross_section_schema["properties"]["axis"]["default"],
        json!("Z")
    );
    assert_eq!(
        cross_section_schema["properties"]["objects"]["maxItems"],
        Value::Null
    );
    assert_eq!(
        cross_section_schema["properties"]["timeout_seconds"]["default"],
        json!(3600.0)
    );

    let heatmap = lookup("printable_render_printability_heatmap").expect("heatmap tool");
    let heatmap_schema =
        serde_json::to_value((heatmap.schema)()).expect("heatmap schema serializes");
    assert_eq!(
        heatmap_schema["properties"]["build_direction"]["default"],
        json!([0.0, 0.0, 1.0])
    );
    assert!(
        heatmap_schema["properties"]["build_direction"]["description"]
            .as_str()
            .expect("build direction description")
            .contains("normalized")
    );
    assert_eq!(
        heatmap_schema["properties"]["overhang_angle_degrees"]["default"],
        json!(45.0)
    );
    assert_eq!(
        heatmap_schema["properties"]["overhang_angle_degrees"]["maximum"],
        json!(90.0)
    );
    assert_eq!(
        heatmap_schema["properties"]["overhang_angle_degrees"]["description"],
        json!(
            "Downward overhang angle in degrees; faces strictly above it receive warning or severe coloring (default 45)."
        )
    );

    let validate = lookup("printable_validate_mesh").expect("mesh validation tool");
    let validate_schema =
        serde_json::to_value((validate.schema)()).expect("validation schema serializes");
    assert_eq!(
        validate_schema["properties"]["build_direction"]["default"],
        json!([0.0, 0.0, 1.0])
    );
    assert_eq!(
        validate_schema["properties"]["overhang_angle_degrees"]["default"],
        json!(45.0)
    );
    assert_eq!((validate.annotations)().read_only_hint, Some(true));
    assert_eq!((validate.annotations)().destructive_hint, Some(false));
    assert_eq!((validate.annotations)().idempotent_hint, Some(true));
    assert!(validate.description.contains("actionable"));

    let assembly = lookup("printable_analyze_assembly").expect("assembly analysis tool");
    let assembly_schema =
        serde_json::to_value((assembly.schema)()).expect("assembly schema serializes");
    assert_eq!(
        assembly_schema["required"],
        json!(["fixed_path", "moving_path"])
    );
    assert_eq!(
        assembly_schema["properties"]["motion"]["anyOf"][0]["$ref"],
        json!("#/$defs/AssemblyMotionParams")
    );
    assert_eq!(
        assembly_schema["properties"]["rotation"]["anyOf"][0]["$ref"],
        json!("#/$defs/AssemblyRotationParams")
    );
    assert_eq!((assembly.annotations)().read_only_hint, Some(true));
    assert_eq!((assembly.annotations)().destructive_hint, Some(false));
    assert_eq!((assembly.annotations)().idempotent_hint, Some(true));
    assert!(assembly.description.contains("continuously sweep"));

    let scad_compile = lookup("printable_scad_compile").expect("OpenSCAD compile tool");
    let scad_compile_schema =
        serde_json::to_value((scad_compile.schema)()).expect("OpenSCAD compile schema serializes");
    assert_eq!(
        scad_compile_schema["properties"]["source"]["maxLength"],
        Value::Null
    );
    assert_eq!(
        scad_compile_schema["properties"]["timeout_seconds"]["default"],
        json!(3600.0)
    );
    assert_eq!(
        scad_compile_schema["properties"]["variant"]["maxLength"],
        Value::Null
    );
    assert_eq!(
        scad_compile_schema["$defs"]["ScadDefinitions"]["maxProperties"],
        Value::Null
    );
    assert_eq!(
        scad_compile_schema["properties"]["design_profile"]["anyOf"][0]["$ref"],
        json!("#/$defs/ProductDesignProfile")
    );
    assert_eq!(
        scad_compile_schema["$defs"]["ProductKit"]["enum"],
        json!(["product_v1"])
    );
    assert_eq!(
        scad_compile_schema["$defs"]["ProductDesignProfile"]["required"],
        json!(["kit", "manufacturing", "form"])
    );
    assert!(
        scad_compile_schema["properties"]["timeout_seconds"]
            .get("maximum")
            .is_none()
    );
    assert_eq!((scad_compile.annotations)().destructive_hint, Some(true));

    let scad_render = lookup("printable_scad_render").expect("OpenSCAD render tool");
    let scad_render_schema =
        serde_json::to_value((scad_render.schema)()).expect("OpenSCAD render schema serializes");
    assert_eq!(
        scad_render_schema["properties"]["view"]["default"],
        json!("iso")
    );
    assert_eq!(
        scad_render_schema["properties"]["size"]["default"],
        json!(512)
    );
    assert_eq!(
        scad_render_schema["properties"]["size"]["maximum"],
        Value::Null
    );
    assert_eq!(
        scad_render_schema["properties"]["preview"]["default"],
        json!(true)
    );
    assert_eq!(
        scad_render_schema["properties"]["include_inline"]["default"],
        json!(true)
    );

    let scad_cross = lookup("printable_scad_cross_section").expect("OpenSCAD cross-section tool");
    let scad_cross_schema = serde_json::to_value((scad_cross.schema)())
        .expect("OpenSCAD cross-section schema serializes");
    assert_eq!(
        scad_cross_schema["properties"]["z_mm"]["default"],
        json!(0.0)
    );
    assert_eq!((scad_cross.annotations)().destructive_hint, Some(true));

    let turntable = lookup("printable_render_turntable").expect("turntable tool");
    let turntable_schema =
        serde_json::to_value((turntable.schema)()).expect("turntable schema serializes");
    assert_eq!(
        turntable_schema["properties"]["frames"]["default"],
        json!(8)
    );
    assert_eq!(
        turntable_schema["properties"]["frames"]["maximum"],
        Value::Null
    );
    assert_eq!(
        turntable_schema["properties"]["elevation_degrees"]["default"],
        json!(20.0)
    );

    let compare = lookup("printable_compare_renders").expect("compare tool");
    let compare_schema =
        serde_json::to_value((compare.schema)()).expect("compare schema serializes");
    assert_eq!(
        compare_schema["properties"]["panel_width"]["maximum"],
        Value::Null
    );
    assert_eq!((compare.annotations)().destructive_hint, Some(true));
}

#[tokio::test]
async fn scad_compile_snapshots_imports_and_returns_validated_stl() {
    let tmp = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace(Some(tmp.path()));
    ws.write_artifact("imports/part.stl", &cube_stl(2.0), false)
        .expect("seed imported STL");
    ws.write_artifact("models/compiled.stl", &cube_stl(1.0), false)
        .expect("seed destination to replace");
    let fake = fake_openscad();
    let config = settings_with_openscad(fake.binary.clone());

    let result = dispatch(
        &ws,
        &uploads(),
        &client("127.0.0.1", 9),
        &config,
        "printable_scad_compile",
        json!({
            "source": "import(\"imports/part.stl\");",
            "path": "models/compiled.stl",
            "overwrite": true,
            "timeout_seconds": 5.0,
        }),
    )
    .await
    .expect("compile succeeds");

    assert_eq!(result["artifact"]["path"], json!("models/compiled.stl"));
    assert_eq!(result["validation"]["printable"], json!(true));
    assert_eq!(
        result["validation"]["assessment"]["status"],
        json!("incomplete")
    );
    let schema = printable_server::tools::output::schema("scad_build");
    jsonschema::validator_for(&Value::Object(schema.as_ref().clone()))
        .unwrap()
        .validate(&result)
        .unwrap();
    assert_eq!(
        result["validation"]["solid_properties"]["volume_mm3"],
        json!(1000.0)
    );
    assert_eq!(result["diagnostics"]["stdout"], json!("fake-openscad"));
    let confined_source =
        std::fs::read_to_string(&fake.captured_source).expect("read confined source");
    assert!(!confined_source.contains("imports/part.stl"));
    assert!(confined_source.contains("import(\"/"), "{confined_source}");
    let (_, artifact) = ws
        .read_artifact("models/compiled.stl")
        .expect("read compiled STL");
    assert_eq!(artifact, cube_stl(10.0));
}

#[tokio::test]
async fn scad_definitions_are_identical_across_compile_render_and_cross_section() {
    let cases = [
        ("printable_scad_compile", "models/defined.stl", json!({})),
        (
            "printable_scad_render",
            "renders/defined.png",
            json!({"size": 32}),
        ),
        (
            "printable_scad_cross_section",
            "sections/defined.svg",
            json!({"z_mm": 1.0}),
        ),
    ];

    for (tool, path, extras) in cases {
        let tmp = tempfile::tempdir().expect("workspace tempdir");
        let ws = workspace(Some(tmp.path()));
        let fake = fake_openscad();
        let config = settings_with_openscad(fake.binary.clone());
        let mut arguments = json!({
            "source": "width = 1; cube([width, width, width]);",
            "path": path,
            "defines": {
                "width": 70.0,
                "enabled": true,
                "label": "sentinel-secret \"A\" \\\\ path",
                "samples": [1.0, 2.0, 3.0]
            },
            "variant": "print",
            "timeout_seconds": 5.0
        });
        arguments
            .as_object_mut()
            .expect("arguments object")
            .extend(extras.as_object().expect("extras object").clone());

        let result = dispatch(
            &ws,
            &uploads(),
            &client("127.0.0.1", 9),
            &config,
            tool,
            arguments,
        )
        .await
        .expect("OpenSCAD workflow succeeds");

        assert_eq!(
            result["definitions"],
            json!({
                "count": 5,
                "names": ["enabled", "label", "pbl_variant", "samples", "width"],
                "variant_applied": true
            }),
            "tool={tool}"
        );
        let captured = std::fs::read_to_string(&fake.captured_args).expect("read captured argv");
        for expected in [
            "enabled=true",
            "label=\"sentinel-secret \\\"A\\\" \\\\\\\\ path\"",
            "pbl_variant=\"print\"",
            "samples=[1,2,3]",
            "width=70",
        ] {
            assert!(
                captured.lines().any(|line| line == expected),
                "{tool}: {captured}"
            );
        }
        assert_eq!(
            captured.lines().filter(|line| *line == "-D").count(),
            5,
            "tool={tool}: {captured}"
        );
        assert!(
            !result.to_string().contains("sentinel-secret"),
            "tool={tool} echoed a definition value: {result}"
        );
        if tool == "printable_scad_cross_section" {
            assert!(
                result["projection_diagnostics"].is_object(),
                "parameterized cross-section must report the projection subprocess"
            );
        }
    }
}

#[tokio::test]
async fn product_v1_profile_and_wrapper_are_consistent_across_scad_workflows() {
    let cases = [
        (
            "printable_scad_compile",
            "models/bracket.stl",
            r#"difference() {
    union() {
        pbl_panel(size=[50, 24, 4]);
        translate([-18, -6, 3.8]) cube([36, 12, 24.2]);
        translate([-18, 0, 3.8]) pbl_rib(length=12, height=12.2);
    }
    translate([0, 0, 17])
        pbl_horizontal_bore_cutter(length=40, diameter=8, axis="x");
}"#,
            json!({}),
        ),
        (
            "printable_scad_render",
            "renders/enclosure.png",
            r#"union() {
    pbl_shell(size=[60, 40, 20], wall=pbl_minimum_wall_mm);
    pbl_linear_pattern(count=2, spacing=36, axis="x")
        translate([-18, -10, 0])
            pbl_boss(height=8, outer_diameter=9, bore_diameter=3);
}"#,
            json!({"size": 32}),
        ),
        (
            "printable_scad_cross_section",
            "sections/grip.svg",
            "pbl_capsule(length=70, diameter=20, height=10);",
            json!({"z_mm": 5.0}),
        ),
    ];
    let expected_names = json!([
        "pbl_edge_break_mm",
        "pbl_layer_height_mm",
        "pbl_maximum_overhang_degrees",
        "pbl_minimum_wall_mm",
        "pbl_moving_clearance_mm",
        "pbl_nozzle_diameter_mm",
        "pbl_primary_radius_mm",
        "pbl_secondary_radius_mm",
        "pbl_transition_length_mm"
    ]);

    for (tool, path, source, extras) in cases {
        let tmp = tempfile::tempdir().expect("workspace tempdir");
        let ws = workspace(Some(tmp.path()));
        let fake = fake_openscad();
        let config = settings_with_openscad(fake.binary.clone());
        let profile = product_design_profile(50.0);
        let mut arguments = json!({
            "source": source,
            "path": path,
            "design_profile": profile,
            "timeout_seconds": 5.0
        });
        arguments
            .as_object_mut()
            .expect("arguments object")
            .extend(extras.as_object().expect("extras object").clone());

        let result = dispatch(
            &ws,
            &uploads(),
            &client("127.0.0.1", 9),
            &config,
            tool,
            arguments,
        )
        .await
        .expect("product workflow succeeds");

        assert_eq!(result["definitions"]["count"], json!(9), "tool={tool}");
        assert_eq!(
            result["definitions"]["names"], expected_names,
            "tool={tool}"
        );
        assert_eq!(
            result["design_profile"],
            product_design_profile(50.0),
            "tool={tool}"
        );

        let wrapper =
            std::fs::read_to_string(&fake.captured_first_source).expect("read trusted wrapper");
        assert!(wrapper.contains("include <product_v1.scad>"), "tool={tool}");
        assert!(wrapper.contains("include <caller.scad>"), "tool={tool}");
        let caller =
            std::fs::read_to_string(&fake.captured_caller).expect("read confined caller source");
        assert_eq!(caller, source, "tool={tool}");
        let kit = std::fs::read_to_string(&fake.captured_kit).expect("read bundled design kit");
        assert_eq!(kit, printable_scad::PRODUCT_V1_SOURCE);
        assert!(!kit.to_ascii_lowercase().contains("hinge"));

        let captured = std::fs::read_to_string(&fake.captured_args).expect("read captured argv");
        assert!(
            captured
                .lines()
                .any(|line| line == "pbl_maximum_overhang_degrees=50"),
            "{tool}: {captured}"
        );
        if tool == "printable_scad_compile" {
            assert_eq!(
                result["validation"]["overhang"]["threshold_degrees"],
                json!(50.0)
            );
            assert_eq!(
                result["manufacturing_evidence"],
                json!({
                    "units": "millimeters",
                    "build_direction": [0.0, 0.0, 1.0],
                    "measured": ["topology", "bounds", "build_plate_contact", "overhang"],
                    "kit_local_wall_assertions": {
                        "status": "enforced_when_used",
                        "minimum_wall_mm": 2.0,
                    },
                    "global_minimum_wall": {"status": "not_certified"},
                    "moving_clearance": {
                        "status": "not_run",
                        "requested_mm": 0.35,
                    },
                })
            );
        }
    }
}

#[tokio::test]
async fn scad_render_persists_and_returns_a_valid_inline_png() {
    let tmp = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace(Some(tmp.path()));
    let fake = fake_openscad();
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client("127.0.0.1", 9)),
        Arc::new(settings_with_openscad(fake.binary.clone())),
    );
    let arguments = json!({
        "source": "cube([10, 10, 10]);",
        "path": "renders/cube.png",
        "view": "front",
        "size": 32,
        "preview": false,
        "timeout_seconds": 5.0,
    })
    .as_object()
    .cloned()
    .expect("arguments object");

    let result = server
        .invoke_tool(workflow_call("printable_scad_render").with_arguments(arguments))
        .await
        .expect("render succeeds");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 2);
    let metadata: Value =
        serde_json::from_str(&result.content[0].as_text().expect("metadata text").text)
            .expect("metadata JSON");
    assert_eq!(metadata["artifact"]["path"], json!("renders/cube.png"));
    assert_eq!(metadata["view"], json!("front"));
    assert_eq!(metadata["preview"], json!(false));
    assert_eq!(metadata["inline"]["included"], json!(true));
    let image = result.content[1].as_image().expect("inline render image");
    let decoded = image::load_from_memory(
        &base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .expect("render base64"),
    )
    .expect("decode render PNG");
    assert_eq!((decoded.width(), decoded.height()), (32, 32));
    assert!(ws.read_artifact("renders/cube.png").is_ok());
}

#[tokio::test]
async fn scad_cross_section_wraps_the_requested_plane_and_persists_svg() {
    let tmp = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace(Some(tmp.path()));
    let fake = fake_openscad();
    let config = settings_with_openscad(fake.binary.clone());

    let result = dispatch(
        &ws,
        &uploads(),
        &client("127.0.0.1", 9),
        &config,
        "printable_scad_cross_section",
        json!({
            "source": "cube([10, 10, 10]);",
            "path": "sections/cube-z2_5.svg",
            "z_mm": 2.5,
            "timeout_seconds": 5.0,
        }),
    )
    .await
    .expect("cross-section succeeds");

    assert_eq!(result["artifact"]["path"], json!("sections/cube-z2_5.svg"));
    assert_eq!(result["z_mm"], json!(2.5));
    assert!(
        result.get("projection_diagnostics").is_none(),
        "legacy direct projection response must remain unchanged"
    );
    let captured =
        std::fs::read_to_string(&fake.captured_args).expect("read captured OpenSCAD argv");
    assert_eq!(
        captured.lines().filter(|line| *line == "-o").count(),
        1,
        "legacy cross-section must retain one OpenSCAD process: {captured}"
    );
    let confined_source =
        std::fs::read_to_string(&fake.captured_source).expect("read projected source");
    assert!(confined_source.contains("projection(cut = true)"));
    assert!(confined_source.contains("translate([0, 0, -2.5])"));
    let (_, svg) = ws
        .read_artifact("sections/cube-z2_5.svg")
        .expect("read SVG artifact");
    assert!(
        std::str::from_utf8(&svg)
            .expect("UTF-8 SVG")
            .contains("<svg")
    );
}

#[tokio::test]
async fn scad_rejects_invalid_requests_before_starting_the_process() {
    let tmp = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace(Some(tmp.path()));
    let fake = fake_openscad();
    let config = settings_with_openscad(fake.binary.clone());
    ws.write_artifact("existing.stl", &cube_stl(1.0), false)
        .expect("seed existing destination");
    let cases = [
        (
            "printable_scad_compile",
            json!({"source": "", "path": "bad.stl", "timeout_seconds": 5.0}),
        ),
        (
            "printable_scad_render",
            json!({
                "source": "cube(1);",
                "path": "bad.png",
                "view": "diagonal",
                "timeout_seconds": 5.0,
            }),
        ),
        (
            "printable_scad_render",
            json!({
                "source": "cube(1);",
                "path": "zero.png",
                "size": 0,
                "timeout_seconds": 5.0,
            }),
        ),
        (
            "printable_scad_cross_section",
            json!({
                "source": "cube(1);",
                "path": "bad.svg",
                "timeout_seconds": 0.0,
            }),
        ),
        (
            "printable_scad_compile",
            json!({
                "source": "cube(1);",
                "path": "existing.stl",
                "overwrite": false,
                "timeout_seconds": 5.0,
            }),
        ),
        (
            "printable_scad_compile",
            json!({
                "source": "cube(1);",
                "path": "reserved.stl",
                "defines": {"pbl_private": true},
                "timeout_seconds": 5.0,
            }),
        ),
        (
            "printable_scad_render",
            json!({
                "source": "cube(1);",
                "path": "control.png",
                "defines": {"label": "line\nbreak"},
                "timeout_seconds": 5.0,
            }),
        ),
        (
            "printable_scad_compile",
            json!({
                "source": "pbl_panel();",
                "path": "invalid-profile.stl",
                "design_profile": product_design_profile(90.1),
                "timeout_seconds": 5.0,
            }),
        ),
        (
            "printable_scad_compile",
            json!({
                "source": "module pbl_panel(size) { cube(size); } pbl_panel(10);",
                "path": "shadowed-kit.stl",
                "design_profile": product_design_profile(45.0),
                "timeout_seconds": 5.0,
            }),
        ),
    ];

    for (name, arguments) in cases {
        let error = dispatch(
            &ws,
            &uploads(),
            &client("127.0.0.1", 9),
            &config,
            name,
            arguments,
        )
        .await
        .expect_err("invalid request rejected");
        assert!(
            ["validation", "already_exists", "reserved_product_symbol"].contains(&error.code()),
            "tool={name}: {error}"
        );
        assert!(
            !fake.captured_source.exists(),
            "tool={name} started OpenSCAD before validation"
        );
    }
}

#[tokio::test]
async fn storage_actions_report_usage_and_preserve_live_and_retained_inputs() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(Some(tmp.path()));
    ws.configure_storage_budget(Some(100_000)).unwrap();
    ws.write_artifact("checkpoint.blend", b"retained", false)
        .unwrap();
    let live = ws.scratch(100, "transfer").unwrap();
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);
    let mut id = String::new();
    for action in ["usage", "cleanup_preview", "cleanup"] {
        let params = if action == "cleanup" {
            json!({"ids":[id.clone()]})
        } else {
            json!({})
        };
        let call = printable_server::tools::workflows::resolve(
            "artifact",
            json!({"action":action,"params":params}),
        )
        .unwrap();
        let result = dispatch(&ws, &up, &blender, &cfg, call.name, call.arguments)
            .await
            .unwrap();
        let contract = printable_server::resources::contracts::read(
            &format!("printable://contracts/artifact/{action}"),
            Default::default(),
        )
        .unwrap();
        jsonschema::validator_for(&contract["outputSchema"])
            .unwrap()
            .validate(&result)
            .unwrap();
        if action == "usage" {
            assert!(result["reserved_remaining_bytes"].as_u64().unwrap() > 0);
            assert_eq!(result["complete"], true);
        } else {
            assert_eq!(result["cleanup"][0]["state"], "protected");
            id = result["cleanup"][0]["id"].as_str().unwrap().to_owned();
        }
    }
    drop(live);
    let call = printable_server::tools::workflows::resolve(
        "artifact",
        json!({"action":"cleanup","params":{"ids":[id]}}),
    )
    .unwrap();
    let result = dispatch(&ws, &up, &blender, &cfg, call.name, call.arguments)
        .await
        .unwrap();
    assert_eq!(result["cleanup"][0]["state"], "deleted");
    assert_eq!(ws.read_artifact("checkpoint.blend").unwrap().1, b"retained");
}

#[tokio::test]
async fn workspace_write_read_list_round_trip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    let payload = b"solid cube\nendsolid\n";

    // write
    let written = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write",
        json!({"path": "a.stl", "data_base64": b64(payload)}),
    )
    .await
    .expect("write ok");
    assert_eq!(written["path"], json!("a.stl"));
    assert_eq!(written["size_bytes"], json!(payload.len()));

    // read round-trips the bytes
    let read = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_read",
        json!({"path": "a.stl"}),
    )
    .await
    .expect("read ok");
    assert_eq!(decode(&read["data_base64"]), payload);

    // list includes the artifact
    let listed = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_list",
        json!({}),
    )
    .await
    .expect("list ok");
    assert_eq!(listed["requested_limit"], 1000);
    assert_eq!(listed["returned"], 1);
    let paths: Vec<&str> = listed["entries"]
        .as_array()
        .expect("entries are an array")
        .iter()
        .filter_map(|m| m["path"].as_str())
        .collect();
    assert!(paths.contains(&"a.stl"), "listed: {listed}");
}

#[tokio::test]
async fn workspace_video_is_discoverable_but_never_base64_transferable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    ws.write_artifact("preview.mp4", b"small video", false)
        .expect("video artifact");
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    let error = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_read",
        json!({"path": "preview.mp4"}),
    )
    .await
    .expect_err("MP4 is not a base64 response type");
    assert_eq!(error.code(), "non_transferable_artifact");

    let listed = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_list",
        json!({}),
    )
    .await
    .expect("list video");
    assert_eq!(listed["entries"][0]["path"], json!("preview.mp4"));
    assert_eq!(listed["entries"][0]["media_type"], json!("video/mp4"));
}

#[tokio::test]
async fn artifact_pages_report_requested_and_returned_counts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    for index in 0..1101 {
        ws.write_artifact(&format!("file-{index}.stl"), b"fixture", false)
            .expect("artifact");
    }
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);
    for (arguments, requested, returned) in [
        (json!({}), 1000, 1000),
        (json!({"limit": 1500}), 1500, 1101),
        (json!({"limit": usize::MAX}), usize::MAX, 1101),
    ] {
        let listed = dispatch(
            &ws,
            &up,
            &blender,
            &cfg,
            "printable_workspace_list",
            arguments,
        )
        .await
        .expect("list");
        assert_eq!(listed["requested_limit"], requested);
        assert_eq!(listed["returned"], returned);
        assert_eq!(
            listed["entries"].as_array().expect("entries").len(),
            returned
        );
    }
    assert_eq!(
        dispatch(
            &ws,
            &up,
            &blender,
            &cfg,
            "printable_workspace_list",
            json!({"limit":0})
        )
        .await
        .expect_err("zero count")
        .code(),
        "invalid_limit"
    );
}

#[tokio::test]
async fn ordinary_mutation_tools_reject_the_reserved_job_namespace() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);
    let path = ".printable/jobs/job/source.blend";

    for (tool, arguments) in [
        (
            "printable_workspace_write",
            json!({"path": path, "data_base64": b64(b"replacement"), "overwrite": true}),
        ),
        (
            "printable_workspace_write_begin",
            json!({"path": path, "overwrite": true}),
        ),
        ("printable_scene_checkpoint", json!({"path": path})),
    ] {
        let error = dispatch(&ws, &up, &blender, &cfg, tool, arguments)
            .await
            .expect_err("reserved mutation destination rejected");
        assert_eq!(error.code(), "reserved_path", "tool={tool}: {error}");
    }
    assert!(!tmp.path().join(path).exists());
}

#[tokio::test]
async fn workspace_write_refuses_overwrite_and_bad_base64() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write",
        json!({"path": "a.stl", "data_base64": b64(b"one")}),
    )
    .await
    .expect("first write ok");

    // no-overwrite by default
    let err = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write",
        json!({"path": "a.stl", "data_base64": b64(b"two")}),
    )
    .await
    .expect_err("second write refused");
    assert!(matches!(
        err,
        ToolError::Workspace(WsError::AlreadyExists(_))
    ));
    assert_eq!(err.code(), "already_exists");

    // overwrite=true succeeds
    dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write",
        json!({"path": "a.stl", "data_base64": b64(b"two"), "overwrite": true}),
    )
    .await
    .expect("overwrite ok");

    // malformed base64
    let err = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write",
        json!({"path": "b.stl", "data_base64": "not valid base64!!!"}),
    )
    .await
    .expect_err("bad base64 rejected");
    assert!(matches!(err, ToolError::InvalidBase64));
    assert_eq!(err.to_string(), "data_base64 is not valid base64");
}

#[tokio::test]
async fn single_shot_write_caps_at_one_chunk() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    // Exactly the per-request chunk cap is accepted single-shot.
    let at_cap = vec![0u8; CHUNK_MAX_DECODED];
    let ok = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write",
        json!({"path": "cap.stl", "data_base64": b64(&at_cap)}),
    )
    .await
    .expect("write at chunk cap ok");
    assert_eq!(ok["size_bytes"], json!(CHUNK_MAX_DECODED));

    // One byte over the chunk cap is rejected: larger artifacts must stream.
    let over_cap = vec![0u8; CHUNK_MAX_DECODED + 1];
    let err = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write",
        json!({"path": "over.stl", "data_base64": b64(&over_cap)}),
    )
    .await
    .expect_err("single-shot over the chunk cap rejected");
    assert!(matches!(err, ToolError::PayloadTooLarge(_)), "got {err:?}");
    assert_eq!(err.code(), "payload_too_large");

    // An over-cap payload is rejected by *encoded length*, before any decode.
    // The bytes are deliberately invalid base64 ('@' is outside the alphabet),
    // which discriminates the pre-check from the post-decode path: with the
    // encoded-length guard this is PayloadTooLarge; without it the decode would
    // run first and return InvalidBase64.
    let max_encoded = (CHUNK_MAX_DECODED as u64).div_ceil(3) * 4;
    let oversized_invalid = "@".repeat(max_encoded as usize + 4);
    let err = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write",
        json!({"path": "huge.stl", "data_base64": oversized_invalid}),
    )
    .await
    .expect_err("oversized base64 rejected before decoding");
    assert!(
        matches!(err, ToolError::PayloadTooLarge(_)),
        "must reject by encoded length before decoding, got {err:?}"
    );
}

#[tokio::test]
async fn chunked_upload_round_trip_streams_a_large_artifact() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    // A payload larger than a single-shot write, streamed in chunk-sized pieces.
    let payload: Vec<u8> = (0..CHUNK_MAX_DECODED * 2 + 12_345)
        .map(|i| (i % 251) as u8)
        .collect();

    let begun = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_begin",
        json!({"path": "big.stl"}),
    )
    .await
    .expect("begin ok");
    let id = begun["upload_id"]
        .as_str()
        .expect("begin returns an upload_id")
        .to_string();

    let mut expected_total = 0u64;
    for slice in payload.chunks(CHUNK_MAX_DECODED) {
        expected_total += slice.len() as u64;
        let r = dispatch(
            &ws,
            &up,
            &blender,
            &cfg,
            "printable_workspace_write_chunk",
            json!({"upload_id": id, "data_base64": b64(slice)}),
        )
        .await
        .expect("chunk ok");
        assert_eq!(r["bytes_written"], json!(expected_total));
    }

    let meta = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_commit",
        json!({"upload_id": id}),
    )
    .await
    .expect("commit ok");
    assert_eq!(meta["path"], json!("big.stl"));
    assert_eq!(meta["size_bytes"], json!(payload.len()));

    // The committed artifact reads back byte-for-byte.
    let read = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_read",
        json!({"path": "big.stl"}),
    )
    .await
    .expect("read ok");
    assert_eq!(decode(&read["data_base64"]), payload);

    // The upload is consumed: committing it again is not found.
    let err = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_commit",
        json!({"upload_id": id}),
    )
    .await
    .expect_err("committed upload is gone");
    assert!(matches!(err, ToolError::UploadNotFound), "got {err:?}");
    assert_eq!(err.code(), "upload_not_found");
}

#[tokio::test]
async fn chunk_rejects_unknown_upload_and_oversized_chunk() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    // A chunk for an id that was never begun is not found.
    let err = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_chunk",
        json!({"upload_id": "0123456789abcdef0123456789abcdef", "data_base64": b64(b"x")}),
    )
    .await
    .expect_err("unknown upload rejected");
    assert!(matches!(err, ToolError::UploadNotFound), "got {err:?}");

    // An over-cap chunk is rejected before it can touch the staging file.
    let begun = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_begin",
        json!({"path": "x.stl"}),
    )
    .await
    .expect("begin ok");
    let id = begun["upload_id"].as_str().expect("upload_id").to_string();

    let over = vec![0u8; CHUNK_MAX_DECODED + 1];
    let err = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_chunk",
        json!({"upload_id": id, "data_base64": b64(&over)}),
    )
    .await
    .expect_err("oversized chunk rejected");
    assert!(matches!(err, ToolError::PayloadTooLarge(_)), "got {err:?}");
}

#[tokio::test]
async fn chunked_upload_accepts_artifacts_larger_than_inline_transfer_capacity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    let begun = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_begin",
        json!({"path": "capped.stl"}),
    )
    .await
    .expect("begin ok");
    let id = begun["upload_id"].as_str().expect("upload_id").to_string();

    // Reuse one encoded chunk to reach the inline transfer threshold.
    let full_chunks = MAX_TRANSFER_BYTES as usize / CHUNK_MAX_DECODED;
    let chunk_b64 = b64(&vec![0u8; CHUNK_MAX_DECODED]);
    for _ in 0..full_chunks {
        dispatch(
            &ws,
            &up,
            &blender,
            &cfg,
            "printable_workspace_write_chunk",
            json!({"upload_id": id, "data_base64": chunk_b64}),
        )
        .await
        .expect("chunk accepted");
    }

    let result = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_chunk",
        json!({"upload_id": id, "data_base64": b64(b"!")}),
    )
    .await
    .expect("streamed artifacts can exceed inline transport capacity");
    assert_eq!(result["bytes_written"], json!(MAX_TRANSFER_BYTES + 1));
    dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_commit",
        json!({"upload_id": id}),
    )
    .await
    .expect("large artifact committed");
    assert_eq!(
        ws.stat_artifact("capped.stl").unwrap().size_bytes,
        MAX_TRANSFER_BYTES + 1
    );
}

#[tokio::test]
async fn write_begin_requires_a_confined_workspace() {
    // begin must honour the same "workspace tools require a root" contract as
    // the direct write, refusing before it stages anything.
    let ws = workspace(None);
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    let err = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_begin",
        json!({"path": "x.stl"}),
    )
    .await
    .expect_err("begin on an unconfined server rejected");
    assert!(
        matches!(err, ToolError::Workspace(WsError::Unconfined)),
        "got {err:?}"
    );
    assert_eq!(err.code(), "unconfined");
}

#[tokio::test]
async fn begin_enforces_the_concurrent_upload_cap() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    // Open uploads up to the cap; the registry is shared, so they accumulate.
    for i in 0..MAX_CONCURRENT_UPLOADS {
        dispatch(
            &ws,
            &up,
            &blender,
            &cfg,
            "printable_workspace_write_begin",
            json!({"path": format!("f{i}.stl")}),
        )
        .await
        .expect("begin within cap ok");
    }

    // One more exceeds the concurrent-upload cap.
    let err = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_begin",
        json!({"path": "overflow.stl"}),
    )
    .await
    .expect_err("begin over the concurrent-upload cap rejected");
    assert!(matches!(err, ToolError::TooManyUploads(_)), "got {err:?}");
    assert_eq!(err.code(), "too_many_uploads");
}

#[tokio::test]
async fn status_rejects_unknown_arguments() {
    let ws = workspace(None);
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    // printable_status takes no parameters; an unknown field is rejected
    // (deny_unknown_fields) rather than silently accepted.
    let err = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_status",
        json!({"unexpected": 1}),
    )
    .await
    .expect_err("unknown status argument rejected");
    assert!(matches!(err, ToolError::Validation(_)), "got {err:?}");
}

#[tokio::test]
async fn project_scene_dispatch_confines_paths_and_preserves_observed_identity() {
    use printable_server::projects::{CreateParams, ProjectRequest};

    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&captured);
    let fake = FakeAddon::spawn(move |command, params| {
        observed.lock().unwrap().push((command, params.clone()));
        ResponseSpec::Success {
            result: json!({"scene_state": params["expected_scene"]}),
            addon_version: Some("0.4.0".into()),
        }
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());
    for id in ["alpha", "beta"] {
        printable_server::projects::dispatch(
            &ws,
            ProjectRequest::Create(CreateParams {
                project_id: id.into(),
                name: id.into(),
                description: String::new(),
                adopt_existing: false,
            }),
        )
        .unwrap();
    }
    ws.write_artifact("projects/alpha/model.glb", b"fixture", false)
        .unwrap();
    ws.write_artifact("projects/beta/scene.blend", b"fixture", false)
        .unwrap();
    let state = json!({
        "generation": "11111111-1111-4111-8111-111111111111",
        "revision": 7,
        "project_id": "alpha",
    });
    let attach = json!({"project_id":"alpha", "path":"model.glb", "expected_scene":state});
    dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_scene_attach_cad",
        attach.clone(),
    )
    .await
    .unwrap();
    let open = json!({"project_id":"beta", "mode":"checkpoint", "checkpoint":"scene.blend",
        "save_current_to":"projects/alpha/backup.blend", "expected_scene":state});
    dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_scene_open_project",
        open.clone(),
    )
    .await
    .unwrap();
    {
        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "attach_cad");
        assert_eq!(calls[0].1["path"], "projects/alpha/model.glb");
        assert_eq!(calls[0].1["expected_scene"], state);
        assert_eq!(calls[0].1["timeout_seconds"], 600);
        assert_eq!(calls[1].0, "open_project");
        assert_eq!(calls[1].1["checkpoint"], "projects/beta/scene.blend");
        assert_eq!(calls[1].1["save_current_to"], "projects/alpha/backup.blend");
        assert_eq!(calls[1].1["expected_scene"], state);
    }
    for (operation, base, field, value) in [
        (
            "printable_scene_attach_cad",
            &attach,
            "project_id",
            json!("beta"),
        ),
        (
            "printable_scene_attach_cad",
            &attach,
            "path",
            json!("../beta/model.glb"),
        ),
        (
            "printable_scene_attach_cad",
            &attach,
            "timeout_seconds",
            json!(0),
        ),
        (
            "printable_scene_open_project",
            &open,
            "save_current_to",
            json!("projects/beta/backup.blend"),
        ),
        (
            "printable_scene_open_project",
            &open,
            "save_current_to",
            Value::Null,
        ),
        (
            "printable_scene_open_project",
            &open,
            "mode",
            json!("adopt"),
        ),
        (
            "printable_scene_open_project",
            &open,
            "checkpoint",
            Value::Null,
        ),
        (
            "printable_scene_open_project",
            &open,
            "timeout_seconds",
            json!(0),
        ),
    ] {
        let mut invalid = base.clone();
        invalid[field] = value;
        assert!(
            dispatch(&ws, &up, &blender, &cfg, operation, invalid)
                .await
                .is_err(),
            "invalid {field} accepted by {operation}"
        );
    }
    assert_eq!(
        captured.lock().unwrap().len(),
        2,
        "invalid request reached Blender"
    );
}

#[tokio::test]
async fn status_reports_blender_up_via_fake_addon() {
    let fake = FakeAddon::ok(
        "bridge_status",
        json!({
            "blender_version": "5.2.0",
            "render_device": "OPTIX",
            "cycles_devices": [{"name": "RTX", "type": "OPTIX", "enabled": true}],
            "commands": ["bridge_status", "boolean"],
            "execution_limits": {
                "default_timeout_seconds": 120.0,
                "timeout_policy": "caller-selected positive runtime-representable seconds; no configured maximum",
                "timeout_outcome": "unknown after request delivery; never automatically retry"
            },
        }),
        "0.2.5",
    )
    .await;
    let ws = workspace(None);
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());

    let status = dispatch(&ws, &up, &blender, &cfg, "printable_status", json!({}))
        .await
        .expect("status ok");

    assert_eq!(status["transport"], json!("streamable-http"));
    assert_eq!(
        status["blender"]["available"],
        json!(true),
        "status: {status}"
    );
    assert_eq!(status["blender"]["version"], json!("5.2.0"));
    assert_eq!(status["blender"]["addon_version"], json!("0.2.5"));
    assert_eq!(status["blender"]["render_device"], json!("OPTIX"));
    assert_eq!(
        status["blender"]["cycles_devices"][0]["enabled"],
        json!(true)
    );
    assert_eq!(status["blender"]["commands"][1], json!("boolean"));
    assert_eq!(
        status["blender"]["execution_limits"]["default_timeout_seconds"],
        json!(120.0)
    );
    assert_eq!(
        status["blender"]["execution_limits"]["timeout_outcome"],
        json!("unknown after request delivery; never automatically retry")
    );
    assert_eq!(status["workspace"]["confined"], json!(false));
    assert_eq!(status["workspace"]["workspace_root"], Value::Null);
    // openscad section is present regardless of whether a binary is found.
    assert!(
        status["openscad"].get("available").is_some(),
        "status: {status}"
    );
}

#[tokio::test]
async fn status_reports_the_configured_openscad_runner_and_capacity() {
    let tmp = tempfile::tempdir().expect("workspace tempdir");
    let fake = fake_openscad();
    let config = settings_with_openscad(fake.binary.clone());

    let status = dispatch(
        &workspace(Some(tmp.path())),
        &uploads(),
        &client("127.0.0.1", 9),
        &config,
        "printable_status",
        json!({}),
    )
    .await
    .expect("status succeeds while Blender is unavailable");

    assert_eq!(status["openscad"]["available"], json!(true));
    assert_eq!(
        status["openscad"]["binary"],
        json!(fake.binary.display().to_string())
    );
    assert_eq!(status["openscad"]["concurrency"], json!(2));
}

#[tokio::test]
async fn status_reports_busy_blender_without_waiting_or_sending_another_command() {
    use printable_blender::Params;
    use tokio::sync::Notify;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let fake = FakeAddon::spawn({
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        move |_, _| ResponseSpec::SuccessWhenReleased {
            result: json!({}),
            addon_version: Some(printable_blender::BRIDGE_PROTOCOL_VERSION.to_owned()),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        }
    })
    .await;
    let blender = Arc::new(client(&fake.host(), fake.port()));
    let active = tokio::spawn({
        let blender = Arc::clone(&blender);
        async move {
            blender
                .send_value("long_render", Params::new(), blender.default_deadline())
                .await
        }
    });
    started.notified().await;
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        dispatch(
            &workspace(None),
            &uploads(),
            &blender,
            &settings(&fake.host(), fake.port()),
            "printable_status",
            json!({}),
        ),
    )
    .await
    .expect("status must not wait for the render")
    .unwrap();
    assert_eq!(status["blender"]["state"], "busy");
    assert_eq!(status["blender"]["available"], true);
    assert_eq!(status["cad"]["state"], "not_configured");
    assert_eq!(status["slicer"]["state"], "not_configured");
    assert_eq!(fake.commands(), ["long_render"]);
    release.notify_one();
    active.await.unwrap().unwrap();
    let schema = Value::Object(
        printable_server::tools::output::schema("status")
            .as_ref()
            .clone(),
    );
    assert!(
        jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&status)
    );
}

#[tokio::test]
async fn mesh_validation_returns_actionable_solid_and_support_properties() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let original = cube_stl(10.0);
    ws.write_artifact("models/cube.stl", &original, false)
        .expect("seed cube STL");

    let report = dispatch(
        &ws,
        &uploads(),
        &client("127.0.0.1", 9),
        &settings("127.0.0.1", 9),
        "printable_validate_mesh",
        json!({"path": "models/cube.stl", "density_g_cm3": 1.24}),
    )
    .await
    .expect("validate cube");

    assert_eq!(report["artifact"]["path"], json!("models/cube.stl"));
    assert_eq!(report["units"], json!("millimetres"));
    assert_eq!(report["report"]["topology"]["watertight"], json!(true));
    assert_eq!(report["report"]["topology"]["manifold"], json!(true));
    assert_eq!(report["report"]["solid_geometry"], json!(true));
    assert_eq!(report["report"]["printable"], json!(true));
    let assessment = &report["report"]["assessment"];
    assert_eq!(assessment["status"], "incomplete");
    assert_eq!(assessment["criteria"]["solid_topology"]["status"], "passed");
    assert_eq!(
        assessment["criteria"]["wall_thickness"]["status"],
        "unmeasured"
    );
    assert_eq!(
        assessment["criteria"]["physical_performance"]["status"],
        "physical_test_required"
    );
    let selected = printable_server::tools::workflows::resolve(
        "validate_mesh",
        json!({"path":"models/cube.stl"}),
    )
    .unwrap();
    let compact = selected.response.apply(report.clone());
    assert_eq!(compact["report"]["assessment"], *assessment);
    let contract = printable_server::resources::contracts::read(
        "printable://contracts/validate_mesh",
        Default::default(),
    )
    .unwrap();
    let validator = jsonschema::validator_for(&contract["outputSchema"]).unwrap();
    validator.validate(&compact).unwrap();
    let mut misleading = compact.clone();
    misleading["report"]["assessment"]["status"] = json!("passed");
    assert!(!validator.is_valid(&misleading));
    assert_eq!(
        report["report"]["solid_properties"]["volume_mm3"],
        json!(1000.0)
    );
    assert_eq!(report["report"]["solid_properties"]["mass_g"], json!(1.24));
    assert_eq!(
        report["report"]["overhang"]["bed_contact"]["faces"],
        json!(2)
    );
    assert_eq!(
        report["report"]["overhang"]["requires_support"],
        json!(false)
    );
    let (_, after) = ws
        .read_artifact("models/cube.stl")
        .expect("read cube after analysis");
    assert_eq!(after, original, "validation must not modify the artifact");
}

#[tokio::test]
async fn mesh_validation_reports_open_geometry_and_rejects_invalid_stl() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let triangle = binary_stl(
        &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        &[[0, 1, 2]],
    );
    ws.write_artifact("open.stl", &triangle, false)
        .expect("seed open STL");
    ws.write_artifact("invalid.stl", b"not an STL", false)
        .expect("seed invalid STL");
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    let report = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_validate_mesh",
        json!({"path": "open.stl"}),
    )
    .await
    .expect("open mesh report");
    assert_eq!(report["report"]["printable"], json!(false));
    assert_eq!(
        report["report"]["assessment"]["criteria"]["solid_topology"]["status"],
        "failed"
    );
    assert_eq!(report["report"]["topology"]["boundary_edges"], json!(3));
    assert_eq!(report["report"]["issues"][0]["code"], json!("open_mesh"));
    assert!(
        report["report"]["issues"][0]["recommendation"]
            .as_str()
            .expect("recommendation")
            .contains("watertight")
    );

    let error = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_validate_mesh",
        json!({"path": "invalid.stl"}),
    )
    .await
    .expect_err("invalid STL rejected");
    assert_eq!(error.code(), "invalid_stl");
}

#[tokio::test]
async fn assembly_analysis_separates_clearance_limits_from_physical_retention() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let fixed = cube_stl_at(2.0, [0.0, 0.0, 0.0]);
    let moving = cube_stl_at(2.0, [5.0, 0.0, 0.0]);
    ws.write_artifact("assembly/fixed.stl", &fixed, false)
        .expect("seed fixed STL");
    ws.write_artifact("assembly/moving.stl", &moving, false)
        .expect("seed moving STL");

    let report = dispatch(
        &ws,
        &uploads(),
        &client("127.0.0.1", 9),
        &settings("127.0.0.1", 9),
        "printable_analyze_assembly",
        json!({
            "fixed_path": "assembly/fixed.stl",
            "moving_path": "assembly/moving.stl",
            "required_clearance_mm": 2.0,
            "motion": {
                "direction": [-1.0, 0.0, 0.0],
                "travel_mm": 5.0,
                "target_clearance_mm": 0.5
            },
            "rotation": {
                "pivot_mm": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0,
                "target_clearance_mm": 0.5
            }
        }),
    )
    .await
    .expect("analyze assembly");

    assert_eq!(
        report["fixed_artifact"]["path"],
        json!("assembly/fixed.stl")
    );
    assert_eq!(
        report["moving_artifact"]["path"],
        json!("assembly/moving.stl")
    );
    assert_eq!(report["units"], json!("millimetres"));
    assert_eq!(
        report["report"]["static_analysis"]["relation"],
        json!("separated")
    );
    assert_eq!(
        report["report"]["static_analysis"]["clearance_mm"],
        json!(3.0)
    );
    assert_eq!(
        report["report"]["static_analysis"]["interference_volume_mm3"],
        json!(0.0)
    );
    assert_eq!(
        report["report"]["static_analysis"]["meets_required_clearance"],
        json!(true)
    );
    assert_eq!(
        report["report"]["motion"]["can_translate_full_distance"],
        json!(false)
    );
    assert_eq!(
        report["report"]["motion"]["first_blocked_at_mm"],
        json!(2.5)
    );
    assert_eq!(
        report["report"]["motion"]["block_reason"],
        json!("clearance_threshold")
    );
    assert_eq!(report["report"]["motion"]["retained"], json!(false));
    assert_eq!(
        report["report"]["rotation"]["can_rotate_full_angle"],
        json!(true)
    );
    assert!(
        report["report"]["rotation"]["minimum_certified_clearance_mm"]
            .as_f64()
            .expect("rotation clearance certificate")
            >= 0.5
    );

    let (_, fixed_after) = ws
        .read_artifact("assembly/fixed.stl")
        .expect("read fixed after analysis");
    let (_, moving_after) = ws
        .read_artifact("assembly/moving.stl")
        .expect("read moving after analysis");
    assert_eq!(fixed_after, fixed);
    assert_eq!(moving_after, moving);
}

#[tokio::test]
async fn assembly_analysis_identifies_the_invalid_part() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    ws.write_artifact("fixed.stl", &cube_stl(1.0), false)
        .expect("seed fixed STL");
    let open = binary_stl(
        &[[2.0, 0.0, 0.0], [3.0, 0.0, 0.0], [2.0, 1.0, 0.0]],
        &[[0, 1, 2]],
    );
    ws.write_artifact("moving.stl", &open, false)
        .expect("seed open moving STL");

    let error = dispatch(
        &ws,
        &uploads(),
        &client("127.0.0.1", 9),
        &settings("127.0.0.1", 9),
        "printable_analyze_assembly",
        json!({"fixed_path": "fixed.stl", "moving_path": "moving.stl"}),
    )
    .await
    .expect_err("open moving part rejected");

    assert_eq!(error.code(), "invalid_assembly_part");
    assert!(error.to_string().contains("moving"));
    assert!(error.to_string().contains("open_mesh"));
}

#[tokio::test]
async fn assembly_worker_unavailability_fails_before_in_process_geometry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    ws.write_artifact("fixed.stl", &cube_stl(1.0), false)
        .expect("seed fixed STL");
    ws.write_artifact("moving.stl", &cube_stl_at(1.0, [2.0, 0.0, 0.0]), false)
        .expect("seed moving STL");
    let mut unavailable = settings("127.0.0.1", 9);
    unavailable.geometry_worker_bin = Some(tmp.path().join("missing-worker"));

    let error = dispatch(
        &ws,
        &uploads(),
        &client("127.0.0.1", 9),
        &unavailable,
        "printable_analyze_assembly",
        json!({"fixed_path": "fixed.stl", "moving_path": "moving.stl"}),
    )
    .await
    .expect_err("missing worker is visible");

    assert_eq!(error.code(), "geometry_worker_unavailable");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn assembly_worker_memory_exhaustion_fails_without_killing_the_server() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    ws.write_artifact("fixed.stl", &cube_stl(1.0), false)
        .expect("seed fixed STL");
    ws.write_artifact("moving.stl", &cube_stl_at(1.0, [2.0, 0.0, 0.0]), false)
        .expect("seed moving STL");
    let mut limited = settings("127.0.0.1", 9);
    limited.geometry_worker_memory_bytes = 1024 * 1024;

    let error = dispatch(
        &ws,
        &uploads(),
        &client("127.0.0.1", 9),
        &limited,
        "printable_analyze_assembly",
        json!({"fixed_path": "fixed.stl", "moving_path": "moving.stl"}),
    )
    .await
    .expect_err("worker memory limit is visible");

    assert_eq!(error.code(), "geometry_resource_limit");
    let (_, fixed) = ws
        .read_artifact("fixed.stl")
        .expect("server remains usable after child termination");
    assert_eq!(fixed, cube_stl(1.0));
    assert_eq!(
        ws.storage_usage()
            .unwrap()
            .by_class
            .get("temporary")
            .copied()
            .unwrap_or(0),
        0,
        "failed geometry work releases its immutable snapshots"
    );
}

#[tokio::test]
async fn workflow_scene_preconditions_and_stale_metadata_reach_mcp() {
    let generation = "00000000-0000-4000-8000-000000000001";
    let revision = Arc::new(std::sync::Mutex::new(0_u64));
    let observed_revision = Arc::clone(&revision);
    let fake = FakeAddon::spawn(move |command, params| {
        assert_eq!(command, "rename_object");
        assert_eq!(params["expected_scene"]["generation"], generation);
        let mut revision = observed_revision.lock().expect("fake state lock");
        if params["expected_scene"]["revision"] != json!(*revision) {
            return ResponseSpec::RawWithId(json!({
                "status": "error", "error": "scene changed", "error_code": "stale_scene_state",
                "scene_state": {"generation": generation, "revision": *revision},
                "bridge_instance_id": generation,
                "addon_version": printable_blender::BRIDGE_PROTOCOL_VERSION,
            }));
        }
        *revision += 1;
        ResponseSpec::RawWithId(json!({
            "status": "success", "result": {"name": params["new_name"], "scene_state": {
                "generation": generation, "revision": *revision
            }},
            "bridge_instance_id": generation,
            "addon_version": printable_blender::BRIDGE_PROTOCOL_VERSION,
        }))
    })
    .await;
    let tmp = tempfile::tempdir().expect("workspace");
    let server = PrintableServer::new(
        workspace(Some(tmp.path())),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );
    for (expected, stale) in [(0, false), (0, true), (1, false)] {
        let arguments = json!({"action": "rename", "params": {
            "name": "Cube", "new_name": "Part",
            "expected_scene": {"generation": generation, "revision": expected}
        }})
        .as_object()
        .cloned()
        .expect("arguments object");
        let result = server
            .invoke_tool(CallToolRequestParams::new("edit").with_arguments(arguments))
            .await
            .expect("tool response");
        assert_eq!(result.is_error, Some(stale));
        let structured = result.structured_content.expect("structured state");
        let schema = serde_json::Value::Object(
            printable_server::tools::output::schema("edit")
                .as_ref()
                .clone(),
        );
        assert!(
            jsonschema::validator_for(&schema)
                .unwrap()
                .is_valid(&structured)
        );
        if stale {
            assert_eq!(structured["error"]["code"], "stale_scene_state");
            assert_eq!(structured["error"]["scene_state"]["revision"], 1);
        } else {
            assert_eq!(structured["scene_state"]["revision"], expected + 1);
        }
    }
    assert_eq!(*revision.lock().expect("revision"), 2);
    assert_eq!(fake.connection_count(), 3);
}

#[tokio::test]
async fn delivered_mutation_failures_validate_without_automatic_replay() {
    for code in ["addon", "timeout"] {
        let fake = FakeAddon::spawn(move |command, _| {
            assert_eq!(command, "execute_code");
            if code == "timeout" {
                ResponseSpec::Hang
            } else {
                ResponseSpec::Error {
                    message: "inspect scene state before another edit".into(),
                    traceback: None,
                    addon_version: None,
                }
            }
        })
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let server = PrintableServer::new(
            workspace(Some(tmp.path())),
            Arc::new(client(&fake.host(), fake.port())),
            Arc::new(settings(&fake.host(), fake.port())),
        );
        let result = server
            .invoke_tool(
                CallToolRequestParams::new("blender_execute").with_arguments(
                    json!({"code": "result = None", "timeout_seconds": 0.1})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        let payload = result.structured_content.unwrap();
        assert_eq!(payload["error"]["code"], code);
        let schema = Value::Object(
            printable_server::tools::output::schema("blender_execute")
                .as_ref()
                .clone(),
        );
        assert!(
            jsonschema::validator_for(&schema)
                .unwrap()
                .is_valid(&payload)
        );
        assert_eq!(fake.commands(), ["execute_code"]);
    }
}

#[tokio::test]
async fn artifact_workflow_results_match_the_discovered_output_contract() {
    let fake = FakeAddon::spawn(|_, _| panic!("artifact transfer must not invoke Blender")).await;
    let tmp = tempfile::tempdir().unwrap();
    let server = PrintableServer::new(
        workspace(Some(tmp.path())),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );
    let catalog = server.workflow_tools_payload();
    assert!(
        catalog
            .tools
            .iter()
            .all(|tool| tool.output_schema.is_some())
    );
    let schema = catalog
        .tools
        .iter()
        .find(|tool| tool.name == "artifact")
        .unwrap()
        .output_schema
        .as_ref()
        .unwrap();
    let validator = jsonschema::validator_for(&Value::Object(schema.as_ref().clone())).unwrap();
    for arguments in [
        json!({"action": "write", "params": {"path": "sample.stl", "data_base64": "c29saWQgZW1wdHlcbiBlbmRzb2xpZCBlbXB0eQ=="}}),
        json!({"action": "read", "params": {"path": "sample.stl"}}),
        json!({"action": "stat", "params": {"path": "sample.stl"}}),
        json!({"action": "list", "params": {}}),
    ] {
        let result = server
            .invoke_tool(
                CallToolRequestParams::new("artifact")
                    .with_arguments(arguments.as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        let value = result.structured_content.unwrap();
        assert!(
            validator.is_valid(&value),
            "{:?}",
            validator.iter_errors(&value).collect::<Vec<_>>()
        );
    }
    assert!(fake.commands().is_empty());
}

#[tokio::test]
async fn artifact_stat_resolves_project_paths_without_calling_blender() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(Some(tmp.path()));
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client("127.0.0.1", 9)),
        Arc::new(settings("127.0.0.1", 9)),
    );
    let created = server.invoke_tool(CallToolRequestParams::new("project").with_arguments(
        json!({"action": "create", "params": {"project_id": "organic", "name": "Organic study"}}).as_object().unwrap().clone()
    )).await.unwrap();
    assert_eq!(created.is_error, Some(false));
    ws.write_artifact("projects/organic/source.blend", b"source", false)
        .unwrap();
    let query = |path: &str| {
        CallToolRequestParams::new("artifact").with_arguments(
            json!({"action": "stat", "params": {"project_id": "organic", "path": path}})
                .as_object()
                .unwrap()
                .clone(),
        )
    };
    let result = server.invoke_tool(query("source.blend")).await.unwrap();
    assert_eq!(result.is_error, Some(false));
    let value = result.structured_content.unwrap();
    assert_eq!(value["path"], "projects/organic/source.blend");
    assert_eq!(value["project_path"], "source.blend");
    assert_eq!(value["project_id"], "organic");
    assert_eq!(value["identity"], "mutable_path");
    assert_eq!(value["size_bytes"], 6);
    assert!(value.get("data_base64").is_none());
    assert!(value.get("sha256").is_none());
    for path in ["../other/source.blend", "/source.blend", "missing.blend"] {
        assert_eq!(
            server.invoke_tool(query(path)).await.unwrap().is_error,
            Some(true)
        );
    }
}

#[tokio::test]
async fn native_view_retains_verified_images_and_observation_history() {
    let tmp = tempfile::tempdir().expect("workspace");
    let workspace = workspace(Some(tmp.path()));
    let backend_workspace = Arc::clone(&workspace);
    let fake = FakeAddon::spawn(move |command, params| {
        assert_eq!(command, "capture_native_view");
        assert_eq!(params["view"]["axis"], "FRONT");
        let path = params["path"].as_str().expect("generated path");
        let bytes = rgb_png(64, 32, Rgb([80, 120, 160]));
        backend_workspace.write_artifact(path, &bytes, false).expect("backend PNG");
        ResponseSpec::Success { result: json!({
            "path": path, "media_type": "image/png", "method": "viewport",
            "width": 64, "height": 32, "size_bytes": bytes.len(), "sha256": sha256_hex(&bytes),
            "freshness": {"dependency_evaluated": true, "redraw": "offscreen_draw_view3d"},
            "fidelity": "native_viewport_draw", "overlay_fidelity": "not_guaranteed",
            "capture_source": "gpu_offscreen",
            "convergence": "not_progressive", "view_configuration": {"view": {"shading": "SOLID"}},
            "view_state": {"configuration_sha256": "0".repeat(64)},
            "captured_at_unix_ms": 1, "elapsed_ms": 1,
            "scene_state": {"generation": "00000000-0000-4000-8000-000000000001", "revision": 2},
        }), addon_version: None }
    }).await;
    let server = PrintableServer::new(
        Arc::clone(&workspace),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );
    let mut paths = Vec::new();
    for _ in 0..2 {
        let result = server.invoke_tool(CallToolRequestParams::new("view").with_arguments(
            json!({"action": "native", "params": {"max_size": 64, "view": {"axis": "FRONT"}}})
                .as_object().cloned().expect("arguments"),
        )).await.expect("tool result");
        assert_eq!(result.is_error, Some(false));
        assert!(
            result
                .content
                .iter()
                .any(|item| matches!(item, ContentBlock::Image(_)))
        );
        let value = result.structured_content.expect("observation");
        let (_, metadata) = workspace
            .read_artifact(value["metadata_path"].as_str().expect("metadata path"))
            .expect("retained metadata");
        let metadata: Value = serde_json::from_slice(&metadata).expect("JSON metadata");
        assert_eq!(metadata["scene_state"], value["scene_state"]);
        assert_eq!(metadata["sha256"], value["sha256"]);
        paths.push(value["path"].clone());
    }
    assert_ne!(
        paths[0], paths[1],
        "omitted paths retain separate observations"
    );
}

#[tokio::test]
async fn editing_state_and_execution_context_reach_the_backend() {
    let fake = FakeAddon::spawn(|command, params| {
        match command.as_str() {
            "get_editing_state" => {
                assert_eq!(params["section"], "editors");
                assert_eq!(params["limit"], 2);
            }
            "execute_code" => {
                assert_eq!(params["context"]["area_type"], "NODE_EDITOR");
                assert_eq!(params["context"]["expected_mode"], "OBJECT");
            }
            _ => panic!("unexpected command {command}"),
        }
        ResponseSpec::Success {
            result: json!({"ok": true}),
            addon_version: None,
        }
    })
    .await;
    let tmp = tempfile::tempdir().expect("workspace");
    let server = PrintableServer::new(
        workspace(Some(tmp.path())),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );
    for (tool, arguments) in [
        (
            "inspect",
            json!({"action": "editing_state", "params": {"section": "editors", "limit": 2}}),
        ),
        (
            "blender_execute",
            json!({"code": "result = 1", "context": {"area_type": "NODE_EDITOR", "expected_mode": "OBJECT"}}),
        ),
    ] {
        let result = server
            .invoke_tool(
                CallToolRequestParams::new(tool)
                    .with_arguments(arguments.as_object().cloned().expect("arguments")),
            )
            .await
            .expect("tool response");
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result.structured_content.expect("structured response")["ok"],
            true
        );
    }
}

#[tokio::test]
async fn modeling_tools_send_typed_commands_and_confined_paths() {
    let fake =
        FakeAddon::spawn(
            |command, params| printable_blender::fake_addon::ResponseSpec::Success {
                result: json!({"command": command, "params": params}),
                addon_version: Some("0.2.5".to_string()),
            },
        )
        .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    ws.write_artifact("part.stl", b"solid part", false)
        .expect("seed STL");
    ws.write_artifact("checkpoint.blend", b"blend", false)
        .expect("seed checkpoint");
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());

    let cases = [
        (
            "printable_scene_get",
            json!({"name_contains": "handle", "object_type": "MESH", "collection": "Parts", "include_transforms": false}),
            "get_scene_info",
            json!({"offset": 0, "limit": 100, "name_contains": "handle", "object_type": "MESH", "collection": "Parts", "include_transforms": false}),
        ),
        (
            "printable_object_get",
            json!({"name": "Body", "section": "modifiers", "offset": 2, "limit": 3}),
            "get_object_info",
            json!({"name": "Body", "section": "modifiers", "offset": 2, "limit": 3}),
        ),
        (
            "printable_node_tree_get",
            json!({"name": "Paint"}),
            "get_node_tree_info",
            json!({"name": "Paint", "kind": "material", "section": "nodes", "offset": 0, "limit": 20}),
        ),
        (
            "printable_scene_get",
            json!({}),
            "get_scene_info",
            json!({"offset": 0, "limit": 100}),
        ),
        (
            "printable_scene_get",
            json!({"offset": 1_000_000, "limit": 1000}),
            "get_scene_info",
            json!({"offset": 1_000_000, "limit": 1000}),
        ),
        (
            "printable_object_get",
            json!({"name": "Body"}),
            "get_object_info",
            json!({"name": "Body"}),
        ),
        ("printable_scene_clear", json!({}), "clear_scene", json!({})),
        (
            "printable_scene_checkpoint",
            json!({"path": "checkpoints/model.blend"}),
            "save_blend",
            json!({"path": "checkpoints/model.blend"}),
        ),
        (
            "printable_scene_restore",
            json!({"path": "checkpoint.blend"}),
            "restore_checkpoint",
            json!({"path": "checkpoint.blend"}),
        ),
        (
            "printable_object_rename",
            json!({"name": "Body", "new_name": "Housing"}),
            "rename_object",
            json!({"name": "Body", "new_name": "Housing"}),
        ),
        (
            "printable_rigid_rotation_animate",
            json!({
                "objects": ["Housing", "Pin"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 2.0],
                "angle_degrees": 105.0
            }),
            "animate_rotation",
            json!({
                "objects": ["Housing", "Pin"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 2.0],
                "angle_degrees": 105.0,
                "frame_start": 1,
                "frame_end": 250
            }),
        ),
        (
            "printable_primitive_create",
            json!({"primitive": "cylinder", "name": "Pin", "vertices": 48, "radius": 2.5, "depth": 8.0}),
            "create_primitive",
            json!({"primitive": "cylinder", "name": "Pin", "vertices": 48, "radius": 2.5, "depth": 8.0}),
        ),
        (
            "printable_primitive_create",
            json!({"primitive": "cylinder", "vertices": 1024}),
            "create_primitive",
            json!({"primitive": "cylinder", "vertices": 1024}),
        ),
        (
            "printable_primitive_create",
            json!({"primitive": "cylinder", "vertices": 3}),
            "create_primitive",
            json!({"primitive": "cylinder", "vertices": 3}),
        ),
        (
            "printable_boolean_apply",
            json!({"target": "Housing", "operand": "Pin", "operation": "DIFFERENCE", "result_name": "BoredHousing", "delete_operand": true}),
            "boolean",
            json!({"target": "Housing", "operand": "Pin", "operation": "DIFFERENCE", "result_name": "BoredHousing", "delete_operand": true}),
        ),
        (
            "printable_stl_import",
            json!({"path": "part.stl"}),
            "import_stl",
            json!({"path": "part.stl"}),
        ),
        (
            "printable_stl_export",
            json!({"path": "exports/model.stl", "selected_only": true}),
            "export_stl",
            json!({"path": "exports/model.stl", "selected_only": true}),
        ),
        (
            "printable_blend_save",
            json!({"path": "exports/model.blend"}),
            "save_blend",
            json!({"path": "exports/model.blend"}),
        ),
        (
            "printable_blender_execute",
            json!({"code": "result = {'ok': True}"}),
            "execute_code",
            json!({"code": "result = {'ok': True}", "timeout_seconds": 120.0}),
        ),
        (
            "printable_blender_execute",
            json!({"code": "result = 1", "timeout_seconds": 0.1}),
            "execute_code",
            json!({"code": "result = 1", "timeout_seconds": 0.1}),
        ),
        (
            "printable_blender_execute",
            json!({"code": "result = 1", "timeout_seconds": 10_000_000_000.0}),
            "execute_code",
            json!({"code": "result = 1", "timeout_seconds": 10_000_000_000.0}),
        ),
    ];

    for (tool, arguments, command, expected_params) in cases.into_iter().chain([
        (
            "printable_scene_get", json!({"limit":usize::MAX}),
            "get_scene_info", json!({"offset":0,"limit":usize::MAX}),
        ),
        (
            "printable_object_get", json!({"name":"Body","section":"modifiers","limit":300}),
            "get_object_info", json!({"name":"Body","section":"modifiers","limit":300}),
        ),
        (
            "printable_node_tree_get", json!({"name":"Paint","limit":usize::MAX}),
            "get_node_tree_info", json!({"name":"Paint","kind":"material","section":"nodes","offset":0,"limit":usize::MAX}),
        ),
    ]) {
        let result = dispatch(&ws, &up, &blender, &cfg, tool, arguments)
            .await
            .unwrap_or_else(|error| panic!("{tool} failed: {error}"));
        assert_eq!(result["command"], json!(command), "tool={tool}");
        assert_eq!(result["params"], expected_params, "tool={tool}");
    }
}

#[tokio::test]
async fn rigid_rotation_validates_boundary_contracts_before_blender_mutation() {
    let fake = FakeAddon::registry(vec!["animate_rotation"], "0.2.5").await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());
    let invalid = [
        (
            json!({
                "objects": ["Leaf"],
                "controller_name": "",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0
            }),
            "non-empty string",
        ),
        (
            json!({
                "objects": ["Leaf"],
                "controller_name": "",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0
            }),
            "non-empty string",
        ),
        (
            json!({
                "objects": ["Leaf", "Leaf"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0
            }),
            "unique names",
        ),
        (
            json!({
                "objects": ["Leaf"],
                "controller_name": "Leaf",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0
            }),
            "must differ",
        ),
        (
            json!({
                "objects": ["Leaf"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 0.0],
                "angle_degrees": 90.0
            }),
            "non-zero magnitude",
        ),
        (
            json!({
                "objects": ["Leaf"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 0.0
            }),
            "positive finite",
        ),
        (
            json!({
                "objects": ["Leaf"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0,
                "frame_start": 10,
                "frame_end": 10
            }),
            "less than frame_end",
        ),
        (
            json!({
                "objects": ["Leaf"],
                "controller_name": "HingePivot",
                "pivot": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0,
                "frame_start": -1048575,
                "frame_end": 10
            }),
            "between -1048574 and 1048574",
        ),
    ];

    for (arguments, message) in invalid {
        let error = dispatch(
            &ws,
            &up,
            &blender,
            &cfg,
            "printable_rigid_rotation_animate",
            arguments,
        )
        .await
        .expect_err("invalid rotation rejected");
        assert!(matches!(error, ToolError::Validation(_)), "{error:?}");
        assert!(error.to_string().contains(message), "{error}");
    }
    assert_eq!(fake.connection_count(), 0);

    dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_rigid_rotation_animate",
        json!({
            "objects": ["Leaf"],
            "controller_name": "x".repeat(255),
            "pivot": [0.0, 0.0, 0.0],
            "axis": [0.0, 0.0, 1.0],
            "angle_degrees": 90.0
        }),
    )
    .await
    .expect("maximum-length controller name accepted");
    assert_eq!(fake.connection_count(), 1);
}

#[tokio::test]
async fn render_preview_forwards_defaults_and_caller_selected_quality() {
    let fake = FakeAddon::spawn(
        |command, params| printable_blender::fake_addon::ResponseSpec::Success {
            result: json!({
                "path": params["path"],
                "size_bytes": 2048,
                "media_type": "image/png",
                "width": params["width"],
                "height": params["height"],
                "engine": if params["engine"] == "CYCLES" { "CYCLES" } else { "BLENDER_EEVEE_NEXT" },
                "render_device": if params["engine"] == "CYCLES" { "CPU" } else { "GRAPHICS" },
                "graphics_backend": if params["engine"] == "CYCLES" {
                    json!({
                        "backend": "OPENGL",
                        "device_type": "NVIDIA",
                        "renderer": "RTX",
                        "vendor": "NVIDIA",
                        "version": "4.6",
                    })
                } else {
                    Value::Null
                },
                "samples": params.get("samples").cloned().unwrap_or(Value::Null),
                "received_command": command,
                "received_params": params,
            }),
            addon_version: Some("0.2.5".to_string()),
        },
    )
    .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());

    let defaulted = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_render_preview",
        json!({"path": "renders/preview.png", "include_inline": false}),
    )
    .await
    .expect("default preview renders");
    assert_eq!(defaulted["received_command"], json!("render_still"));
    assert_eq!(
        defaulted["received_params"],
        json!({
            "path": "renders/preview.png",
            "width": 512,
            "height": 512,
            "engine": "EEVEE",
            "timeout_seconds": 3600.0,
        })
    );
    assert_eq!(defaulted["inline"]["included"], json!(false));
    assert_eq!(defaulted["inline"]["reason"], json!("disabled by caller"));

    let cycles = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_render_preview",
        json!({
            "path": "renders/final.png",
            "width": 1920,
            "height": 1080,
            "engine": "CYCLES",
            "samples": 512,
            "timeout_seconds": 14_400.0,
        }),
    )
    .await
    .expect("caller-selected Cycles preview renders");
    assert_eq!(cycles["received_params"]["samples"], json!(512));
    assert_eq!(
        cycles["received_params"]["timeout_seconds"],
        json!(14_400.0)
    );
    assert!(cycles["received_params"].get("include_inline").is_none());
}

#[tokio::test]
async fn product_render_returns_a_verified_profile_artifact_without_source_mutation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace_root = tmp.path().to_path_buf();
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fake_capture = Arc::clone(&captured);
    let fake = FakeAddon::spawn(move |command, params| {
        assert_eq!(command, "render_product");
        fake_capture
            .lock()
            .expect("request capture lock")
            .push(params.clone());
        let path = params["path"].as_str().expect("product path");
        let bytes = rgb_png(
            params["width"].as_u64().expect("width") as u32,
            params["height"].as_u64().expect("height") as u32,
            Rgb([118, 49, 24]),
        );
        let destination = workspace_root.join(path);
        std::fs::create_dir_all(destination.parent().expect("render parent"))
            .expect("create render parent");
        std::fs::write(&destination, &bytes).expect("write product PNG");
        ResponseSpec::Success {
            result: json!({
                "path": path,
                "size_bytes": bytes.len(),
                "sha256": if path.ends_with("replaced.png") {
                    "0".repeat(64)
                } else {
                    sha256_hex(&bytes)
                },
                "media_type": "image/png",
                "width": params["width"],
                "height": params["height"],
                "engine": "BLENDER_EEVEE_NEXT",
                "render_device": "GRAPHICS",
                "graphics_backend": null,
                "samples": null,
                "objects": ["Body", "Insert"],
                "bounds": bounds_json([0.0, 0.0, 0.0], [20.0, 10.0, 5.0]),
                "presentation": {
                    "profile": "studio_neutral",
                    "camera": {
                        "type": "perspective",
                        "azimuth_degrees": 35.0,
                        "elevation_degrees": 20.0,
                        "position": [30.0, 30.0, 20.0],
                        "target": [10.0, 5.0, 2.5],
                        "ortho_scale": null,
                        "lens_mm": 70.0,
                        "sensor_width_mm": 36.0,
                        "clip_start": 0.01,
                        "clip_end": 1000.0
                    },
                    "lighting": [
                        {"role": "key", "type": "AREA", "position": [30.0, -30.0, 40.0], "energy_watts": 1000.0, "shape": "DISK", "size": 20.0},
                        {"role": "fill", "type": "AREA", "position": [-20.0, -10.0, 20.0], "energy_watts": 350.0, "shape": "DISK", "size": 24.0},
                        {"role": "rim", "type": "AREA", "position": [10.0, 30.0, 35.0], "energy_watts": 650.0, "shape": "DISK", "size": 15.0}
                    ],
                    "color_management": {
                        "display_device": "sRGB",
                        "view_transform": "Khronos PBR Neutral",
                        "look": "None",
                        "exposure": 0.0,
                        "gamma": 1.0
                    },
                    "world": {"base_color_srgb": [0.055, 0.055, 0.055], "strength": 0.65},
                    "ground": {
                        "enabled": true,
                        "style": "seamless",
                        "z": -0.05,
                        "size": 200.0,
                        "base_color_srgb": [0.18, 0.18, 0.18],
                        "metallic": 0.0,
                        "roughness": 0.72
                    },
                    "materials": {
                        "preserved_objects": ["Insert"],
                        "fallback": {
                            "objects": ["Insert"],
                            "base_color_srgb": [0.42, 0.45, 0.5],
                            "metallic": 0.0,
                            "roughness": 0.5
                        },
                        "overrides": [{
                            "objects": ["Body"],
                            "base_color_srgb": [0.74, 0.31, 0.12],
                            "metallic": 0.0,
                            "roughness": 0.34
                        }]
                    },
                    "shading": {
                        "mode": "smooth_by_angle",
                        "angle_degrees": 30.0,
                        "presentation_only": true
                    },
                    "framing": {
                        "margin_percent": 15.0,
                        "bounds": bounds_json([0.0, 0.0, 0.0], [20.0, 10.0, 5.0]),
                        "instance_count": 2
                    },
                    "geometry": {
                        "instances": 2,
                        "unique_evaluated_meshes": 2,
                        "vertices": 16,
                        "edges": 24,
                        "faces": 12,
                        "loops": 48,
                        "attribute_values": 0,
                        "material_slots": 1
                    }
                },
                "source_state_verified": true,
                "cleanup_verified": true
            }),
            addon_version: Some("0.4.0".to_string()),
        }
    })
    .await;
    let ws = workspace(Some(tmp.path()));
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );
    let arguments = json!({
        "path": "renders/product.png",
        "objects": ["Insert", "Body"],
        "presentation": {
            "profile": "studio_neutral",
            "view": {"azimuth_degrees": 35.0, "elevation_degrees": 20.0},
            "materials": [{
                "objects": ["Body"],
                "base_color_srgb": [0.74, 0.31, 0.12],
                "metallic": 0.0,
                "roughness": 0.34
            }]
        },
        "width": 80,
        "height": 60
    })
    .as_object()
    .cloned()
    .expect("product arguments");

    let result = server
        .invoke_tool(workflow_call("printable_render_product").with_arguments(arguments))
        .await
        .expect("product render succeeds");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 2);
    let metadata: Value =
        serde_json::from_str(&result.content[0].as_text().expect("metadata").text)
            .expect("metadata JSON");
    assert_eq!(metadata["presentation"]["profile"], json!("studio_neutral"));
    assert_eq!(metadata["source_state_verified"], json!(true));
    assert_eq!(metadata["cleanup_verified"], json!(true));
    assert_eq!(
        result.content[1]
            .as_image()
            .expect("inline product PNG")
            .mime_type,
        "image/png"
    );
    {
        let requests = captured.lock().expect("request capture lock");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["width"], json!(80));
        assert_eq!(requests[0]["height"], json!(60));
        assert_eq!(requests[0]["max_output_bytes"], json!(u64::MAX));
        assert!(requests[0].get("include_inline").is_none());
    }

    let replaced = server
        .invoke_tool(
            workflow_call("printable_render_product").with_arguments(
                json!({
                    "path": "renders/replaced.png",
                    "objects": ["Insert", "Body"],
                    "presentation": {
                        "profile": "studio_neutral",
                        "view": {"azimuth_degrees": 35.0, "elevation_degrees": 20.0},
                        "materials": [{
                            "objects": ["Body"],
                            "base_color_srgb": [0.74, 0.31, 0.12],
                            "metallic": 0.0,
                            "roughness": 0.34
                        }]
                    },
                    "width": 80,
                    "height": 60
                })
                .as_object()
                .cloned()
                .expect("replacement arguments"),
            ),
        )
        .await
        .expect("digest mismatch is a tool result");
    assert_eq!(replaced.is_error, Some(true));
}

#[tokio::test]
async fn product_render_rejects_invalid_presentation_before_blender() {
    let fake = FakeAddon::registry(vec!["render_product"], "0.4.0").await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());
    let base = || {
        json!({
            "path": "renders/product.png",
            "objects": ["Body", "Insert"],
            "presentation": {"profile": "studio_neutral"}
        })
    };
    let material = |objects: Value| {
        json!({
            "objects": objects,
            "base_color_srgb": [0.2, 0.3, 0.4],
            "metallic": 0.0,
            "roughness": 0.5
        })
    };
    let with_material = || {
        let mut arguments = base();
        arguments["presentation"]["materials"] = Value::Array(vec![material(json!(["Body"]))]);
        arguments
    };
    let mut duplicate_objects = base();
    duplicate_objects["objects"] = json!(["Body", "Body"]);
    let mut invalid_azimuth = base();
    let mut invalid_intensity = base();
    invalid_intensity["presentation"]["light_intensity_scale"] = json!(-0.1);
    invalid_azimuth["presentation"]["view"] = json!({"azimuth_degrees": 361.0});
    let mut invalid_elevation = base();
    invalid_elevation["presentation"]["view"] = json!({"elevation_degrees": 90.0});
    let mut occluded_studio_view = base();
    occluded_studio_view["presentation"]["view"] = json!({"elevation_degrees": -1.0});
    let mut unknown_material_object = base();
    unknown_material_object["presentation"]["materials"] =
        Value::Array(vec![material(json!(["Missing"]))]);
    let mut duplicate_assignment = base();
    duplicate_assignment["presentation"]["materials"] =
        Value::Array(vec![material(json!(["Body"])), material(json!(["Body"]))]);
    let mut invalid_color = with_material();
    invalid_color["presentation"]["materials"][0]["base_color_srgb"][0] = json!(1.1);
    let mut invalid_metallic = with_material();
    invalid_metallic["presentation"]["materials"][0]["metallic"] = json!(1.1);
    let mut invalid_roughness = with_material();
    invalid_roughness["presentation"]["materials"][0]["roughness"] = json!(-0.1);
    let mut invalid_samples = base();
    invalid_samples["engine"] = json!("EEVEE");
    invalid_samples["samples"] = json!(16);
    let mut too_many_materials = base();
    too_many_materials["presentation"]["materials"] =
        Value::Array((0..65).map(|_| material(json!(["Body"]))).collect());
    let cases = [
        (duplicate_objects, "unique names"),
        (invalid_azimuth, "azimuth_degrees"),
        (invalid_intensity, "light_intensity_scale"),
        (invalid_elevation, "elevation_degrees"),
        (occluded_studio_view, "ground cannot occlude"),
        (unknown_material_object, "not selected"),
        (duplicate_assignment, "assigned more than once"),
        (invalid_color, "base_color_srgb"),
        (invalid_metallic, "metallic"),
        (invalid_roughness, "roughness"),
        (invalid_samples, "samples"),
        (too_many_materials, "assigned more than once"),
    ];

    for (arguments, message) in cases {
        let error = dispatch(
            &ws,
            &uploads(),
            &blender,
            &cfg,
            "printable_render_product",
            arguments,
        )
        .await
        .expect_err("invalid product presentation rejected");
        assert_eq!(error.code(), "validation");
        assert!(error.to_string().contains(message), "{error}");
    }
    assert_eq!(fake.connection_count(), 0);

    let boundary_objects = (0..65)
        .map(|index| format!("Body{index}"))
        .collect::<Vec<_>>();
    let boundary_materials = boundary_objects
        .iter()
        .map(|object| material(json!([object])))
        .collect::<Vec<_>>();
    dispatch(
        &ws,
        &uploads(),
        &blender,
        &cfg,
        "printable_render_product",
        json!({
            "path": "renders/product.png",
            "objects": boundary_objects,
            "presentation": {
                "profile": "studio_neutral",
                "materials": boundary_materials
            }
        }),
    )
    .await
    .expect_err("empty fake response is malformed after admission");

    for boundary in [
        json!({
            "path": "renders/ground-boundary.png",
            "objects": ["Body"],
            "presentation": {
                "profile": "studio_neutral",
                "exposure_stops": 20.0,
                "light_intensity_scale": 100.0,
                "view": {"elevation_degrees": 0.0}
            }
        }),
        json!({
            "path": "renders/pixel-boundary.png",
            "objects": ["Body"],
            "presentation": {"profile": "engineering"},
            "width": 16384,
            "height": 8192
        }),
    ] {
        dispatch(
            &ws,
            &uploads(),
            &blender,
            &cfg,
            "printable_render_product",
            boundary,
        )
        .await
        .expect_err("empty fake response is malformed after boundary admission");
    }
    assert_eq!(
        fake.connection_count(),
        3,
        "the exact material, grounded-view, and pixel boundaries reach Blender"
    );
}

#[tokio::test]
async fn dimensions_render_returns_exact_bounds_and_labeled_orthographic_views() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let (fake, requests) = render_views_fake(tmp.path().to_path_buf()).await;
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );
    let arguments = json!({
        "path": "reviews/dimensions.png",
        "width": 32,
        "height": 24,
    })
    .as_object()
    .cloned()
    .expect("arguments object");

    let result = server
        .invoke_tool(workflow_call("printable_render_dimensions").with_arguments(arguments))
        .await
        .expect("dimensions call succeeds");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 2);
    let metadata: Value =
        serde_json::from_str(&result.content[0].as_text().expect("metadata text").text)
            .expect("metadata JSON");
    assert_eq!(metadata["kind"], json!("dimensions"));
    assert_eq!(metadata["path"], json!("reviews/dimensions.png"));
    assert_eq!(metadata["width"], json!(96));
    assert_eq!(metadata["height"], json!(52));
    assert_eq!(metadata["bounds"]["dimensions"], json!([20.0, 40.0, 60.0]));
    assert_eq!(
        metadata["views"]
            .as_array()
            .expect("views")
            .iter()
            .map(|view| view["label"].as_str().expect("label"))
            .collect::<Vec<_>>(),
        [
            "FRONT | X 20 x Z 60",
            "RIGHT | Y 40 x Z 60",
            "TOP | X 20 x Y 40",
        ]
    );
    assert!(result.content[1].as_image().is_some());

    let captured = requests.lock().expect("request capture lock");
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["views"].as_array().expect("views").len(), 3);
    assert_eq!(
        captured[0]["views"][0]["direction"],
        json!([0.0, -1.0, 0.0])
    );
    assert_eq!(captured[0]["views"][1]["direction"], json!([1.0, 0.0, 0.0]));
    assert_eq!(captured[0]["views"][2]["direction"], json!([0.0, 0.0, 1.0]));
    assert!(
        captured[0].get("presentation").is_none(),
        "dimension diagnostics retain their engineering render contract"
    );
}

#[tokio::test]
async fn cross_section_returns_exact_analysis_source_and_labeled_inline_render() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let (fake, requests) = render_diagnostic_fake(tmp.path().to_path_buf()).await;
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );
    let arguments = json!({
        "path": "reviews/section.png",
        "objects": ["Bracket", "Bolt"],
        "axis": "X",
        "position": 1.5,
        "view_direction": [1.0, 0.0, 0.0],
        "width": 32,
        "height": 24,
    })
    .as_object()
    .cloned()
    .expect("arguments object");

    let result = server
        .invoke_tool(workflow_call("printable_render_cross_section").with_arguments(arguments))
        .await
        .expect("cross-section call succeeds");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 2);
    let metadata: Value =
        serde_json::from_str(&result.content[0].as_text().expect("metadata text").text)
            .expect("metadata JSON");
    assert_eq!(metadata["kind"], json!("cross_section"));
    assert_eq!(metadata["path"], json!("reviews/section.png"));
    assert_eq!(metadata["width"], json!(32));
    assert_eq!(metadata["height"], json!(52));
    assert_eq!(metadata["objects"], json!(["Bolt", "Bracket"]));
    assert_eq!(
        metadata["source_bounds"]["dimensions"],
        json!([20.0, 40.0, 60.0])
    );
    assert_eq!(metadata["analysis"]["axis"], json!("X"));
    assert_eq!(metadata["analysis"]["position"], json!(1.5));
    assert_eq!(metadata["analysis"]["section_faces"], json!(2));
    assert!(
        metadata["source"]["path"]
            .as_str()
            .expect("source path")
            .starts_with("visual/diagnostics/")
    );
    assert!(result.content[1].as_image().is_some());

    let captured = requests.lock().expect("request capture lock");
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["mode"], json!("cross_section"));
    assert_eq!(captured[0]["timeout_seconds"], json!(3600.0));
    assert_eq!(captured[0]["objects"], json!(["Bracket", "Bolt"]));
    assert_eq!(captured[0]["path"], metadata["source"]["path"]);
}

#[tokio::test]
async fn heatmap_returns_exact_face_categories_source_and_labeled_inline_render() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let (fake, requests) = render_diagnostic_fake(tmp.path().to_path_buf()).await;
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );
    let arguments = json!({
        "path": "reviews/overhang.png",
        "build_direction": [0.0, 1.0, 0.0],
        "overhang_angle_degrees": 50.0,
        "view_direction": [1.0, -1.0, 1.0],
        "width": 32,
        "height": 24,
    })
    .as_object()
    .cloned()
    .expect("arguments object");

    let result = server
        .invoke_tool(
            workflow_call("printable_render_printability_heatmap").with_arguments(arguments),
        )
        .await
        .expect("heatmap call succeeds");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 2);
    let metadata: Value =
        serde_json::from_str(&result.content[0].as_text().expect("metadata text").text)
            .expect("metadata JSON");
    assert_eq!(metadata["kind"], json!("printability_heatmap"));
    assert_eq!(metadata["path"], json!("reviews/overhang.png"));
    assert_eq!(metadata["objects"], Value::Null);
    assert_eq!(
        metadata["analysis"]["categories"],
        json!({
            "supported": {"faces": 6, "area": 1000.0},
            "warning": {"faces": 4, "area": 500.0},
            "severe": {"faces": 2, "area": 250.0},
        })
    );
    assert!(
        metadata["source"]["path"]
            .as_str()
            .expect("source path")
            .starts_with("visual/diagnostics/")
    );
    assert!(result.content[1].as_image().is_some());

    let captured = requests.lock().expect("request capture lock");
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["mode"], json!("overhang"));
    assert_eq!(captured[0]["build_direction"], json!([0.0, 1.0, 0.0]));
    assert_eq!(captured[0]["overhang_angle_degrees"], json!(50.0));
    assert_eq!(captured[0]["path"], metadata["source"]["path"]);
}

#[tokio::test]
async fn diagnostic_directions_are_stably_normalized_before_blender() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (fake, requests) = render_diagnostic_fake(tmp.path().to_path_buf()).await;

    dispatch(
        &workspace(Some(tmp.path())),
        &uploads(),
        &client(&fake.host(), fake.port()),
        &settings(&fake.host(), fake.port()),
        "printable_render_printability_heatmap",
        json!({
            "path": "reviews/normalized.png",
            "build_direction": [f64::MAX, f64::MAX, f64::MAX],
            "view_direction": [0.0, -1e308, 0.0],
            "width": 8,
            "height": 8,
            "include_inline": false,
        }),
    )
    .await
    .expect("large finite directions are normalized without overflow");

    dispatch(
        &workspace(Some(tmp.path())),
        &uploads(),
        &client(&fake.host(), fake.port()),
        &settings(&fake.host(), fake.port()),
        "printable_render_printability_heatmap",
        json!({
            "path": "reviews/tiny-direction.png",
            "build_direction": [1e-300, 0.0, 0.0],
            "width": 8,
            "height": 8,
            "include_inline": false,
        }),
    )
    .await
    .expect("tiny finite directions are normalized without an arbitrary cutoff");

    let captured = requests.lock().expect("request capture lock");
    let expected = 3.0_f64.sqrt().recip();
    for component in captured[0]["build_direction"]
        .as_array()
        .expect("normalized build direction")
    {
        assert!((component.as_f64().expect("direction component") - expected).abs() < 1e-12);
    }
    assert_eq!(captured[0]["view_direction"], json!([0.0, -1.0, 0.0]));
    assert_eq!(captured[1]["build_direction"], json!([1.0, 0.0, 0.0]));
}

#[tokio::test]
async fn diagnostic_tools_reject_invalid_inputs_before_contacting_blender() {
    let fake = FakeAddon::spawn(|command, _params| {
        panic!("invalid diagnostic input reached Blender as {command}")
    })
    .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());
    let cases = [
        (
            "printable_render_cross_section",
            json!({"path": "reviews/a.png", "objects": ["Cube", "Cube"]}),
        ),
        (
            "printable_render_cross_section",
            json!({"path": "reviews/b.png", "view_direction": [0.0, 0.0, 0.0]}),
        ),
        (
            "printable_render_printability_heatmap",
            json!({"path": "reviews/c.png", "build_direction": [0.0, 0.0, 0.0]}),
        ),
        (
            "printable_render_printability_heatmap",
            json!({"path": "reviews/d.png", "overhang_angle_degrees": 91.0}),
        ),
    ];

    for (tool, arguments) in cases {
        let error = dispatch(&ws, &up, &blender, &cfg, tool, arguments)
            .await
            .expect_err("invalid diagnostic input rejected");
        assert_eq!(error.code(), "validation", "tool={tool}: {error}");
    }
}

#[tokio::test]
async fn heatmap_rejects_each_malformed_backend_contract() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let cases = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from([
        "path",
        "mode",
        "source_bounds",
        "rendered_minimum",
        "rendered_maximum",
        "topology",
        "category_sum",
        "parameters",
    ])));
    let pending = Arc::clone(&cases);
    let fake = FakeAddon::spawn(move |command, params| {
        assert_eq!(command, "render_diagnostic");
        let case = pending
            .lock()
            .expect("case queue lock")
            .pop_front()
            .expect("one case per request");
        let mut result = diagnostic_result(&root, &params);
        match case {
            "path" => result["path"] = json!("visual/diagnostics/other/source.png"),
            "mode" => result["mode"] = json!("cross_section"),
            "source_bounds" => result["source_bounds"]["dimensions"][0] = json!(99.0),
            "rendered_minimum" => {
                result["rendered_bounds"] = bounds_json([-9.0, -20.0, -30.0], [10.0, 20.0, 30.0]);
            }
            "rendered_maximum" => {
                result["rendered_bounds"] = bounds_json([-10.0, -20.0, -30.0], [9.0, 20.0, 30.0]);
            }
            "topology" => result["analysis"]["evaluated_loops"] = json!(2),
            "category_sum" => result["analysis"]["categories"]["supported"]["faces"] = json!(7),
            "parameters" => result["analysis"]["overhang_angle_degrees"] = json!(55.0),
            other => panic!("unknown case: {other}"),
        }
        printable_blender::fake_addon::ResponseSpec::Success {
            result,
            addon_version: Some("0.2.5".to_string()),
        }
    })
    .await;
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());

    for case in [
        "path",
        "mode",
        "source_bounds",
        "rendered_minimum",
        "rendered_maximum",
        "topology",
        "category_sum",
        "parameters",
    ] {
        let error = dispatch(
            &ws,
            &up,
            &blender,
            &cfg,
            "printable_render_printability_heatmap",
            json!({
                "path": format!("reviews/{case}.png"),
                "width": 8,
                "height": 8,
                "include_inline": false,
            }),
        )
        .await
        .expect_err("malformed diagnostic response rejected");
        assert_eq!(error.code(), "validation", "case={case}: {error}");
    }
    assert!(cases.lock().expect("case queue lock").is_empty());
}

#[tokio::test]
async fn cross_section_accepts_only_the_selected_axis_default_center() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let fake = FakeAddon::spawn(move |_command, params| {
        let mut result = diagnostic_result(&root, &params);
        let offset_bounds = json!({
            "minimum": [-10.0, -15.0, -20.0],
            "maximum": [10.0, 25.0, 40.0],
            "dimensions": [20.0, 40.0, 60.0],
            "center": [0.0, 5.0, 10.0],
            "diagonal": 5600.0_f64.sqrt(),
            "coordinate_space": "world",
            "unit": "blender_unit",
        });
        result["source_bounds"] = offset_bounds;
        result["rendered_bounds"] = bounds_json([-10.0, -15.0, -20.0], [10.0, 25.0, 10.0]);
        result["analysis"]["position"] = json!(10.0);
        printable_blender::fake_addon::ResponseSpec::Success {
            result,
            addon_version: Some("0.2.5".to_string()),
        }
    })
    .await;
    let result = dispatch(
        &workspace(Some(tmp.path())),
        &uploads(),
        &client(&fake.host(), fake.port()),
        &settings(&fake.host(), fake.port()),
        "printable_render_cross_section",
        json!({
            "path": "reviews/section.png",
            "width": 8,
            "height": 8,
            "include_inline": false,
        }),
    )
    .await
    .expect("selected-axis center accepted");

    assert_eq!(result["analysis"]["position"], json!(10.0));
}

#[tokio::test]
async fn cross_section_rejects_each_semantically_impossible_rendered_bounds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let cases = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from([
        "past_cut",
        "outside_source",
    ])));
    let pending = Arc::clone(&cases);
    let fake = FakeAddon::spawn(move |_command, params| {
        let mut result = diagnostic_result(&root, &params);
        let case = pending
            .lock()
            .expect("case queue lock")
            .pop_front()
            .expect("one case per request");
        result["rendered_bounds"] = match case {
            "past_cut" => render_bounds_json(),
            "outside_source" => bounds_json([-11.0, -20.0, -30.0], [10.0, 20.0, 0.0]),
            other => panic!("unknown case: {other}"),
        };
        printable_blender::fake_addon::ResponseSpec::Success {
            result,
            addon_version: Some("0.2.5".to_string()),
        }
    })
    .await;

    for case in ["past_cut", "outside_source"] {
        let error = dispatch(
            &workspace(Some(tmp.path())),
            &uploads(),
            &client(&fake.host(), fake.port()),
            &settings(&fake.host(), fake.port()),
            "printable_render_cross_section",
            json!({
                "path": format!("reviews/{case}.png"),
                "axis": "Z",
                "position": 0.0,
                "width": 8,
                "height": 8,
                "include_inline": false,
            }),
        )
        .await
        .expect_err("semantically impossible rendered bounds are rejected");
        assert_eq!(error.code(), "validation", "case={case}: {error}");
    }
    assert!(cases.lock().expect("case queue lock").is_empty());
}

#[tokio::test]
async fn cross_section_allows_an_empty_slice_on_the_retained_side() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let fake = FakeAddon::spawn(move |_command, params| {
        let mut result = diagnostic_result(&root, &params);
        result["rendered_bounds"] = bounds_json([-10.0, -20.0, -30.0], [10.0, 20.0, -5.0]);
        result["analysis"]["section_faces"] = json!(0);
        result["analysis"]["section_area"] = json!(0.0);
        printable_blender::fake_addon::ResponseSpec::Success {
            result,
            addon_version: Some("0.2.5".to_string()),
        }
    })
    .await;

    let result = dispatch(
        &workspace(Some(tmp.path())),
        &uploads(),
        &client(&fake.host(), fake.port()),
        &settings(&fake.host(), fake.port()),
        "printable_render_cross_section",
        json!({
            "path": "reviews/empty-section.png",
            "axis": "Z",
            "position": 0.0,
            "width": 8,
            "height": 8,
            "include_inline": false,
        }),
    )
    .await
    .expect("an empty slice can end before the requested plane");
    assert_eq!(result["analysis"]["section_faces"], json!(0));
}

#[tokio::test]
async fn cross_section_allows_float_rounding_far_from_the_origin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let fake = FakeAddon::spawn(move |_command, params| {
        let mut result = diagnostic_result(&root, &params);
        result["source_bounds"] = bounds_json([999_990.0, -20.0, -30.0], [1_000_010.0, 20.0, 30.0]);
        result["rendered_bounds"] =
            bounds_json([999_990.0, -20.0, -30.0], [1_000_000.0, 20.0, 30.0]);
        result["analysis"]["position"] = json!(1_000_000.01);
        printable_blender::fake_addon::ResponseSpec::Success {
            result,
            addon_version: Some("0.2.5".to_string()),
        }
    })
    .await;

    dispatch(
        &workspace(Some(tmp.path())),
        &uploads(),
        &client(&fake.host(), fake.port()),
        &settings(&fake.host(), fake.port()),
        "printable_render_cross_section",
        json!({
            "path": "reviews/far-origin.png",
            "axis": "X",
            "position": 1_000_000.01,
            "width": 8,
            "height": 8,
            "include_inline": false,
        }),
    )
    .await
    .expect("single-precision Blender bounds can round at large world coordinates");
}

#[tokio::test]
async fn gallery_renders_one_batch_and_returns_a_labeled_inline_composite() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let (fake, requests) = render_views_fake(tmp.path().to_path_buf()).await;
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );
    let arguments = json!({
        "path": "reviews/gallery.png",
        "views": ["front", "right"],
        "columns": 2,
        "width": 32,
        "height": 24,
        "presentation": {
            "profile": "studio_dark"
        }
    })
    .as_object()
    .cloned()
    .expect("arguments object");

    let result = server
        .invoke_tool(workflow_call("printable_render_gallery").with_arguments(arguments))
        .await
        .expect("gallery call succeeds");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 2);
    let metadata: Value =
        serde_json::from_str(&result.content[0].as_text().expect("metadata text").text)
            .expect("metadata JSON");
    assert_eq!(metadata["kind"], json!("gallery"));
    assert_eq!(metadata["path"], json!("reviews/gallery.png"));
    assert_eq!(metadata["width"], json!(64));
    assert_eq!(metadata["height"], json!(52));
    assert_eq!(metadata["layout"]["columns"], json!(2));
    assert_eq!(metadata["preset_views"], json!(["front", "right"]));
    assert_eq!(metadata["presentation"]["profile"], json!("studio_dark"));
    assert_eq!(metadata["views"].as_array().expect("views").len(), 2);
    assert!(
        metadata["views"][0]["path"]
            .as_str()
            .expect("view path")
            .starts_with("visual/renders/")
    );
    assert_eq!(metadata["inline"]["included"], json!(true));
    let image = result.content[1].as_image().expect("inline gallery image");
    let decoded = image::load_from_memory(
        &base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .expect("gallery base64"),
    )
    .expect("decode gallery");
    assert_eq!((decoded.width(), decoded.height()), (64, 52));

    let captured = requests.lock().expect("request capture lock");
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["views"][0]["label"], json!("FRONT"));
    assert_eq!(
        captured[0]["views"][0]["direction"],
        json!([0.0, -1.0, 0.0])
    );
    assert_eq!(captured[0]["views"][1]["direction"], json!([1.0, 0.0, 0.0]));
    assert_eq!(captured[0]["presentation"]["profile"], json!("studio_dark"));
}

#[tokio::test]
async fn gallery_rejects_each_malformed_backend_view_contract() {
    let cases = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from([
        "count", "path", "label", "media", "zero", "width",
    ])));
    let pending = Arc::clone(&cases);
    let fake = FakeAddon::spawn(move |_command, params| {
        let case = pending
            .lock()
            .expect("case queue lock")
            .pop_front()
            .expect("one case per request");
        let requested = &params["views"][0];
        let mut views = vec![json!({
            "path": requested["path"],
            "label": requested["label"],
            "size_bytes": 100,
            "media_type": "image/png",
            "width": params["width"],
            "height": params["height"],
        })];
        match case {
            "count" => views.clear(),
            "path" => views[0]["path"] = json!("visual/renders/other.png"),
            "label" => views[0]["label"] = json!("OTHER"),
            "media" => views[0]["media_type"] = json!("image/jpeg"),
            "zero" => views[0]["size_bytes"] = json!(0),
            "width" => views[0]["width"] = json!(7),
            other => panic!("unknown case: {other}"),
        }
        printable_blender::fake_addon::ResponseSpec::Success {
            result: json!({
                "views": views,
                "engine": "BLENDER_EEVEE_NEXT",
                "render_device": "GRAPHICS",
                "graphics_backend": null,
                "samples": null,
                "bounds": render_bounds_json(),
            }),
            addon_version: Some("0.2.5".to_string()),
        }
    })
    .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());

    for case in ["count", "path", "label", "media", "zero", "width"] {
        let error = dispatch(
            &ws,
            &up,
            &blender,
            &cfg,
            "printable_render_gallery",
            json!({
                "path": format!("reviews/{case}.png"),
                "views": ["front"],
                "width": 8,
                "height": 8,
                "include_inline": false,
            }),
        )
        .await
        .expect_err("malformed backend view rejected");
        assert_eq!(error.code(), "validation", "case={case}: {error}");
    }
    assert!(cases.lock().expect("case queue lock").is_empty());
}

#[tokio::test]
async fn gallery_accepts_the_complete_seven_preset_catalog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let (fake, requests) = render_views_fake(tmp.path().to_path_buf()).await;
    let result = dispatch(
        &ws,
        &uploads(),
        &client(&fake.host(), fake.port()),
        &settings(&fake.host(), fake.port()),
        "printable_render_gallery",
        json!({
            "path": "reviews/all-presets.png",
            "views": ["front", "right", "back", "left", "top", "bottom", "isometric"],
            "width": 8,
            "height": 8,
            "include_inline": false,
        }),
    )
    .await
    .expect("all seven presets render");

    assert_eq!(result["views"].as_array().expect("views").len(), 7);
    let captured = requests.lock().expect("request capture lock");
    assert_eq!(captured[0]["views"].as_array().expect("views").len(), 7);
}

#[tokio::test]
async fn turntable_generates_even_clockwise_orbit_views_and_contact_sheet() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let (fake, requests) = render_views_fake(tmp.path().to_path_buf()).await;
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());

    let result = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_render_turntable",
        json!({
            "path": "reviews/turntable.png",
            "frames": 4,
            "columns": 2,
            "width": 24,
            "height": 16,
            "elevation_degrees": 30.0,
            "clockwise": true,
            "include_inline": false,
        }),
    )
    .await
    .expect("turntable renders");

    assert_eq!(result["kind"], json!("turntable"));
    assert_eq!(result["width"], json!(48));
    assert_eq!(result["height"], json!(88));
    assert_eq!(result["layout"]["rows"], json!(2));
    assert_eq!(
        result["orbit"],
        json!({"frames": 4, "elevation_degrees": 30.0, "clockwise": true})
    );
    assert_eq!(result["views"].as_array().expect("views").len(), 4);
    assert_eq!(result["inline"]["reason"], json!("disabled by caller"));

    let captured = requests.lock().expect("request capture lock");
    let directions = captured[0]["views"].as_array().expect("views");
    let first = directions[0]["direction"]
        .as_array()
        .expect("first direction");
    let second = directions[1]["direction"]
        .as_array()
        .expect("second direction");
    assert!(first[0].as_f64().expect("x").abs() < 1e-12);
    assert!((first[1].as_f64().expect("y") + 3.0_f64.sqrt() / 2.0).abs() < 1e-12);
    assert!((first[2].as_f64().expect("z") - 0.5).abs() < 1e-12);
    assert!((second[0].as_f64().expect("x") + 3.0_f64.sqrt() / 2.0).abs() < 1e-12);
    assert!(second[1].as_f64().expect("y").abs() < 1e-12);
    assert_eq!(directions[0]["label"], json!("0°"));
    assert_eq!(directions[1]["label"], json!("90° CW"));
}

#[tokio::test]
async fn before_after_comparison_decodes_inputs_and_returns_a_real_png() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    ws.write_artifact(
        "reviews/before.png",
        &rgb_png(8, 8, Rgb([255, 0, 0])),
        false,
    )
    .expect("write before");
    ws.write_artifact("reviews/after.png", &rgb_png(8, 8, Rgb([0, 0, 255])), false)
        .expect("write after");
    let cfg = settings("127.0.0.1", 9);
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client("127.0.0.1", 9)),
        Arc::new(cfg),
    );
    let arguments = json!({
        "before_path": "reviews/before.png",
        "after_path": "reviews/after.png",
        "path": "reviews/comparison.png",
        "panel_width": 16,
        "panel_height": 12,
    })
    .as_object()
    .cloned()
    .expect("arguments object");

    let result = server
        .invoke_tool(workflow_call("printable_compare_renders").with_arguments(arguments))
        .await
        .expect("comparison call succeeds");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 2);
    let metadata: Value =
        serde_json::from_str(&result.content[0].as_text().expect("metadata text").text)
            .expect("metadata JSON");
    assert_eq!(metadata["kind"], json!("before_after"));
    assert_eq!(metadata["width"], json!(32));
    assert_eq!(metadata["height"], json!(40));
    assert_eq!(metadata["panels"]["before"], json!("reviews/before.png"));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            &result.content[1]
                .as_image()
                .expect("inline comparison")
                .data,
        )
        .expect("comparison base64");
    let image = image::load_from_memory(&bytes)
        .expect("decode comparison")
        .into_rgb8();
    assert_eq!(*image.get_pixel(8, 34), Rgb([255, 0, 0]));
    assert_eq!(*image.get_pixel(24, 34), Rgb([0, 0, 255]));
}

#[tokio::test]
async fn before_after_rejects_malformed_input_without_replacing_the_destination() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    ws.write_artifact("reviews/before.png", b"not a png", false)
        .expect("write malformed before");
    ws.write_artifact("reviews/after.png", &rgb_png(8, 8, Rgb([0, 0, 255])), false)
        .expect("write after");
    let original = rgb_png(4, 4, Rgb([0, 255, 0]));
    ws.write_artifact("reviews/comparison.png", &original, false)
        .expect("write existing comparison");
    let up = uploads();
    let blender = client("127.0.0.1", 9);
    let cfg = settings("127.0.0.1", 9);

    let error = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_compare_renders",
        json!({
            "before_path": "reviews/before.png",
            "after_path": "reviews/after.png",
            "path": "reviews/comparison.png",
        }),
    )
    .await
    .expect_err("malformed source rejected");

    assert_eq!(error.code(), "validation");
    assert_eq!(
        ws.read_artifact("reviews/comparison.png")
            .expect("read unchanged comparison")
            .1,
        original
    );
}

#[tokio::test]
async fn before_after_decodes_a_valid_input_above_one_megapixel() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let source = rgb_png(1200, 1000, Rgb([80, 120, 160]));
    ws.write_artifact("reviews/large-source.png", &source, false)
        .expect("write large source");

    let result = dispatch(
        &ws,
        &uploads(),
        &client("127.0.0.1", 9),
        &settings("127.0.0.1", 9),
        "printable_compare_renders",
        json!({
            "before_path": "reviews/large-source.png",
            "after_path": "reviews/large-source.png",
            "path": "reviews/large-source-comparison.png",
            "panel_width": 16,
            "panel_height": 16,
            "include_inline": false,
        }),
    )
    .await
    .expect("large but bounded PNG decodes");

    assert_eq!(result["width"], json!(32));
    assert_eq!(result["height"], json!(44));
}

#[tokio::test]
async fn render_preview_rejects_invalid_backend_artifact_metadata() {
    let fake =
        FakeAddon::spawn(
            |_command, params| printable_blender::fake_addon::ResponseSpec::Success {
                result: {
                    let requested_path = params["path"].as_str().expect("path string");
                    let case = requested_path
                        .rsplit('/')
                        .next()
                        .expect("path leaf")
                        .trim_end_matches(".png");
                    let cycles = params["engine"] == "CYCLES";
                    let mut result = json!({
                        "path": requested_path,
                        "size_bytes": 24,
                        "media_type": "image/png",
                        "width": params["width"],
                        "height": params["height"],
                        "engine": if cycles { "CYCLES" } else { "BLENDER_EEVEE_NEXT" },
                        "render_device": if cycles { "CPU" } else { "GRAPHICS" },
                        "samples": if cycles { json!(128) } else { Value::Null },
                        "graphics_backend": null,
                    });
                    match case {
                        "path" => result["path"] = json!("renders/other.png"),
                        "media" => result["media_type"] = json!("image/jpeg"),
                        "zero" => result["size_bytes"] = json!(0),
                        "missing-size" => {
                            result
                                .as_object_mut()
                                .expect("result object")
                                .remove("size_bytes");
                        }
                        "width" => result["width"] = json!(511),
                        "height" => result["height"] = json!(511),
                        "engine" => result["engine"] = json!("CYCLES"),
                        "device" => result["render_device"] = json!("CPU"),
                        "cycles-engine" => result["engine"] = json!("BLENDER_EEVEE_NEXT"),
                        "cycles-device" => result["render_device"] = json!("GRAPHICS"),
                        "samples" => result["samples"] = json!(32),
                        "graphics" => result["graphics_backend"] = json!({"backend": "OPENGL"}),
                        other => panic!("unknown metadata case: {other}"),
                    }
                    result
                },
                addon_version: Some("0.2.5".to_string()),
            },
        )
        .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());

    for case in [
        "path",
        "media",
        "zero",
        "missing-size",
        "width",
        "height",
        "engine",
        "device",
        "cycles-engine",
        "cycles-device",
        "samples",
        "graphics",
    ] {
        let arguments = if case.starts_with("cycles-") {
            json!({"path": format!("renders/{case}.png"), "engine": "CYCLES"})
        } else {
            json!({"path": format!("renders/{case}.png")})
        };
        let error = dispatch(
            &ws,
            &up,
            &blender,
            &cfg,
            "printable_render_preview",
            arguments,
        )
        .await
        .expect_err("invalid backend metadata rejected");
        assert_eq!(error.code(), "validation", "case={case}: {error}");
    }
}

#[tokio::test]
async fn render_preview_returns_small_png_as_inline_mcp_image_content() {
    let png = rgb_png(320, 240, Rgb([20, 80, 160]));
    let size_bytes = png.len();
    let fake = FakeAddon::ok(
        "render_still",
        json!({
            "path": "renders/preview.png",
            "size_bytes": size_bytes,
            "media_type": "image/png",
            "width": 320,
            "height": 240,
            "engine": "BLENDER_EEVEE_NEXT",
            "render_device": "GRAPHICS",
            "graphics_backend": null,
            "samples": null,
        }),
        "0.2.5",
    )
    .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    ws.write_artifact("renders/preview.png", &png, false)
        .expect("seed rendered PNG");
    let cfg = settings(&fake.host(), fake.port());
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(cfg),
    );
    let arguments = json!({
        "path": "renders/preview.png",
        "width": 320,
        "height": 240,
    })
    .as_object()
    .cloned()
    .expect("arguments object");

    let result = server
        .invoke_tool(workflow_call("printable_render_preview").with_arguments(arguments))
        .await
        .expect("MCP tool call succeeds");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 2);
    let text = result.content[0].as_text().expect("metadata text block");
    let metadata: Value = serde_json::from_str(&text.text).expect("metadata JSON");
    assert_eq!(metadata["inline"]["included"], json!(true));
    let image = result.content[1].as_image().expect("inline image block");
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .expect("valid image base64"),
        png
    );
}

#[tokio::test]
async fn large_render_remains_an_artifact_without_inline_transfer() {
    let fake = FakeAddon::ok(
        "render_still",
        json!({
            "path": "renders/large.png",
            "size_bytes": 1_048_577,
            "media_type": "image/png",
            "width": 4096,
            "height": 4096,
            "engine": "CYCLES",
            "render_device": "OPTIX",
            "graphics_backend": null,
            "samples": 128,
        }),
        "0.2.5",
    )
    .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );
    let arguments = json!({
        "path": "renders/large.png",
        "width": 4096,
        "height": 4096,
        "engine": "CYCLES",
    })
    .as_object()
    .cloned()
    .expect("arguments object");

    let result = server
        .invoke_tool(workflow_call("printable_render_preview").with_arguments(arguments))
        .await
        .expect("MCP tool call succeeds");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 1);
    assert!(matches!(result.content[0], ContentBlock::Text(_)));
    let text = result.content[0].as_text().expect("metadata text block");
    let metadata: Value = serde_json::from_str(&text.text).expect("metadata JSON");
    assert_eq!(metadata["path"], json!("renders/large.png"));
    assert_eq!(
        metadata["inline"]["reason"],
        json!("artifact exceeds inline transport limit")
    );
}

#[tokio::test]
async fn inline_render_rejects_each_malformed_png_contract() {
    let mut bad_signature = rgb_png(320, 240, Rgb([20, 80, 160]));
    bad_signature[0] = 0;
    let mut bad_header = rgb_png(320, 240, Rgb([20, 80, 160]));
    bad_header[12..16].copy_from_slice(b"NOPE");
    let cases = [
        ("renders/short.png", b"not-png!".to_vec()),
        ("renders/signature.png", bad_signature),
        ("renders/header.png", bad_header),
        ("renders/width.png", rgb_png(321, 240, Rgb([20, 80, 160]))),
        ("renders/height.png", rgb_png(320, 241, Rgb([20, 80, 160]))),
    ];
    let reported_sizes: std::collections::HashMap<String, u64> = cases
        .iter()
        .map(|(path, bytes)| ((*path).to_string(), bytes.len() as u64))
        .collect();
    let fake = FakeAddon::spawn(move |_command, params| {
        printable_blender::fake_addon::ResponseSpec::Success {
            result: {
                let path = params["path"].as_str().expect("path string");
                json!({
                    "path": path,
                    "size_bytes": reported_sizes[path],
                    "media_type": "image/png",
                    "width": params["width"],
                    "height": params["height"],
                    "engine": "BLENDER_EEVEE_NEXT",
                    "render_device": "GRAPHICS",
                    "samples": null,
                    "graphics_backend": null,
                })
            },
            addon_version: Some("0.2.5".to_string()),
        }
    })
    .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    for (path, bytes) in cases {
        ws.write_artifact(path, &bytes, false)
            .expect("seed malformed render artifact");
    }
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );

    for case in ["short", "signature", "header", "width", "height"] {
        let arguments = json!({
            "path": format!("renders/{case}.png"),
            "width": 320,
            "height": 240,
        })
        .as_object()
        .cloned()
        .expect("arguments object");
        let result = server
            .invoke_tool(workflow_call("printable_render_preview").with_arguments(arguments))
            .await
            .expect("tool error is an MCP result");
        assert_eq!(result.is_error, Some(true), "case={case}: {result:?}");
    }
}

#[tokio::test]
async fn inline_render_includes_an_artifact_at_the_exact_size_limit() {
    let png = rgb_png_with_total_size(16, 16, 1024 * 1024);
    let fake = FakeAddon::ok(
        "render_still",
        json!({
            "path": "renders/limit.png",
            "size_bytes": png.len(),
            "media_type": "image/png",
            "width": 16,
            "height": 16,
            "engine": "BLENDER_EEVEE_NEXT",
            "render_device": "GRAPHICS",
            "samples": null,
            "graphics_backend": null,
        }),
        "0.2.5",
    )
    .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = workspace(Some(tmp.path()));
    ws.write_artifact("renders/limit.png", &png, false)
        .expect("seed limit-size render artifact");
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(settings(&fake.host(), fake.port())),
    );
    let arguments = json!({"path": "renders/limit.png", "width": 16, "height": 16})
        .as_object()
        .cloned()
        .expect("arguments object");

    let result = server
        .invoke_tool(workflow_call("printable_render_preview").with_arguments(arguments))
        .await
        .expect("limit-size preview succeeds");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 2);
    let metadata: Value =
        serde_json::from_str(&result.content[0].as_text().expect("metadata text").text)
            .expect("metadata JSON");
    assert!(metadata["inline"].get("reason").is_none());
    let image = result.content[1].as_image().expect("inline image block");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .expect("valid image base64")
            .len(),
        1024 * 1024
    );
}

#[tokio::test]
async fn modeling_tools_validate_before_blender_mutation() {
    let fake = FakeAddon::registry(
        vec![
            "export_stl",
            "import_stl",
            "restore_checkpoint",
            "save_blend",
        ],
        "0.2.5",
    )
    .await;
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());

    let unconfined = workspace(None);
    let error = dispatch(
        &unconfined,
        &up,
        &blender,
        &cfg,
        "printable_stl_export",
        json!({"path": "model.stl"}),
    )
    .await
    .expect_err("unconfined export rejected");
    assert!(matches!(error, ToolError::Workspace(WsError::Unconfined)));

    let tmp = tempfile::tempdir().expect("tempdir");
    let confined = workspace(Some(tmp.path()));
    for (tool, arguments, expected_code) in [
        (
            "printable_stl_import",
            json!({"path": "missing.stl"}),
            "not_found",
        ),
        (
            "printable_scene_restore",
            json!({"path": "../escape.blend"}),
            "path_escapes",
        ),
        (
            "printable_blend_save",
            json!({"path": "wrong.stl"}),
            "validation",
        ),
        ("printable_scene_get", json!({"limit": 0}), "validation"),
        (
            "printable_scene_get",
            json!({"name_contains": ""}),
            "validation",
        ),
        (
            "printable_scene_get",
            json!({"include_transforms": "false"}),
            "validation",
        ),
        (
            "printable_object_get",
            json!({"name": "Cube", "limit": 0}),
            "validation",
        ),
        (
            "printable_object_get",
            json!({"name": "Cube", "section": "all"}),
            "validation",
        ),
        ("printable_node_tree_get", json!({"name": ""}), "validation"),
        (
            "printable_node_tree_get",
            json!({"name": "Material", "limit": 0}),
            "validation",
        ),
        (
            "printable_node_tree_get",
            json!({"name": "Material", "limit": 0}),
            "validation",
        ),
        (
            "printable_node_tree_get",
            json!({"name": "Material", "kind": "shader"}),
            "validation",
        ),
        (
            "printable_scene_get",
            json!({"offset": 1_000_001, "limit": 0}),
            "validation",
        ),
        (
            "printable_primitive_create",
            json!({"primitive": "cube", "radius": 1.0}),
            "validation",
        ),
        (
            "printable_primitive_create",
            json!({"primitive": "cylinder", "vertices": 2}),
            "validation",
        ),
        (
            "printable_primitive_create",
            json!({"primitive": "cylinder", "vertices": 2}),
            "validation",
        ),
        (
            "printable_primitive_create",
            json!({"primitive": "cylinder", "radius": -1.0}),
            "validation",
        ),
        (
            "printable_blender_execute",
            json!({"code": ""}),
            "validation",
        ),
        (
            "printable_blender_execute",
            json!({"code": "result = None", "timeout_seconds": 0.0}),
            "validation",
        ),
        (
            "printable_blender_execute",
            json!({"code": "result = None", "timeout_seconds": -1.0}),
            "validation",
        ),
        (
            "printable_render_preview",
            json!({"path": "preview.jpg"}),
            "validation",
        ),
        (
            "printable_render_preview",
            json!({"path": "preview.PNG"}),
            "validation",
        ),
        (
            "printable_render_preview",
            json!({"path": "preview.png", "width": 0}),
            "validation",
        ),
        (
            "printable_render_preview",
            json!({"path": "preview.png", "engine": "EEVEE", "samples": 32}),
            "validation",
        ),
        (
            "printable_render_preview",
            json!({"path": "preview.png", "engine": "CYCLES", "samples": 0}),
            "validation",
        ),
        (
            "printable_render_preview",
            json!({"path": "preview.png", "timeout_seconds": 0.0}),
            "validation",
        ),
        (
            "printable_render_gallery",
            json!({"path": "gallery.png", "views": ["front", "front"]}),
            "validation",
        ),
        (
            "printable_render_gallery",
            json!({"path": "gallery.png", "views": []}),
            "validation",
        ),
        (
            "printable_render_gallery",
            json!({
                "path": "gallery.png",
                "views": ["front", "right", "back", "left", "top", "bottom", "isometric", "front"]
            }),
            "validation",
        ),
        (
            "printable_render_gallery",
            json!({"path": "gallery.png", "unknown": true}),
            "validation",
        ),
        (
            "printable_render_gallery",
            json!({"path": "gallery.png", "width": 0}),
            "validation",
        ),
        (
            "printable_render_turntable",
            json!({"path": "turntable.png", "frames": 0}),
            "validation",
        ),
        (
            "printable_render_turntable",
            json!({"path": "turntable.png", "elevation_degrees": 90.0}),
            "validation",
        ),
    ] {
        let error = dispatch(&confined, &up, &blender, &cfg, tool, arguments)
            .await
            .expect_err("invalid path rejected");
        assert_eq!(error.code(), expected_code, "tool={tool}: {error}");
    }

    assert_eq!(fake.connection_count(), 0);
}

#[tokio::test]
async fn status_reports_blender_down_without_erroring() {
    let port = closed_port().await;
    let ws = workspace(None);
    let up = uploads();
    let blender = client("127.0.0.1", port);
    let cfg = settings("127.0.0.1", port);

    // The tool itself succeeds even though Blender is unreachable.
    let status = dispatch(&ws, &up, &blender, &cfg, "printable_status", json!({}))
        .await
        .expect("status ok even with blender down");

    assert_eq!(
        status["blender"]["available"],
        json!(false),
        "status: {status}"
    );
    assert!(
        status["blender"]["error"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "a downed blender reports an error string: {status}"
    );
    assert_eq!(status["blender"]["port"], json!(port));
}

#[tokio::test]
async fn status_reports_a_legacy_bridge_protocol_before_rejecting_its_envelope() {
    let fake = FakeAddon::spawn(|_command, _params| {
        ResponseSpec::RawWithId(json!({
            "status": "success",
            "result": {},
            "addon_version": "0.1.0"
        }))
    })
    .await;
    let ws = workspace(None);
    let up = uploads();
    let blender = client(&fake.host(), fake.port());
    let cfg = settings(&fake.host(), fake.port());

    let status = dispatch(&ws, &up, &blender, &cfg, "printable_status", json!({}))
        .await
        .expect("status remains available for a legacy bridge");

    assert_eq!(status["blender"]["available"], json!(false));
    assert!(
        status["blender"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("bridge_instance_id"))
    );
    let warning = status["blender"]["compatibility_warning"]
        .as_str()
        .expect("compatibility warning");
    assert!(
        warning.contains("0.1.0") && warning.contains(printable_blender::BRIDGE_PROTOCOL_VERSION)
    );
}

#[tokio::test]
async fn durable_still_job_is_submitted_polled_and_discovered_through_mcp_tools() {
    let temp = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace(Some(temp.path()));
    ws.write_artifact("scene.blend", b"checkpoint", false)
        .expect("source checkpoint");
    let root = temp.path().to_path_buf();
    let fake = FakeAddon::spawn(move |command, params| match command.as_str() {
        "bridge_status" => ResponseSpec::Success {
            result: json!({"role": "render_worker", "background": true}),
            addon_version: Some("test".to_string()),
        },
        "job_restore_checkpoint" => ResponseSpec::Success {
            result: json!({"restored": true}),
            addon_version: Some("test".to_string()),
        },
        "job_render_still" => {
            let path = params["path"].as_str().expect("render path");
            let destination = root.join(path);
            std::fs::create_dir_all(destination.parent().expect("frame parent"))
                .expect("create frame directory");
            std::fs::write(&destination, b"png-frame").expect("write fake frame");
            ResponseSpec::Success {
                result: json!({"path": path, "size_bytes": 9}),
                addon_version: Some("test".to_string()),
            }
        }
        _ => ResponseSpec::Error {
            message: format!("unexpected command: {command}"),
            traceback: None,
            addon_version: Some("test".to_string()),
        },
    })
    .await;
    let live = FakeAddon::spawn(|_, _| panic!("durable rendering contacted the live scene")).await;
    let mut config = settings(&live.host(), live.port());
    config.render_worker_host = Some(fake.host());
    config.render_worker_port = fake.port();
    let server = PrintableServer::new(
        Arc::clone(&ws),
        Arc::new(client(&live.host(), live.port())),
        Arc::new(config),
    );
    let submit_arguments = json!({
        "source_blend": "scene.blend",
        "kind": "still",
        "width": 32,
        "height": 24,
        "frame_timeout_seconds": 5.0,
    })
    .as_object()
    .cloned()
    .expect("submit arguments");
    let submitted = server
        .invoke_tool(workflow_call("printable_render_job_submit").with_arguments(submit_arguments))
        .await
        .expect("submit response");
    assert_eq!(submitted.is_error, Some(false));
    let submitted: Value =
        serde_json::from_str(&submitted.content[0].as_text().expect("submit text").text)
            .expect("submit JSON");
    let job_id = submitted["job_id"].as_str().expect("job id").to_string();

    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let arguments = json!({"job_id": job_id})
                .as_object()
                .cloned()
                .expect("status arguments");
            let response = server
                .invoke_tool(workflow_call("printable_render_job_status").with_arguments(arguments))
                .await
                .expect("status response");
            let status: Value =
                serde_json::from_str(&response.content[0].as_text().expect("status text").text)
                    .expect("status JSON");
            if status["state"] == json!("succeeded") {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("job completes");
    assert_eq!(completed["progress"]["completed_frames"], json!(1));
    assert!(live.commands().is_empty());
    assert_eq!(completed["execution"]["mode"], "isolated_worker");

    let arguments = json!({"job_id": job_id})
        .as_object()
        .cloned()
        .expect("artifact arguments");
    let response = server
        .invoke_tool(workflow_call("printable_render_job_artifacts").with_arguments(arguments))
        .await
        .expect("artifact response");
    let artifacts: Value =
        serde_json::from_str(&response.content[0].as_text().expect("artifact text").text)
            .expect("artifact JSON");
    assert_eq!(artifacts["frames"][0]["media_type"], json!("image/png"));
}

fn head_library(variants: &[&str]) -> Arc<printable_server::flame::FlameLibrary> {
    let directory = tempfile::tempdir().unwrap();
    for variant in variants {
        let mut fixture = printable_flame::fixture::Fixture::default();
        fixture.metadata.insert("variant".into(), (*variant).into());
        std::fs::write(
            directory.path().join(format!("{variant}.safetensors")),
            fixture.bytes(),
        )
        .unwrap();
    }
    Arc::new(printable_server::flame::FlameLibrary::load(
        directory.path(),
    ))
}

fn head_server(
    fake: &FakeAddon,
    library: Option<Arc<printable_server::flame::FlameLibrary>>,
) -> PrintableServer {
    let mut cfg = settings(&fake.host(), fake.port());
    cfg.flame = library;
    PrintableServer::new(
        workspace(None),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(cfg),
    )
}

fn head_server_in(
    fake: &FakeAddon,
    library: Arc<printable_server::flame::FlameLibrary>,
    root: &Path,
) -> PrintableServer {
    let mut cfg = settings(&fake.host(), fake.port());
    cfg.flame = Some(library);
    PrintableServer::new(
        workspace(Some(root)),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(cfg),
    )
}

fn head_call(arguments: Value) -> CallToolRequestParams {
    CallToolRequestParams::new("head").with_arguments(arguments.as_object().unwrap().clone())
}

const HEAD_SCENE_STATE: &str =
    r#"{"generation": "11111111-1111-4111-8111-111111111111", "revision": 3}"#;

/// A fake add-on that records head commands and answers like Blender.
async fn recording_addon() -> (FakeAddon, Arc<std::sync::Mutex<Vec<(String, Value)>>>) {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&captured);
    let fake = FakeAddon::spawn(move |command, params| {
        observed.lock().unwrap().push((command, params.clone()));
        let name = params.get("name").or_else(|| params.get("object")).cloned();
        let mut result = json!({"name": name, "type": "MESH", "vertices": 4, "polygons": 4});
        result["scene_state"] = serde_json::from_str(HEAD_SCENE_STATE).unwrap();
        ResponseSpec::Success {
            result,
            addon_version: Some("0.4.0".into()),
        }
    })
    .await;
    (fake, captured)
}

#[tokio::test]
async fn head_create_sends_texture_coordinates_and_a_colour_map() {
    let root = tempfile::tempdir().unwrap();
    image::RgbImage::new(8, 8)
        .save(root.path().join("colour.png"))
        .unwrap();
    let (fake, captured) = recording_addon().await;
    let (_models, textured) = tetrahedron_library(true);
    let server = head_server_in(&fake, textured, root.path());
    for params in [
        json!({"name": "Plain"}),
        json!({"name": "Coloured", "texture": "colour.png"}),
    ] {
        let result = server
            .invoke_tool(head_call(json!({"action": "create", "params": params})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
    }
    let update = server
        .invoke_tool(head_call(json!({
            "action": "update", "params": {"object": "Coloured", "texture": "colour.png"}
        })))
        .await
        .unwrap();
    assert_eq!(update.is_error, Some(false), "{update:?}");
    let calls = captured.lock().unwrap();
    // Every corner of the four fixture triangles carries its coordinates.
    for (_, params) in &calls[..2] {
        let uv = params["uv"].as_array().unwrap();
        assert_eq!(uv.len(), 12);
        assert_eq!(uv[0], json!([0.1f32, 0.1f32]));
        assert_eq!(uv[1], json!([0.5f32, 0.9f32]));
    }
    assert!(calls[0].1.get("texture").is_none());
    assert_eq!(calls[1].1["texture"], "colour.png");
    assert_eq!(calls[2].0, "update_flame_head");
    assert_eq!(calls[2].1["texture"], "colour.png");
}

#[tokio::test]
async fn head_create_passes_its_appearance_to_blender() {
    let root = tempfile::tempdir().unwrap();
    image::RgbImage::new(8, 8)
        .save(root.path().join("colour.png"))
        .unwrap();
    image::GrayImage::new(8, 8)
        .save(root.path().join("detail.png"))
        .unwrap();
    let (fake, captured) = recording_addon().await;
    let (_models, textured) = tetrahedron_library(true);
    let server = head_server_in(&fake, textured, root.path());
    let appearance = json!({
        "subdivision": 2, "close_neck": false, "skin_roughness": 0.4, "skin_subsurface": 0.2,
        "subsurface_scale_mm": 1.5, "eye_roughness": 0.1,
        "bump": {"path": "detail.png", "scale_mm": 6.6, "strength": 1.5}
    });
    let result = server
        .invoke_tool(head_call(json!({"action": "create", "params": {
            "name": "Styled", "texture": "colour.png", "appearance": appearance
        }})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let calls = captured.lock().unwrap();
    assert_eq!(calls[0].1["appearance"], appearance);
}

#[tokio::test]
async fn head_appearance_is_checked_before_blender_is_asked() {
    let root = tempfile::tempdir().unwrap();
    image::RgbImage::new(8, 8)
        .save(root.path().join("colour.png"))
        .unwrap();
    std::fs::write(root.path().join("fake.png"), b"not a png").unwrap();
    let fake =
        FakeAddon::spawn(|_, _| panic!("an invalid appearance must not reach Blender")).await;
    let (_models, textured) = tetrahedron_library(true);
    let server = head_server_in(&fake, textured, root.path());
    for (params, message) in [
        (
            json!({"appearance": {"subdivision": 3}}),
            "appearance.subdivision",
        ),
        (
            json!({"appearance": {"skin_roughness": 2.0}}),
            "appearance.skin_roughness",
        ),
        (json!({"appearance": {"glow": true}}), "glow"),
        (
            json!({"appearance": {"bump": {"path": "colour.png", "scale_mm": 1.0}}}),
            "appearance.bump needs a texture",
        ),
        (
            json!({"texture": "colour.png", "appearance": {"bump": {"path": "fake.png", "scale_mm": 1.0}}}),
            "not a PNG",
        ),
        (
            json!({"texture": "colour.png", "appearance": {"bump": {"path": "colour.png", "scale_mm": -1.0}}}),
            "appearance.bump.scale_mm",
        ),
    ] {
        let result = server
            .invoke_tool(head_call(json!({"action": "create", "params": params})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{params}");
        assert!(
            format!("{result:?}").contains(message),
            "{params}: {result:?}"
        );
    }
}

#[tokio::test]
async fn head_colour_maps_are_checked_before_blender_is_asked() {
    let root = tempfile::tempdir().unwrap();
    image::RgbImage::new(8, 8)
        .save(root.path().join("colour.png"))
        .unwrap();
    std::fs::write(root.path().join("colour.jpg"), b"jpg").unwrap();
    std::fs::write(root.path().join("fake.png"), b"not a png").unwrap();
    image::GrayImage::new(4097, 1)
        .save(root.path().join("wide.png"))
        .unwrap();
    std::fs::File::create(root.path().join("huge.png"))
        .unwrap()
        .set_len(12 * 1024 * 1024 + 1)
        .unwrap();
    let fake = FakeAddon::spawn(|_, _| panic!("invalid colour maps must not reach Blender")).await;
    let (_plain_models, plain) = tetrahedron_library(false);
    let (_models, textured) = tetrahedron_library(true);
    for (library, action, params, message) in [
        (
            &plain,
            "create",
            json!({"texture": "colour.png"}),
            "--texture",
        ),
        (
            &plain,
            "update",
            json!({"object": "Head", "texture": "colour.png"}),
            "--texture",
        ),
        (
            &textured,
            "create",
            json!({"texture": "colour.jpg"}),
            ".png",
        ),
        (
            &textured,
            "create",
            json!({"texture": "fake.png"}),
            "not a PNG image",
        ),
        (
            &textured,
            "update",
            json!({"object": "Head", "texture": "fake.png"}),
            "not a PNG image",
        ),
        (
            &textured,
            "create",
            json!({"texture": "missing.png"}),
            "missing.png",
        ),
        (
            &textured,
            "create",
            json!({"texture": "wide.png"}),
            "4096 pixels",
        ),
        (
            &textured,
            "create",
            json!({"texture": "huge.png"}),
            "read_too_large",
        ),
        (
            &textured,
            "update",
            json!({"object": "Head", "texture": "huge.png"}),
            "read_too_large",
        ),
    ] {
        let server = head_server_in(&fake, Arc::clone(library), root.path());
        let result = server
            .invoke_tool(head_call(json!({"action": action, "params": params})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{params}");
        assert!(
            format!("{result:?}").contains(message),
            "{params}: {result:?}"
        );
    }
}

#[tokio::test]
async fn head_tool_is_advertised_only_with_a_mounted_model() {
    let fake = FakeAddon::spawn(|_, _| panic!("listing must not invoke Blender")).await;
    let advertises = |server: &PrintableServer| {
        server
            .workflow_tools_payload()
            .tools
            .iter()
            .any(|tool| tool.name == "head")
    };
    for library in [None, Some(head_library(&[]))] {
        let server = head_server(&fake, library);
        assert!(!advertises(&server));
        let call = server
            .invoke_tool(head_call(json!({"action": "models", "params": {}})))
            .await;
        assert!(call.is_err(), "an unadvertised tool must not be callable");
    }
    assert!(advertises(&head_server(
        &fake,
        Some(head_library(&["test"]))
    )));
}

#[tokio::test]
async fn head_create_sends_a_millimetre_mesh_matching_the_output_contract() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&captured);
    let fake = FakeAddon::spawn(move |command, params| {
        observed.lock().unwrap().push((command, params.clone()));
        let mut result =
            json!({"name": params["name"], "type": "MESH", "vertices": 4, "polygons": 4});
        result["scene_state"] = serde_json::from_str(HEAD_SCENE_STATE).unwrap();
        ResponseSpec::Success {
            result,
            addon_version: Some("0.4.0".into()),
        }
    })
    .await;
    let server = head_server(&fake, Some(head_library(&["test"])));
    let result = server
        .invoke_tool(head_call(json!({
            "action": "create",
            "params": {"name": "Bust", "identity": [0.5]}
        })))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    let schema = server
        .workflow_tools_payload()
        .tools
        .into_iter()
        .find(|tool| tool.name == "head")
        .unwrap()
        .output_schema
        .unwrap();
    let validator = jsonschema::validator_for(&Value::Object(schema.as_ref().clone())).unwrap();
    assert!(validator.is_valid(&value), "{value}");
    assert_eq!(value["head"]["units"], "millimetres");
    assert_eq!(value["head"]["variant"], "test");

    let calls = captured.lock().unwrap();
    let (command, params) = &calls[0];
    assert_eq!(command, "create_flame_head");
    assert_eq!(params["name"], "Bust");
    assert_eq!(params["variant"], "test");
    // Vertex 0 is (0.25, 0.5, 0) m plus 0.5 of its first identity component
    // (0, 0.375, 0.75), delivered in millimetres.
    assert_eq!(params["vertices"][0], json!([250.0, 687.5, 375.0]));
    assert_eq!(
        params["faces"],
        json!([[0, 1, 2], [0, 1, 3], [0, 2, 3], [1, 2, 3]])
    );
}

#[tokio::test]
async fn head_requests_are_validated_before_blender_is_contacted() {
    let fake = FakeAddon::spawn(|_, _| ResponseSpec::Success {
        result: json!({}),
        addon_version: Some("0.4.0".into()),
    })
    .await;
    let server = head_server(&fake, Some(head_library(&["alpha", "beta"])));
    for (params, message) in [
        (json!({"variant": "alpha", "identity": [6.0]}), "within"),
        (
            json!({"variant": "alpha", "expression": [0.0, 0.0]}),
            "coefficients",
        ),
        (
            json!({"variant": "alpha", "pose": {"jaw": [4.0, 0.0, 0.0]}}),
            "pose.jaw",
        ),
        (json!({"variant": "gamma"}), "unknown head model"),
        (json!({}), "choose a head model"),
    ] {
        let result = server
            .invoke_tool(head_call(json!({"action": "create", "params": params})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{params}");
        assert!(
            format!("{result:?}").contains(message),
            "{params}: {result:?}"
        );
    }
    assert!(
        fake.commands().is_empty(),
        "invalid requests reached Blender"
    );
}

#[tokio::test]
async fn head_update_resends_positions_for_the_named_object() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&captured);
    let fake = FakeAddon::spawn(move |command, params| {
        observed.lock().unwrap().push((command, params.clone()));
        let mut result = json!({"name": params["object"], "type": "MESH"});
        result["scene_state"] = serde_json::from_str(HEAD_SCENE_STATE).unwrap();
        ResponseSpec::Success {
            result,
            addon_version: Some("0.4.0".into()),
        }
    })
    .await;
    let server = head_server(&fake, Some(head_library(&["test"])));
    let result = server
        .invoke_tool(head_call(json!({
            "action": "update",
            "params": {"object": "Bust", "pose": {"global": [0.0, 0.0, 0.1]}}
        })))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let calls = captured.lock().unwrap();
    let (command, params) = &calls[0];
    assert_eq!(command, "update_flame_head");
    assert_eq!(
        (params["object"].as_str(), params["variant"].as_str()),
        (Some("Bust"), Some("test"))
    );
    assert_eq!(params["vertices"].as_array().unwrap().len(), 4);
    assert!(
        params.get("faces").is_none(),
        "updates keep the existing triangles"
    );
}

#[tokio::test]
async fn head_models_reports_mounted_variants_without_blender() {
    let fake = FakeAddon::spawn(|_, _| panic!("listing models must not invoke Blender")).await;
    let server = head_server(&fake, Some(head_library(&["alpha", "beta"])));
    let result = server
        .invoke_tool(head_call(json!({"action": "models", "params": {}})))
        .await
        .unwrap();
    let value = result.structured_content.unwrap();
    let variants: Vec<_> = value["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["variant"].as_str().unwrap())
        .collect();
    assert_eq!(variants, ["alpha", "beta"]);
    assert_eq!(value["models"][0]["identity_components"], 2);
}

fn head_print_server(
    root: &std::path::Path,
    library: Arc<printable_server::flame::FlameLibrary>,
) -> PrintableServer {
    let mut cfg = settings("127.0.0.1", 9);
    cfg.flame = Some(library);
    PrintableServer::new(
        workspace(Some(root)),
        Arc::new(client("127.0.0.1", 9)),
        Arc::new(cfg),
    )
}

fn head_output_validator(server: &PrintableServer) -> jsonschema::Validator {
    let schema = server
        .workflow_tools_payload()
        .tools
        .into_iter()
        .find(|tool| tool.name == "head")
        .unwrap()
        .output_schema
        .unwrap();
    jsonschema::validator_for(&Value::Object(schema.as_ref().clone())).unwrap()
}

#[tokio::test]
async fn head_prepare_print_writes_a_validated_bust_without_blender() {
    // A consistently oriented closed tetrahedron stands in for a head.
    let models = tempfile::tempdir().unwrap();
    let fixture = printable_flame::fixture::Fixture {
        faces: vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
        ..Default::default()
    };
    std::fs::write(models.path().join("test.safetensors"), fixture.bytes()).unwrap();
    let library = Arc::new(printable_server::flame::FlameLibrary::load(models.path()));
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), library);
    let call = json!({
        "action": "prepare_print",
        "params": {"path": "busts/head.stl", "height_mm": 50.0, "edge_length_mm": 2.0}
    });
    let result = server.invoke_tool(head_call(call.clone())).await.unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    assert_eq!(value["variant"], "test");
    assert_eq!(value["validation"]["solid_geometry"], true, "{value}");
    let height = value["bust"]["size_mm"][2].as_f64().unwrap();
    assert!((height - 50.0).abs() < 1e-6, "{value}");
    let written = std::fs::metadata(root.path().join("busts/head.stl")).unwrap();
    assert_eq!(
        written.len(),
        value["artifact"]["size_bytes"].as_u64().unwrap()
    );

    let repeat = server.invoke_tool(head_call(call)).await.unwrap();
    assert_eq!(repeat.is_error, Some(true), "existing files need overwrite");
}

#[tokio::test]
async fn head_prepare_print_can_hollow_the_bust_for_resin() {
    let models = tempfile::tempdir().unwrap();
    let fixture = printable_flame::fixture::Fixture {
        faces: vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
        ..Default::default()
    };
    std::fs::write(models.path().join("test.safetensors"), fixture.bytes()).unwrap();
    let library = Arc::new(printable_server::flame::FlameLibrary::load(models.path()));
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), library);
    let result = server
        .invoke_tool(head_call(json!({
            "action": "prepare_print",
            "params": {
                "path": "hollow.stl", "height_mm": 50.0, "edge_length_mm": 2.0,
                "hollow_wall_mm": 2.0
            }
        })))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    assert_eq!(value["bust"]["hollow"]["drain_diameter_mm"], 3.5);
    assert!(
        value["bust"]["hollow"]["cavity_volume_mm3"]
            .as_f64()
            .unwrap()
            > 0.0
    );
    assert_eq!(value["validation"]["solid_geometry"], true, "{value}");
    assert_eq!(value["validation"]["topology"]["connected_components"], 1);
}

/// A 16-bit greyscale PNG whose pixels come from `level`.
fn displacement_png(width: u32, height: u32, level: impl Fn(u32, u32) -> u16) -> Vec<u8> {
    let image = image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_fn(width, height, |x, y| {
        image::Luma([level(x, y)])
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
}

/// The closed tetrahedron fixture, optionally with texture coordinates.
fn tetrahedron_library(
    with_uv: bool,
) -> (
    tempfile::TempDir,
    Arc<printable_server::flame::FlameLibrary>,
) {
    let models = tempfile::tempdir().unwrap();
    let fixture = printable_flame::fixture::Fixture {
        faces: vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
        uv: with_uv.then(|| {
            (
                vec![0.1, 0.1, 0.9, 0.1, 0.5, 0.9, 0.5, 0.5],
                vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
            )
        }),
        ..Default::default()
    };
    std::fs::write(models.path().join("test.safetensors"), fixture.bytes()).unwrap();
    let library = Arc::new(printable_server::flame::FlameLibrary::load(models.path()));
    (models, library)
}

/// A tetrahedron fixture without its lowest face, so that the bust is
/// trimmed above that opening as above a neck. The apex, vertex 2, is the
/// only vertex above the cut and forms the region `apex`.
fn open_tetrahedron_library(
    with_uv: bool,
) -> (
    tempfile::TempDir,
    Arc<printable_server::flame::FlameLibrary>,
) {
    let models = tempfile::tempdir().unwrap();
    let faces = vec![0, 2, 1, 0, 3, 2, 1, 2, 3];
    let fixture = printable_flame::fixture::Fixture {
        faces: faces.clone(),
        uv: with_uv.then(|| (vec![0.1, 0.1, 0.9, 0.1, 0.5, 0.9, 0.5, 0.5], faces)),
        regions: vec![("apex".into(), vec![2]), ("rim".into(), vec![0, 1, 3])],
        ..Default::default()
    };
    std::fs::write(models.path().join("test.safetensors"), fixture.bytes()).unwrap();
    let library = Arc::new(printable_server::flame::FlameLibrary::load(models.path()));
    (models, library)
}

#[tokio::test]
async fn head_prepare_print_scales_the_default_edge_with_a_small_bust() {
    let (_models, library) = open_tetrahedron_library(false);
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), library);
    for (height, edge) in [(25.0, 0.1), (50.0, 0.2), (150.0, 0.4)] {
        let path = format!("bust-{height}.stl");
        let result = server
            .invoke_tool(head_call(json!({"action": "prepare_print", "params": {
                "path": path, "height_mm": height, "max_triangles": 100_000
            }})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let value = result.structured_content.unwrap();
        let requested = value["bust"]["requested_edge_length_mm"].as_f64().unwrap();
        assert!((requested - edge).abs() < 1e-9, "{height}: {value}");
    }
}

#[tokio::test]
async fn head_prepare_print_follows_the_printer() {
    let (_models, library) = open_tetrahedron_library(false);
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), library);
    let run = |params: Value| {
        server.invoke_tool(head_call(
            json!({"action": "prepare_print", "params": params}),
        ))
    };
    let base = json!({"height_mm": 50.0, "edge_length_mm": 2.0});
    let with = |extra: Value| {
        let mut params = base.clone();
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        params
    };

    let result = run(with(
        json!({"path": "fdm.stl", "printer": {"process": "fdm"}}),
    ))
    .await
    .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    assert_eq!(
        value["printer"],
        json!({"process": "fdm", "nozzle_diameter_mm": 0.4, "layer_height_mm": 0.2,
            "maximum_overhang_degrees": 45.0, "minimum_wall_mm": 0.4})
    );
    assert_eq!(value["validation"]["solid_geometry"], true, "{value}");
    // An FDM bust stands on a plinth 8% of its height, chamfered at the base.
    let plinth = &value["bust"]["plinth"];
    assert!(
        (plinth["height_mm"].as_f64().unwrap() - 4.0).abs() < 1e-9,
        "{value}"
    );
    assert_eq!(plinth["chamfer_mm"], 0.5, "{value}");
    assert!(value["bust"]["overhang"]["area_mm2"].is_number(), "{value}");
    assert!(value["bust"]["thin"]["area_mm2"].is_number(), "{value}");
    assert!(value["warnings"].is_array(), "{value}");
    assert!(value["bust"]["life_size_fraction"].as_f64().unwrap() > 0.0);

    // A plinth height of zero leaves it out; a shallow overhang angle finds
    // regions, each named after the model region nearest it.
    let result = run(with(json!({"path": "steep.stl", "plinth_height_mm": 0.0,
        "printer": {"process": "fdm", "maximum_overhang_degrees": 10.0}})))
    .await
    .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(value["bust"].get("plinth").is_none(), "{value}");
    assert_eq!(value["validation"]["overhang"]["threshold_degrees"], 10.0);
    let regions = value["bust"]["overhang"]["regions"].as_array().unwrap();
    assert!(!regions.is_empty(), "{value}");
    for region in regions {
        assert_eq!(region["region"], "apex", "{value}");
        assert!(region.get("nearest_vertex").is_none(), "{value}");
    }

    // Resin assumes no plinth and checks thickness only when asked.
    let result = run(with(
        json!({"path": "resin.stl", "printer": {"process": "resin"}}),
    ))
    .await
    .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert_eq!(
        value["printer"],
        json!({"process": "resin", "layer_height_mm": 0.05, "maximum_overhang_degrees": 45.0})
    );
    assert!(value["bust"].get("plinth").is_none(), "{value}");
    assert!(value["bust"].get("thin").is_none(), "{value}");

    // Walls thinner than the smallest feature are reported and warned of.
    // The closed tetrahedron hollows with drain holes through its base.
    let (_closed_models, closed) = tetrahedron_library(false);
    let closed_root = tempfile::tempdir().unwrap();
    let closed_server = head_print_server(closed_root.path(), closed);
    let params = with(json!({"path": "hollow.stl", "hollow_wall_mm": 1.0,
        "printer": {"process": "resin", "minimum_wall_mm": 1.5}}));
    let result = closed_server
        .invoke_tool(head_call(
            json!({"action": "prepare_print", "params": params}),
        ))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(
        value["bust"]["thin"]["area_mm2"].as_f64().unwrap() > 0.0,
        "{value}"
    );
    assert!(
        value["warnings"]
            .to_string()
            .contains("thinner than 1.5 mm"),
        "{value}"
    );

    // Without a printer, nothing is assumed and nothing is added.
    let result = run(with(json!({"path": "plain.stl"}))).await.unwrap();
    let value = result.structured_content.unwrap();
    for key in ["printer", "warnings"] {
        assert!(value.get(key).is_none(), "{value}");
    }
    for key in ["plinth", "overhang", "thin"] {
        assert!(value["bust"].get(key).is_none(), "{value}");
    }
    assert!(value["bust"]["life_size_fraction"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn head_prepare_print_adds_a_body_below_the_head() {
    let (_models, library) = open_tetrahedron_library(false);
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), library);
    for (path, extra, kind, height) in [
        (
            "neck.stl",
            json!({"body": "extended_neck", "neck_extension_mm": 8.0}),
            "extended_neck",
            Some(8.0),
        ),
        (
            "shoulders.stl",
            json!({"body": "shoulders"}),
            "shoulders",
            None,
        ),
    ] {
        let mut params = json!({"path": path, "height_mm": 50.0, "edge_length_mm": 2.0});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let result = server
            .invoke_tool(head_call(
                json!({"action": "prepare_print", "params": params}),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let value = result.structured_content.unwrap();
        assert!(head_output_validator(&server).is_valid(&value), "{value}");
        assert_eq!(value["validation"]["solid_geometry"], true, "{value}");
        assert_eq!(
            value["validation"]["topology"]["connected_components"], 1,
            "{value}"
        );
        assert!((value["bust"]["size_mm"][2].as_f64().unwrap() - 50.0).abs() < 1e-6);
        let body = &value["bust"]["body"];
        assert_eq!(body["kind"], kind, "{value}");
        if let Some(height) = height {
            assert_eq!(body["height_mm"], height, "{value}");
        }
    }
}

#[tokio::test]
async fn head_prepare_print_makes_detail_deep_enough_for_the_printer() {
    let (_models, library) = open_tetrahedron_library(true);
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("detail.png"),
        displacement_png(16, 16, |x, y| ((x * 16 + y) * 200) as u16),
    )
    .unwrap();
    let server = head_print_server(root.path(), library);
    let run = |path: &str, displacement: Value| {
        let params = json!({
            "path": path, "height_mm": 50.0, "edge_length_mm": 2.0,
            "printer": {"process": "fdm"}, "displacement": displacement
        });
        server.invoke_tool(head_call(
            json!({"action": "prepare_print", "params": params}),
        ))
    };
    let map = json!({"path": "detail.png", "offset_mm": -2.0, "scale_mm": 4.0});
    // Two 0.2 mm layers, within 1% of the 46 mm above the plinth.
    let result = run("default.stl", map.clone()).await.unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert_eq!(
        value["bust"]["displacement"]["filter"]["relief_mm"], 0.4,
        "{value}"
    );
    assert_eq!(value["validation"]["solid_geometry"], true, "{value}");

    // Detail the caller made shallow is kept, with a warning.
    let mut shallow = map.clone();
    shallow["gain"] = json!(0.01);
    let result = run("shallow.stl", shallow).await.unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(
        value["bust"]["displacement"]["filter"]["relief_mm"].is_null(),
        "{value}"
    );
    assert!(
        value["warnings"]
            .to_string()
            .contains("shallower than one 0.2 mm layer"),
        "{value}"
    );
}

#[tokio::test]
async fn head_expression_scale_softens_the_expression_to_neutral() {
    let (_models, library) = tetrahedron_library(false);
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), library);
    let mut written = Vec::new();
    for (path, params) in [
        ("neutral.obj", json!({})),
        (
            "scaled.obj",
            json!({"expression": [2.0], "expression_scale": 0.0}),
        ),
        (
            "half.obj",
            json!({"expression": [2.0], "expression_scale": 0.5}),
        ),
        ("one.obj", json!({"expression": [1.0]})),
    ] {
        let mut params = params;
        params["path"] = json!(path);
        let result = server
            .invoke_tool(head_call(json!({"action": "export", "params": params})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        written.push(std::fs::read(root.path().join(path)).unwrap());
    }
    assert_eq!(
        written[0], written[1],
        "a scale of 0 gives the neutral face"
    );
    assert_eq!(written[2], written[3], "half of 2 is 1");
    assert_ne!(written[0], written[3]);
}

#[tokio::test]
async fn head_prepare_print_closes_or_recesses_the_mouth_and_fades_detail_on_the_lips() {
    let models = tempfile::tempdir().unwrap();
    let faces = vec![0, 2, 1, 0, 3, 2, 1, 2, 3];
    let fixture = printable_flame::fixture::Fixture {
        faces: faces.clone(),
        uv: Some((vec![0.1, 0.1, 0.9, 0.1, 0.5, 0.9, 0.5, 0.5], faces)),
        // The apex, the only vertex above the cut, stands in for the lips.
        regions: vec![("lips".into(), vec![2])],
        ..Default::default()
    };
    std::fs::write(models.path().join("test.safetensors"), fixture.bytes()).unwrap();
    let library = Arc::new(printable_server::flame::FlameLibrary::load(models.path()));
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("detail.png"),
        displacement_png(16, 16, |_, _| u16::MAX),
    )
    .unwrap();
    let server = head_print_server(root.path(), library);
    let run = |path: &str, extra: Value| {
        let mut params = json!({
            "path": path, "height_mm": 50.0, "edge_length_mm": 2.0,
            "displacement": {"path": "detail.png", "offset_mm": 0.0, "scale_mm": 10.0}
        });
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        server.invoke_tool(head_call(
            json!({"action": "prepare_print", "params": params}),
        ))
    };
    let median = |value: &Value| {
        value["bust"]["displacement"]["percentiles_mm"]["p50"]
            .as_f64()
            .unwrap()
    };
    let posed = run("posed.stl", json!({})).await.unwrap();
    let posed = posed.structured_content.unwrap();
    assert!(posed.get("mouth").is_none(), "{posed}");
    for (shape, extra) in [
        (
            "closed",
            json!({"mouth": "closed", "pose": {"jaw": [0.2, 0.0, 0.0]}}),
        ),
        (
            "recessed",
            json!({"mouth": "recessed", "mouth_recess_mm": 1.0}),
        ),
    ] {
        let result = run(&format!("{shape}.stl"), extra).await.unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let value = result.structured_content.unwrap();
        assert!(head_output_validator(&server).is_valid(&value), "{value}");
        assert_eq!(value["mouth"]["shape"], shape, "{value}");
        assert_eq!(value["mouth"]["detail_faded_over"], "lips", "{value}");
        if shape == "recessed" {
            // The fixture's only opening is its neck.
            assert!(value["mouth"]["recess_mm"].is_null(), "{value}");
            assert!(
                value["mouth"]["note"]
                    .as_str()
                    .is_some_and(|note| note.contains("no mouth opening")),
                "{value}"
            );
        }
        assert_eq!(value["validation"]["solid_geometry"], true, "{value}");
        // Fading the apex leaves the even raise in full only near the rim.
        assert!(median(&value) < 0.9 * median(&posed), "{value} {posed}");
    }
}

#[tokio::test]
async fn head_prepare_print_closes_the_jaw_and_passes_the_recess_to_the_worker() {
    // The open tetrahedron with vertex 3 bound to the jaw, joint 2, which
    // sits on vertex 2: opening the jaw moves vertex 3.
    let models = tempfile::tempdir().unwrap();
    let mut weights = vec![0.0f32; 4 * 5];
    for vertex in 0..3 {
        weights[vertex * 5] = 1.0;
    }
    weights[3 * 5 + 2] = 1.0;
    let fixture = printable_flame::fixture::Fixture {
        faces: vec![0, 2, 1, 0, 3, 2, 1, 2, 3],
        weights,
        ..Default::default()
    };
    std::fs::write(models.path().join("test.safetensors"), fixture.bytes()).unwrap();
    let library = Arc::new(printable_server::flame::FlameLibrary::load(models.path()));
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), library);
    let run = |path: &str, extra: Value| {
        let mut params = json!({"path": path, "height_mm": 50.0, "edge_length_mm": 2.0});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        server.invoke_tool(head_call(
            json!({"action": "prepare_print", "params": params}),
        ))
    };
    let mut written = Vec::new();
    for (path, extra) in [
        ("still.stl", json!({})),
        ("open.stl", json!({"pose": {"jaw": [0.3, 0.0, 0.0]}})),
        (
            "closed.stl",
            json!({"pose": {"jaw": [0.3, 0.0, 0.0]}, "mouth": "closed"}),
        ),
    ] {
        let result = run(path, extra).await.unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        written.push(std::fs::read(root.path().join(path)).unwrap());
    }
    assert_ne!(written[0], written[1], "the jaw moves the fixture");
    assert_eq!(written[0], written[2], "closed replaces the jaw rotation");

    // The fixture has no mouth opening to show a recess, but a recess too
    // deep for a 20 mm bust reaches the worker's limit.
    let result = run(
        "deep.stl",
        json!({"height_mm": 20.0, "mouth": "recessed", "mouth_recess_mm": 10.0}),
    )
    .await
    .unwrap();
    assert_eq!(result.is_error, Some(true), "{result:?}");
    assert!(format!("{result:?}").contains("mouth recess"), "{result:?}");
}

#[tokio::test]
async fn head_prepare_print_writes_the_same_bytes_with_or_without_a_texture_layout() {
    // Appearance must not change the printed shape: without a displacement
    // map, a model converted with --texture prints exactly as one without.
    let params = json!({"path": "bust.stl", "height_mm": 50.0, "edge_length_mm": 2.0});
    let mut written = Vec::new();
    for with_uv in [false, true] {
        let (_models, library) = tetrahedron_library(with_uv);
        let root = tempfile::tempdir().unwrap();
        let server = head_print_server(root.path(), library);
        let result = server
            .invoke_tool(head_call(
                json!({"action": "prepare_print", "params": params}),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        written.push(std::fs::read(root.path().join("bust.stl")).unwrap());
    }
    assert!(!written[0].is_empty());
    assert!(
        written[0] == written[1],
        "the texture layout changed the print"
    );
}

#[tokio::test]
async fn head_prepare_print_raises_detail_from_a_displacement_map() {
    let (_models, library) = tetrahedron_library(true);
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("maps")).unwrap();
    std::fs::write(
        root.path().join("maps/detail.png"),
        displacement_png(16, 16, |_, _| u16::MAX),
    )
    .unwrap();
    let server = head_print_server(root.path(), library);
    let params = json!({
        "path": "detailed.stl", "height_mm": 50.0, "edge_length_mm": 2.0,
        "displacement": {"path": "maps/detail.png", "offset_mm": 0.0, "scale_mm": 20.0}
    });
    let result = server
        .invoke_tool(head_call(
            json!({"action": "prepare_print", "params": params}),
        ))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    assert_eq!(value["validation"]["solid_geometry"], true, "{value}");
    // The fixture is a metre tall at life size, so a 50 mm bust scales the
    // uniform 20 mm raise to 1 mm before smoothing; the smoothed tetrahedron
    // bulges, and fitting it back to 50 mm reduces the raise.
    let applied = &value["bust"]["displacement"];
    let (lowest, highest) = (
        applied["lowest_mm"].as_f64().unwrap(),
        applied["highest_mm"].as_f64().unwrap(),
    );
    assert!((lowest - highest).abs() < 1e-9, "{value}");
    assert!((0.5..1.0).contains(&lowest), "{value}");
}

#[tokio::test]
async fn head_prepare_print_filters_detail_and_reports_how() {
    let (_models, library) = tetrahedron_library(true);
    let root = tempfile::tempdir().unwrap();
    // A one-texel band of extreme values, where the fixture's texture
    // coordinates cross u = 0.5, beside an even 1 mm raise. The band is
    // under a tenth of the texels the fixture's surface covers.
    std::fs::write(
        root.path().join("detail.png"),
        displacement_png(32, 32, |x, _| if x == 16 { u16::MAX } else { 6553 }),
    )
    .unwrap();
    let server = head_print_server(root.path(), library);
    let run = |displacement: Value, path: &str| {
        let params = json!({
            "path": path, "height_mm": 50.0, "edge_length_mm": 2.0, "displacement": displacement
        });
        server.invoke_tool(head_call(
            json!({"action": "prepare_print", "params": params}),
        ))
    };
    let map = json!({"path": "detail.png", "offset_mm": 0.0, "scale_mm": 10.0});
    let plain = run(map.clone(), "plain.stl").await.unwrap();
    assert_eq!(plain.is_error, Some(false), "{plain:?}");
    let plain = plain.structured_content.unwrap();
    let mut clipped = map.clone();
    clipped["clip_percentile"] = json!(90.0);
    clipped["gain"] = json!(2.0);
    let result = run(clipped, "clipped.stl").await.unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    let applied = &value["bust"]["displacement"];
    let filter = &applied["filter"];
    assert_eq!(filter["gain"], 2.0, "{value}");
    assert_eq!(filter["clip_percentile"], 90.0, "{value}");
    assert!(
        (filter["clipped_at_mm"].as_f64().unwrap() - 1.0).abs() < 1e-3,
        "{value}"
    );
    assert!(filter["millimetres_per_texel"].as_f64().unwrap() > 0.0);
    assert!(
        applied["percentiles_mm"]["p50"].as_f64().unwrap() > 0.0,
        "{value}"
    );
    // Clipping removed the extreme column, so the highest raise is about
    // twice the plain bust's even raise rather than its spike.
    let highest = |value: &Value| {
        value["bust"]["displacement"]["highest_mm"]
            .as_f64()
            .unwrap()
    };
    let even = plain["bust"]["displacement"]["percentiles_mm"]["p50"]
        .as_f64()
        .unwrap();
    assert!(highest(&value) < 2.5 * even, "{value} {plain}");
    assert!(highest(&plain) > 5.0 * even, "{plain}");

    let mut relieved = map.clone();
    relieved["relief_mm"] = json!(0.25);
    let result = run(relieved, "relieved.stl").await.unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    let applied = &value["bust"]["displacement"];
    assert_eq!(applied["filter"]["relief_mm"], 0.25, "{value}");
    assert!(applied["relief_scale"].as_f64().unwrap() > 0.0, "{value}");
}

#[tokio::test]
async fn head_prepare_print_rejects_unusable_displacement_maps() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("detail.png"),
        displacement_png(8, 8, |x, _| (x * 4000) as u16),
    )
    .unwrap();
    let rgb = image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    rgb.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    std::fs::write(root.path().join("colour.png"), bytes.into_inner()).unwrap();
    std::fs::write(
        root.path().join("large.png"),
        displacement_png(1025, 2, |_, _| 0),
    )
    .unwrap();
    let map = |path: &str, offset: f64| json!({"path": path, "offset_mm": offset, "scale_mm": 1.0});
    let (_plain_models, plain) = tetrahedron_library(false);
    let (_models, textured) = tetrahedron_library(true);
    for (library, displacement, message) in [
        (
            &plain,
            map("detail.png", 0.0),
            "convert it again with --texture",
        ),
        (&textured, map("colour.png", 0.0), "greyscale PNG"),
        (&textured, map("large.png", 0.0), "2 to 1024 pixels"),
        (&textured, map("missing.png", 0.0), ""),
        (
            &textured,
            map("detail.png", 100.0),
            "5% of the head's height",
        ),
    ] {
        let server = head_print_server(root.path(), Arc::clone(library));
        let params = json!({"path": "head.stl", "displacement": displacement});
        let result = server
            .invoke_tool(head_call(
                json!({"action": "prepare_print", "params": params}),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{params}");
        assert!(
            format!("{result:?}").contains(message),
            "{params}: {result:?}"
        );
        assert!(!root.path().join("head.stl").exists());
    }
}

#[tokio::test]
async fn head_prepare_print_raises_a_hair_cap_over_the_scalp() {
    let library = |scalp: bool| {
        let models = tempfile::tempdir().unwrap();
        let fixture = printable_flame::fixture::Fixture {
            faces: vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
            uv: Some((
                vec![0.1, 0.1, 0.9, 0.1, 0.5, 0.9, 0.5, 0.5],
                vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
            )),
            // The top vertex is the scalp.
            regions: if scalp {
                vec![("scalp".into(), vec![2])]
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        std::fs::write(models.path().join("test.safetensors"), fixture.bytes()).unwrap();
        let library = Arc::new(printable_server::flame::FlameLibrary::load(models.path()));
        (models, library)
    };
    let (_models, with_scalp) = library(true);
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), with_scalp);
    let print = |path: &str, extra: Value| {
        let mut params = json!({"path": path, "height_mm": 50.0, "edge_length_mm": 5.0});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        server.invoke_tool(head_call(
            json!({"action": "prepare_print", "params": params}),
        ))
    };
    let plain = print("plain.stl", json!({})).await.unwrap();
    assert_eq!(plain.is_error, Some(false), "{plain:?}");
    let capped = print("cap.stl", json!({"hair": "cap", "hair_thickness_mm": 3.0}))
        .await
        .unwrap();
    assert_eq!(capped.is_error, Some(false), "{capped:?}");
    let value = capped.structured_content.unwrap();
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    assert_eq!(value["hair"]["style"], "cap");
    assert_eq!(value["hair"]["requested_mm"], 3.0);
    // The thickness applied, after the bust is scaled to its height.
    let applied = value["hair"]["thickness_mm"].as_f64().unwrap();
    assert!(applied > 0.0 && applied <= 3.0 * 1.05, "{value}");
    assert_eq!(value["bust"]["raised_mm"], json!([applied]));
    assert_eq!(value["validation"]["solid_geometry"], true, "{value}");
    // A cap alone carries no detail to report.
    assert!(value["bust"].get("displacement").is_none(), "{value}");
    assert_ne!(
        std::fs::read(root.path().join("plain.stl")).unwrap(),
        std::fs::read(root.path().join("cap.stl")).unwrap()
    );
    // A default thickness is 3% of the height above the plinth.
    let default = print("default.stl", json!({"hair": "cap"}))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert!((default["hair"]["requested_mm"].as_f64().unwrap() - 1.5).abs() < 1e-9);
    // Blending the cap over a model with a few large triangles, refined
    // finely, would take too long, and is refused.
    let refused = print("fine.stl", json!({"hair": "cap", "edge_length_mm": 1.0}))
        .await
        .unwrap();
    assert_eq!(refused.is_error, Some(true));
    assert!(
        format!("{refused:?}").contains("raise edge_length_mm"),
        "{refused:?}"
    );
    // A thickness without a cap, and a model without a scalp, are refused.
    let refused = print("x.stl", json!({"hair_thickness_mm": 2.0}))
        .await
        .unwrap();
    assert!(
        format!("{refused:?}").contains("needs hair cap"),
        "{refused:?}"
    );
    let (_bare_models, bare) = library(false);
    let server = head_print_server(root.path(), bare);
    let refused = server
        .invoke_tool(head_call(json!({"action": "prepare_print", "params": {
            "path": "y.stl", "height_mm": 50.0, "hair": "cap"
        }})))
        .await
        .unwrap();
    assert!(format!("{refused:?}").contains("--masks"), "{refused:?}");
}

#[tokio::test]
async fn head_eyelids_close_with_a_model_that_has_their_basis() {
    let library = |eyelids: bool| {
        let models = tempfile::tempdir().unwrap();
        let mut left = vec![0.0; 12];
        left[2] = 0.01; // vertex 0, 10 mm along z
        let fixture = printable_flame::fixture::Fixture {
            faces: vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
            eyelids: eyelids.then(|| (left, vec![0.0; 12])),
            ..Default::default()
        };
        std::fs::write(models.path().join("test.safetensors"), fixture.bytes()).unwrap();
        let library = Arc::new(printable_server::flame::FlameLibrary::load(models.path()));
        (models, library)
    };
    let (_models, with_basis) = library(true);
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), Arc::clone(&with_basis));
    let models = server
        .invoke_tool(head_call(json!({"action": "models", "params": {}})))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(models["models"][0]["eyelids"], true, "{models}");
    // The first vertex's z, in millimetres, as exported.
    let first_z = |params: Value| {
        let server = &server;
        let root = root.path().to_owned();
        async move {
            let mut params = params;
            params["path"] = json!("head.obj");
            params["overwrite"] = json!(true);
            let result = server
                .invoke_tool(head_call(json!({"action": "export", "params": params})))
                .await
                .unwrap();
            assert_eq!(result.is_error, Some(false), "{result:?}");
            let text = std::fs::read_to_string(root.join("head.obj")).unwrap();
            let line = text.lines().find(|line| line.starts_with("v ")).unwrap();
            line.split(' ').nth(3).unwrap().parse::<f64>().unwrap()
        }
    };
    let open = first_z(json!({})).await;
    let closed = first_z(json!({"eyelids": [1.0, 0.0]})).await;
    let softened = first_z(json!({"eyelids": [1.0, 0.0], "expression_scale": 0.5})).await;
    assert!((closed - open - 10.0).abs() < 1e-3, "{open} {closed}");
    assert!((softened - open - 5.0).abs() < 1e-3, "{open} {softened}");
    // Shares outside 0 to 1, or a model without the basis, are refused.
    let refused = server
        .invoke_tool(head_call(json!({
            "action": "export", "params": {"path": "x.obj", "eyelids": [1.5, 0.0]}
        })))
        .await
        .unwrap();
    assert_eq!(refused.is_error, Some(true));
    assert!(
        format!("{refused:?}").contains("between 0 and 1"),
        "{refused:?}"
    );
    let (_plain_models, plain) = library(false);
    let server = head_print_server(root.path(), plain);
    for action in ["export", "prepare_print"] {
        let path = if action == "export" { "y.obj" } else { "y.stl" };
        let refused = server
            .invoke_tool(head_call(json!({
                "action": action, "params": {"path": path, "eyelids": [0.0, 0.5]}
            })))
            .await
            .unwrap();
        assert_eq!(refused.is_error, Some(true), "{action}");
        assert!(format!("{refused:?}").contains("--eyelids"), "{refused:?}");
    }
}

#[tokio::test]
async fn head_reconstruct_passes_the_eyelids_on_to_a_model_that_can_close_them() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("face.jpg"), b"jpeg").unwrap();
    for basis in [true, false] {
        let library = flame2020_library_edited(|fixture| {
            fixture.eyelids = basis.then(|| (vec![0.0; 12], vec![0.0; 12]));
        });
        let (endpoint, _, task) =
            fake_recon_worker_with(true, json!({"eyelids": [0.25, 0.5]})).await;
        let server = recon_server_with(root.path(), &endpoint, library).await;
        let value = server
            .invoke_tool(head_call(json!({"action": "reconstruct", "params": {
                "images": ["face.jpg"], "displacement_path": "detail.png", "overwrite": true
            }})))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        if basis {
            assert_eq!(value["eyelids"], json!([0.25, 0.5]), "{value}");
        } else {
            assert!(value.get("eyelids").is_none(), "{value}");
        }
        task.abort();
    }
}

#[tokio::test]
async fn head_reconstruct_projects_each_photograph_with_its_eyelids() {
    let root = tempfile::tempdir().unwrap();
    // Red rises across the photograph, so a moved surface takes another red.
    image::RgbImage::from_fn(224, 224, |x, _| image::Rgb([x as u8, 40, 10]))
        .save(root.path().join("face.png"))
        .unwrap();
    let digest: String = {
        use sha2::Digest;
        sha2::Sha256::digest(std::fs::read(root.path().join("face.png")).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    let view = |eyelids: [f32; 2]| {
        json!([{
            "shape": [0.0], "expression": [0.0],
            "jaw": [0.0, 0.0, 0.0], "head_rotation": [0.0, 0.0, 0.0],
            "eyelids": eyelids,
            "camera": [1.0, -0.5, -0.5],
            "crop": {"left": 0.0, "top": 0.0, "step": 1.0},
            "image_size": [224, 224]
        }])
    };
    let map = |basis: bool, eyelids: [f32; 2]| {
        let root = root.path().to_owned();
        let digest = digest.clone();
        let changes = json!({"image_sha256": [digest], "views": view(eyelids)});
        async move {
            // Closing the left eye moves vertex 1 by 4 cm along x.
            let mut left = vec![0.0; 12];
            left[3] = 0.04;
            let library = flame2020_library_edited(|fixture| {
                fixture.uv = Some((
                    vec![0.1, 0.1, 0.9, 0.1, 0.5, 0.9, 0.5, 0.5],
                    vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
                ));
                fixture.eyelids = basis.then(|| (left, vec![0.0; 12]));
            });
            let (endpoint, _, task) = fake_recon_worker_with(true, changes).await;
            let server = recon_server_with(&root, &endpoint, library).await;
            let result = server
                .invoke_tool(head_call(json!({"action": "reconstruct", "params": {
                    "images": ["face.png"], "displacement_path": "detail.png",
                    "texture_path": "colour.png", "overwrite": true, "lighting": "keep"
                }})))
                .await
                .unwrap();
            assert_eq!(result.is_error, Some(false), "{result:?}");
            task.abort();
            std::fs::read(root.join("colour.png")).unwrap()
        }
    };
    // With the basis, a photograph's closed lid moves where colour lands.
    assert_ne!(map(true, [0.0, 0.0]).await, map(true, [1.0, 0.0]).await);
    // Without it, the closure is ignored.
    assert_eq!(map(false, [0.0, 0.0]).await, map(false, [1.0, 0.0]).await);
}

#[tokio::test]
async fn head_export_writes_the_head_as_an_obj_without_blender() {
    for textured in [true, false] {
        let (_models, library) = tetrahedron_library(textured);
        let root = tempfile::tempdir().unwrap();
        let server = head_print_server(root.path(), library);
        let call = json!({
            "action": "export",
            "params": {"path": "heads/head.obj", "identity": [0.5], "pose": {"global": [0.0, 0.3, 0.0]}}
        });
        let result = server.invoke_tool(head_call(call.clone())).await.unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let value = result.structured_content.unwrap();
        assert!(head_output_validator(&server).is_valid(&value), "{value}");
        assert_eq!(value["texture_coordinates"], textured, "{value}");
        assert_eq!(
            (value["vertices"].as_u64(), value["triangles"].as_u64()),
            (Some(4), Some(4))
        );
        assert_eq!(value["head"]["units"], "millimetres");

        let text = std::fs::read_to_string(root.path().join("heads/head.obj")).unwrap();
        let lines = |prefix: &str| text.lines().filter(|line| line.starts_with(prefix)).count();
        assert_eq!(lines("v "), 4, "{text}");
        assert_eq!(lines("vt "), if textured { 4 } else { 0 }, "{text}");
        assert_eq!(lines("f "), 4, "{text}");
        // Faces index from one; textured faces also name their coordinates.
        let first_face = text.lines().find(|line| line.starts_with("f ")).unwrap();
        assert_eq!(
            first_face,
            if textured { "f 1/1 3/3 2/2" } else { "f 1 3 2" }
        );
        // Vertices are in millimetres: the fixture spans a metre.
        let largest = text
            .lines()
            .filter_map(|line| line.strip_prefix("v "))
            .flat_map(|line| {
                line.split(' ')
                    .map(|value| value.parse::<f64>().unwrap().abs())
            })
            .fold(0.0, f64::max);
        assert!(largest > 500.0, "{text}");

        let repeat = server.invoke_tool(head_call(call)).await.unwrap();
        assert_eq!(repeat.is_error, Some(true), "existing files need overwrite");
    }
    let (_models, library) = tetrahedron_library(true);
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), library);
    let wrong_suffix = server
        .invoke_tool(head_call(
            json!({"action": "export", "params": {"path": "head.stl"}}),
        ))
        .await
        .unwrap();
    assert_eq!(wrong_suffix.is_error, Some(true));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn head_prepare_print_rejects_invalid_options_before_running_the_worker() {
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), head_library(&["test"]));
    for (params, message) in [
        (json!({"path": "head.stl", "height_mm": 5.0}), "height_mm"),
        (
            json!({"path": "head.stl", "edge_length_mm": 0.01}),
            "edge_length_mm",
        ),
        (
            json!({"path": "head.stl", "max_triangles": 10}),
            "max_triangles",
        ),
        (json!({"path": "head.stl", "identity": [9.0]}), "within"),
        (
            json!({"path": "head.stl", "pose": {"global": [0.0, 0.1, 0.0]}}),
            "pose.global",
        ),
        (
            json!({"path": "head.stl", "plinth_diameter_mm": 40.0}),
            "require a plinth",
        ),
        (
            json!({"path": "head.stl", "plinth_chamfer_mm": 1.0}),
            "require a plinth",
        ),
        (
            json!({"path": "head.stl", "plinth_height_mm": 0.0, "plinth_chamfer_mm": 1.0,
                "printer": {"process": "fdm"}}),
            "require a plinth",
        ),
        (
            json!({"path": "head.stl", "plinth_height_mm": 3.0, "plinth_chamfer_mm": 4.0}),
            "less than the plinth's height",
        ),
        (
            json!({"path": "head.stl", "plinth_height_mm": 8.0, "plinth_chamfer_mm": 6.0}),
            "plinth_chamfer_mm",
        ),
        (
            json!({"path": "head.stl", "printer": {"process": "resin", "nozzle_diameter_mm": 0.4}}),
            "applies only to fdm",
        ),
        (
            json!({"path": "head.stl", "printer": {"process": "fdm", "nozzle_diameter_mm": 0.4,
                "layer_height_mm": 0.5}}),
            "must not exceed printer.nozzle_diameter_mm",
        ),
        (
            json!({"path": "head.stl", "printer": {"process": "fdm", "nozzle_diameter_mm": 3.0}}),
            "printer.nozzle_diameter_mm",
        ),
        (
            json!({"path": "head.stl", "printer": {"process": "fdm", "maximum_overhang_degrees": 95.0}}),
            "printer.maximum_overhang_degrees",
        ),
        (
            json!({"path": "head.stl", "printer": {"process": "resin", "minimum_wall_mm": 5.0}}),
            "printer.minimum_wall_mm",
        ),
        (
            json!({"path": "head.stl", "expression_scale": 1.5}),
            "expression_scale",
        ),
        (
            json!({"path": "head.stl", "mouth_recess_mm": 2.0}),
            "requires mouth recessed",
        ),
        (
            json!({"path": "head.stl", "mouth": "recessed", "mouth_recess_mm": 20.0}),
            "mouth_recess_mm",
        ),
        (
            json!({"path": "head.stl", "pupil_diameter_mm": 1.0}),
            "need eyes pupil or iris_and_pupil",
        ),
        (
            json!({"path": "head.stl", "eyes": "pupil", "iris_diameter_mm": 3.0}),
            "iris_diameter_mm needs eyes iris_and_pupil",
        ),
        (
            json!({"path": "head.stl", "eyes": "pupil", "pupil_diameter_mm": 20.0}),
            "pupil_diameter_mm",
        ),
        (
            json!({"path": "head.stl", "eyes": "iris_and_pupil", "eye_detail_depth_mm": 0.0}),
            "eye_detail_depth_mm",
        ),
        (
            json!({"path": "head.stl", "eyes": "pupil"}),
            "no separate eyeballs",
        ),
        (
            json!({"path": "head.stl", "neck_extension_mm": 10.0}),
            "needs body extended_neck",
        ),
        (
            json!({"path": "head.stl", "body": "extended_neck", "shoulder_width_mm": 80.0}),
            "need body shoulders",
        ),
        (
            json!({"path": "head.stl", "body": "extended_neck", "neck_extension_mm": 500.0}),
            "neck_extension_mm",
        ),
        (
            json!({"path": "head.stl", "height_mm": 30.0, "plinth_height_mm": 25.0}),
            "must exceed plinth_height_mm",
        ),
        (
            json!({"path": "head.stl", "drain_hole_diameter_mm": 3.0}),
            "requires hollow_wall_mm",
        ),
        (
            json!({"path": "head.stl", "hollow_wall_mm": 0.5}),
            "hollow_wall_mm",
        ),
        (json!({"path": "head.txt"}), ""),
        (
            json!({"path": "head.stl", "displacement": {
                "path": "d.png", "offset_mm": 0.0, "scale_mm": 1.0, "gain": 2.0, "relief_mm": 0.2}}),
            "gain or relief_mm, not both",
        ),
        (
            json!({"path": "head.stl", "displacement": {
                "path": "d.png", "offset_mm": 0.0, "scale_mm": 1.0, "high_pass_mm": 0.5}}),
            "displacement.high_pass_mm",
        ),
        (
            json!({"path": "head.stl", "displacement": {
                "path": "d.png", "offset_mm": 0.0, "scale_mm": 1.0, "clip_percentile": 20.0}}),
            "displacement.clip_percentile",
        ),
        (
            json!({"path": "head.stl", "displacement": {
                "path": "d.png", "offset_mm": 0.0, "scale_mm": 1.0, "gain": -1.0}}),
            "displacement.gain",
        ),
        (
            json!({"path": "head.stl", "displacement": {
                "path": "d.png", "offset_mm": 0.0, "scale_mm": 1.0, "relief_mm": 50.0}}),
            "displacement.relief_mm",
        ),
    ] {
        let result = server
            .invoke_tool(head_call(
                json!({"action": "prepare_print", "params": params}),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{params}");
        assert!(
            format!("{result:?}").contains(message),
            "{params}: {result:?}"
        );
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

/// Prepares a print-resolution bust from a converted model named by
/// `PRINTABLE_TEST_FLAME_MODEL`, under the default worker memory limit.
#[tokio::test]
#[ignore = "requires a converted FLAME model"]
async fn head_prepare_print_makes_a_printable_bust_from_a_real_model() {
    let model = std::env::var_os("PRINTABLE_TEST_FLAME_MODEL")
        .expect("set PRINTABLE_TEST_FLAME_MODEL to a converted .safetensors model");
    let models = tempfile::tempdir().unwrap();
    std::fs::copy(&model, models.path().join("model.safetensors")).unwrap();
    let library = Arc::new(printable_server::flame::FlameLibrary::load(models.path()));
    assert!(library.is_available(), "{}", library.report());
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), library);
    let started = std::time::Instant::now();
    let result = server
        .invoke_tool(head_call(json!({
            "action": "prepare_print",
            "params": {
                "path": "bust.stl",
                "identity": [1.0, -0.5, 0.5],
                "expression": [1.5, 0.5],
                "pose": {"jaw": [0.15, 0.0, 0.0], "neck": [0.1, 0.2, 0.0]}
            }
        })))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    eprintln!(
        "bust in {:?}: {}\nvalidation topology: {}",
        started.elapsed(),
        value["bust"],
        value["validation"]["topology"]
    );
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    assert_eq!(value["validation"]["solid_geometry"], true, "{value}");
    assert_eq!(
        value["validation"]["topology"]["connected_components"], 1,
        "{value}"
    );
    assert!((value["bust"]["size_mm"][2].as_f64().unwrap() - 100.0).abs() < 1e-6);
    assert!(
        value["bust"]["estimated_deviation_p999_mm"]
            .as_f64()
            .unwrap()
            < 0.05
    );
    assert!(value["bust"]["triangles"].as_u64().unwrap() <= 750_000);
}

/// Prepares a hollow bust on a plinth from the model named by
/// `PRINTABLE_TEST_FLAME_MODEL`, under the default worker memory limit.
#[tokio::test]
#[ignore = "requires a converted FLAME model"]
async fn head_prepare_print_hollows_a_real_bust_on_a_plinth() {
    let model = std::env::var_os("PRINTABLE_TEST_FLAME_MODEL")
        .expect("set PRINTABLE_TEST_FLAME_MODEL to a converted .safetensors model");
    let models = tempfile::tempdir().unwrap();
    std::fs::copy(&model, models.path().join("model.safetensors")).unwrap();
    let library = Arc::new(printable_server::flame::FlameLibrary::load(models.path()));
    let root = tempfile::tempdir().unwrap();
    let server = head_print_server(root.path(), library);
    let started = std::time::Instant::now();
    let result = server
        .invoke_tool(head_call(json!({
            "action": "prepare_print",
            "params": {
                "path": "bust.stl",
                "identity": [0.5, 1.0],
                "plinth_height_mm": 12.0,
                "hollow_wall_mm": 2.5
            }
        })))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    eprintln!(
        "bust in {:?}: {}\nvalidation topology: {}",
        started.elapsed(),
        value["bust"],
        value["validation"]["topology"]
    );
    assert_eq!(value["validation"]["solid_geometry"], true, "{value}");
    assert_eq!(
        value["validation"]["topology"]["connected_components"], 1,
        "{value}"
    );
    assert!((value["bust"]["size_mm"][2].as_f64().unwrap() - 100.0).abs() < 1e-6);
    assert_eq!(value["bust"]["plinth"]["height_mm"], 12.0);
    assert!(
        !value["bust"]["hollow"]["drain_holes_mm"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// A FLAME 2020 generic stand-in: the fixture with that release's digest.
fn flame2020_library() -> Arc<printable_server::flame::FlameLibrary> {
    flame2020_library_with_uv(false)
}

fn flame2020_library_with_uv(textured: bool) -> Arc<printable_server::flame::FlameLibrary> {
    flame2020_library_with_regions(textured, Vec::new())
}

/// The FLAME 2020 fixture with named vertex regions.
fn flame2020_library_with_regions(
    textured: bool,
    regions: Vec<(String, Vec<u32>)>,
) -> Arc<printable_server::flame::FlameLibrary> {
    flame2020_library_edited(|fixture| {
        fixture.uv = textured.then(|| {
            (
                vec![0.1, 0.1, 0.9, 0.1, 0.5, 0.9, 0.5, 0.5],
                vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
            )
        });
        fixture.regions = regions;
    })
}

/// The FLAME 2020 fixture, changed by `edit`.
fn flame2020_library_edited(
    edit: impl FnOnce(&mut printable_flame::fixture::Fixture),
) -> Arc<printable_server::flame::FlameLibrary> {
    let models = tempfile::tempdir().unwrap();
    // The published model's split: 300 identity and 100 expression components.
    let mut fixture = printable_flame::fixture::Fixture {
        shapedirs: vec![0.0; 4 * 3 * 400],
        ..Default::default()
    };
    edit(&mut fixture);
    for (key, value) in [
        ("variant", "flame2020_generic"),
        (
            "source_sha256",
            printable_server::flame::FLAME_2020_GENERIC_SHA256,
        ),
        ("identity_components", "300"),
        ("expression_components", "100"),
    ] {
        fixture.metadata.insert(key.into(), value.into());
    }
    std::fs::write(
        models.path().join("flame2020_generic.safetensors"),
        fixture.bytes(),
    )
    .unwrap();
    Arc::new(printable_server::flame::FlameLibrary::load(models.path()))
}

/// A reconstruction worker that is ready and answers with fixed coefficients.
async fn fake_recon_worker(
    ready: bool,
) -> (
    String,
    Arc<std::sync::Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    fake_recon_worker_with(ready, json!({})).await
}

/// As [`fake_recon_worker`], with `changes` replacing fields of its answer.
/// A `_delay_ms` change delays the answer instead.
async fn fake_recon_worker_with(
    ready: bool,
    changes: Value,
) -> (
    String,
    Arc<std::sync::Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{Json, Router, routing::get, routing::post};
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let app = Router::new()
        .route(
            "/readyz",
            get(move || async move {
                if ready {
                    axum::http::StatusCode::OK
                } else {
                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        )
        .route(
            "/reconstruct",
            post(move |Json(request): Json<Value>| {
                let recorded = Arc::clone(&recorded);
                let mut changes = changes.clone();
                async move {
                    recorded.lock().unwrap().push(request.clone());
                    if let Some(delay) = changes.as_object_mut().unwrap().remove("_delay_ms") {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            delay.as_u64().unwrap(),
                        ))
                        .await;
                    }
                    let mut answer = json!({
                        "identity": vec![0.25; 300],
                        "expression": vec![0.5; 50],
                        "jaw": [0.1, 0.0, 0.0],
                        "head_rotation": [0.0, 0.1, 0.0],
                        "identity_agreement": [1.0],
                        "clamped_coefficients": 0,
                        // The fixture head spans 0 to 1 m in x and y; this
                        // camera centres it in a 224 pixel photograph.
                        "views": [{
                            "shape": [0.0], "expression": [0.0],
                            "jaw": [0.0, 0.0, 0.0], "head_rotation": [0.0, 0.0, 0.0],
                            "camera": [1.0, -0.5, -0.5],
                            "crop": {"left": 0.0, "top": 0.0, "step": 1.0},
                            "image_size": [224, 224]
                        }],
                        "displacement": {
                            "offset_mm": -1.0, "scale_mm": 2.0, "width": 256, "height": 256,
                            "artifact": {"path": request["displacement_path"], "size_bytes": 10,
                                "media_type": "image/png", "modified_ns": 1}
                        }
                    });
                    for (key, value) in changes.as_object().unwrap() {
                        if value.is_null() {
                            answer.as_object_mut().unwrap().remove(key);
                        } else {
                            answer[key] = value.clone();
                        }
                    }
                    Json(answer)
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), seen, task)
}

async fn recon_server(root: &Path, endpoint: &str) -> PrintableServer {
    recon_server_with(root, endpoint, flame2020_library()).await
}

async fn recon_server_with(
    root: &Path,
    endpoint: &str,
    library: Arc<printable_server::flame::FlameLibrary>,
) -> PrintableServer {
    let mut cfg = settings("127.0.0.1", 9);
    cfg.flame = Some(library);
    let recon = printable_server::recon::ReconClient::configure(&|name| {
        (name == "PRINTABLE_RECON_ENDPOINT").then(|| endpoint.to_owned())
    })
    .unwrap()
    .unwrap();
    recon.refresh().await;
    cfg.recon = Some(recon);
    PrintableServer::new(
        workspace(Some(root)),
        Arc::new(client("127.0.0.1", 9)),
        Arc::new(cfg),
    )
}

fn head_offers_reconstruct(server: &PrintableServer) -> bool {
    server
        .workflow_tools_payload()
        .tools
        .iter()
        .find(|tool| tool.name == "head")
        .is_some_and(|tool| {
            serde_json::to_string(&*tool.input_schema)
                .unwrap()
                .contains("reconstruct")
        })
}

#[tokio::test]
async fn head_reconstruct_is_offered_only_when_the_worker_is_ready() {
    let root = tempfile::tempdir().unwrap();
    let (endpoint, _, task) = fake_recon_worker(false).await;
    let server = recon_server(root.path(), &endpoint).await;
    assert!(!head_offers_reconstruct(&server));
    task.abort();

    let (endpoint, _, task) = fake_recon_worker(true).await;
    let server = recon_server(root.path(), &endpoint).await;
    assert!(head_offers_reconstruct(&server));
    task.abort();

    // A model other than FLAME 2020 generic cannot take the coefficients.
    let (endpoint, _, task) = fake_recon_worker(true).await;
    let mut cfg = settings("127.0.0.1", 9);
    cfg.flame = Some(head_library(&["flame2023_open"]));
    let recon = printable_server::recon::ReconClient::configure(&|name| {
        (name == "PRINTABLE_RECON_ENDPOINT").then(|| endpoint.clone())
    })
    .unwrap()
    .unwrap();
    recon.refresh().await;
    cfg.recon = Some(recon);
    let server = PrintableServer::new(
        workspace(Some(root.path())),
        Arc::new(client("127.0.0.1", 9)),
        Arc::new(cfg),
    );
    assert!(!head_offers_reconstruct(&server));
    task.abort();
}

#[tokio::test]
async fn head_reconstruct_returns_coefficients_for_the_flame_2020_model() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("face.jpg"), b"jpeg").unwrap();
    let (endpoint, seen, task) = fake_recon_worker(true).await;
    let server = recon_server(root.path(), &endpoint).await;
    let result = server
        .invoke_tool(head_call(json!({
            "action": "reconstruct",
            "params": {"images": ["face.jpg"], "displacement_path": "detail.png"}
        })))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    assert_eq!(value["variant"], "flame2020_generic");
    assert_eq!(value["identity"].as_array().unwrap().len(), 300);
    assert_eq!(value["pose"]["jaw"], json!([0.1, 0.0, 0.0]));
    assert_eq!(seen.lock().unwrap()[0]["images"], json!(["face.jpg"]));

    // Requests are checked before the worker is asked.
    for (params, message) in [
        (
            json!({"images": [], "displacement_path": "d.png"}),
            "between 1",
        ),
        (
            json!({"images": ["face.gif"], "displacement_path": "d.png"}),
            "photograph",
        ),
        (
            json!({"images": ["absent.jpg"], "displacement_path": "d.png"}),
            "absent.jpg",
        ),
        (
            json!({"images": ["face.jpg"], "displacement_path": "d.stl"}),
            ".png",
        ),
    ] {
        let result = server
            .invoke_tool(head_call(
                json!({"action": "reconstruct", "params": params}),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{params}");
        assert!(
            format!("{result:?}").contains(message),
            "{params}: {result:?}"
        );
    }
    assert_eq!(seen.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test]
async fn head_reconstruct_reports_the_primary_photograph() {
    let root = tempfile::tempdir().unwrap();
    for name in ["face.jpg", "side.jpg"] {
        std::fs::write(root.path().join(name), b"jpeg").unwrap();
    }
    let params = |extra: Value| {
        let mut params = json!({
            "images": ["face.jpg", "side.jpg"], "displacement_path": "detail.png", "overwrite": true
        });
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"action": "reconstruct", "params": params})
    };
    // A worker that does not report it used the first; one that does is
    // believed; the request's choice is passed on.
    for (answer, asked, expected) in [
        (json!({}), json!({}), (0, "face.jpg")),
        (
            json!({"primary_image": 1}),
            json!({"primary_image": "most_frontal"}),
            (1, "side.jpg"),
        ),
    ] {
        let (endpoint, seen, task) = fake_recon_worker_with(true, answer).await;
        let server = recon_server(root.path(), &endpoint).await;
        let result = server
            .invoke_tool(head_call(params(asked.clone())))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let value = result.structured_content.unwrap();
        assert_eq!(
            value["primary_image"],
            json!({"index": expected.0, "image": expected.1}),
            "{value}"
        );
        assert_eq!(
            seen.lock().unwrap()[0].get("primary_image"),
            asked.get("primary_image"),
        );
        task.abort();
    }
    // A worker that names a photograph it was not given is not believed.
    let (endpoint, _, task) = fake_recon_worker_with(true, json!({"primary_image": 2})).await;
    let server = recon_server(root.path(), &endpoint).await;
    let result = server
        .invoke_tool(head_call(params(json!({}))))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(format!("{result:?}").contains("primary photograph that was not given"));
    // An index beyond the photographs is refused before the worker is asked.
    let (endpoint, seen, task_two) = fake_recon_worker(true).await;
    let server = recon_server(root.path(), &endpoint).await;
    let result = server
        .invoke_tool(head_call(params(json!({"primary_image": 2}))))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(
        format!("{result:?}").contains("not one of the 2 images"),
        "{result:?}"
    );
    assert!(seen.lock().unwrap().is_empty());
    task.abort();
    task_two.abort();
}

#[tokio::test]
async fn head_reconstruct_suggests_a_neck_pose_from_the_head_rotation() {
    // The fake worker reports the head turned 0.1 radians about y.
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("face.jpg"), b"jpeg").unwrap();
    let (endpoint, seen, task) = fake_recon_worker(true).await;
    let server = recon_server(root.path(), &endpoint).await;
    let run = |extra: Value| {
        let mut params =
            json!({"images": ["face.jpg"], "displacement_path": "detail.png", "overwrite": true});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        server.invoke_tool(head_call(
            json!({"action": "reconstruct", "params": params}),
        ))
    };
    let neck = |value: &Value| -> Vec<f64> {
        value["pose"]["neck"]
            .as_array()
            .unwrap()
            .iter()
            .map(|axis| axis.as_f64().unwrap())
            .collect()
    };
    let plain = run(json!({})).await.unwrap().structured_content.unwrap();
    assert!(plain["pose"].get("neck").is_none(), "{plain}");
    // The turn is kept only when asked for; a tilt alone drops it.
    let turned = run(json!({"neck_turn": 1.0}))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let suggested = neck(&turned);
    assert!(
        (suggested[1] - 0.1).abs() < 1e-9 && suggested[0].abs() < 1e-9,
        "{turned}"
    );
    let tilted = run(json!({"neck_tilt": 1.0}))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert!(
        neck(&tilted).iter().all(|axis| axis.abs() < 1e-9),
        "{tilted}"
    );
    let refused = run(json!({"neck_tilt": 2.0})).await.unwrap();
    assert_eq!(refused.is_error, Some(true), "{refused:?}");
    assert!(format!("{refused:?}").contains("neck_tilt"), "{refused:?}");
    // The server applies the shares; the worker is never sent them.
    let sent = seen.lock().unwrap();
    assert_eq!(sent.len(), 3);
    assert!(
        sent.iter()
            .all(|body| body.get("neck_tilt").is_none() && body.get("neck_turn").is_none()),
        "{sent:?}"
    );
    drop(sent);
    task.abort();
}

#[tokio::test]
async fn head_reconstruct_colours_skin_from_skin_and_the_scalp_as_asked() {
    let root = tempfile::tempdir().unwrap();
    image::RgbImage::from_pixel(224, 224, image::Rgb([200, 40, 10]))
        .save(root.path().join("face.png"))
        .unwrap();
    let digest: String = {
        use sha2::Digest;
        sha2::Sha256::digest(std::fs::read(root.path().join("face.png")).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    // Labels saying every pixel shows hair.
    let labels = |label: u8| {
        let mut png = std::io::Cursor::new(Vec::new());
        image::GrayImage::from_pixel(16, 16, image::Luma([label]))
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(png.into_inner())
    };
    let view = |label: u8, classes: Value| {
        json!([{
            "shape": [0.0], "expression": [0.0],
            "jaw": [0.0, 0.0, 0.0], "head_rotation": [0.0, 0.0, 0.0],
            "camera": [1.0, -0.5, -0.5],
            "crop": {"left": 0.0, "top": 0.0, "step": 1.0},
            "image_size": [224, 224],
            "segmentation": {
                "labels_png": labels(label), "classes": classes,
                "crop": {"left": 0.0, "top": 0.0, "step": 16.0 / 224.0},
                "hair_rgb": [10, 20, 30], "hair_pixels": 256
            }
        }])
    };
    let classes = json!([
        "background",
        "hair",
        "body_skin",
        "face_skin",
        "clothes",
        "other"
    ]);
    let scalp_everywhere = vec![("scalp".to_owned(), vec![0, 1, 2, 3])];
    let mean = |path: &str| -> Vec<f64> {
        let map = image::open(root.path().join(path)).unwrap().into_rgb8();
        let count = f64::from(map.width() * map.height());
        (0..3)
            .map(|channel| {
                map.pixels()
                    .map(|pixel| f64::from(pixel.0[channel]))
                    .sum::<f64>()
                    / count
            })
            .collect()
    };
    for (index, (regions, label, params, applied, colour)) in [
        // Hair over a scalp painted with the hair the photograph shows.
        (
            scalp_everywhere.clone(),
            1,
            json!({}),
            "hair",
            Some([10.0, 20.0, 30.0]),
        ),
        // A caller's own hair colour.
        (
            scalp_everywhere.clone(),
            1,
            json!({"hair_colour": [90, 60, 30]}),
            "hair",
            Some([90.0, 60.0, 30.0]),
        ),
        // Hair taken as photographed.
        (
            scalp_everywhere.clone(),
            1,
            json!({"scalp": "photo"}),
            "photo",
            Some([200.0, 40.0, 10.0]),
        ),
        // Bare scalp keeps the skin it shows; the fixture shows little of
        // its surface, so the unseen rest is painted hair.
        (scalp_everywhere.clone(), 3, json!({}), "hair", None),
        // Without a scalp region the whole head is skin.
        (Vec::new(), 3, json!({}), "skin", Some([200.0, 40.0, 10.0])),
        // Even when hair is asked to be taken as photographed.
        (Vec::new(), 1, json!({"scalp": "photo"}), "skin", None),
    ]
    .into_iter()
    .enumerate()
    {
        let (endpoint, _, task) = fake_recon_worker_with(
            true,
            json!({"image_sha256": [digest.clone()], "views": view(label, classes.clone())}),
        )
        .await;
        let server = recon_server_with(
            root.path(),
            &endpoint,
            flame2020_library_with_regions(true, regions),
        )
        .await;
        let mut request = json!({
            "images": ["face.png"], "displacement_path": format!("d{index}.png"),
            "texture_path": format!("c{index}.png")
        });
        for (key, value) in params.as_object().unwrap() {
            request[key] = value.clone();
        }
        let result = server
            .invoke_tool(head_call(
                json!({"action": "reconstruct", "params": request}),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{index}: {result:?}");
        let value = result.structured_content.unwrap();
        assert!(head_output_validator(&server).is_valid(&value), "{value}");
        let texture = &value["texture"];
        assert_eq!(texture["skin_only"], true, "{value}");
        assert_eq!(texture["scalp"]["applied"], applied, "{index}: {value}");
        assert_eq!(
            texture["scalp"]["hair_found"],
            json!([10, 20, 30]),
            "{value}"
        );
        // Hair-labelled pixels colour nothing unless taken as photographed.
        let seen = texture["seen"].as_f64().unwrap();
        assert_eq!(
            seen > 0.0,
            label != 1 || applied == "photo",
            "{index}: {value}"
        );
        let observed = mean(&format!("c{index}.png"));
        match colour {
            Some(colour) => {
                for (channel, expected) in observed.iter().zip(colour) {
                    assert!(
                        (channel - expected).abs() < 2.0,
                        "{index}: {observed:?} {value}"
                    );
                }
            }
            // Where the photograph sees bare scalp it keeps the skin, so
            // the map is redder than hair alone.
            None => assert!(observed[0] > 13.0, "{index}: {observed:?} {value}"),
        }
        task.abort();
    }

    // Labels with other classes, or larger than a label square, are
    // refused before they are decoded, and the map is not made.
    let mut oversized = view(3, classes.clone());
    let mut png = std::io::Cursor::new(Vec::new());
    image::GrayImage::new(2048, 2048)
        .write_to(&mut png, ImageFormat::Png)
        .unwrap();
    oversized[0]["segmentation"]["labels_png"] =
        json!(base64::engine::general_purpose::STANDARD.encode(png.into_inner()));
    for (index, (views, message)) in [
        (view(3, json!(["background", "skin"])), "unexpected classes"),
        (oversized, "sensible size"),
    ]
    .into_iter()
    .enumerate()
    {
        let (endpoint, _, task) = fake_recon_worker_with(
            true,
            json!({"image_sha256": [digest.clone()], "views": views}),
        )
        .await;
        let server = recon_server_with(
            root.path(),
            &endpoint,
            flame2020_library_with_regions(true, scalp_everywhere.clone()),
        )
        .await;
        let result = server
            .invoke_tool(head_call(json!({"action": "reconstruct", "params": {
                "images": ["face.png"], "displacement_path": format!("dx{index}.png"),
                "texture_path": format!("cx{index}.png")
            }})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(format!("{result:?}").contains(message), "{result:?}");
        assert!(!root.path().join(format!("cx{index}.png")).exists());
        task.abort();
    }
}

#[tokio::test]
async fn head_reconstruct_projects_the_photographs_into_a_colour_map() {
    let root = tempfile::tempdir().unwrap();
    image::RgbImage::from_pixel(224, 224, image::Rgb([200, 40, 10]))
        .save(root.path().join("face.png"))
        .unwrap();
    let digest = |bytes: &[u8]| -> String {
        use sha2::Digest;
        sha2::Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    let photo_digest = digest(&std::fs::read(root.path().join("face.png")).unwrap());
    let (endpoint, seen, task) =
        fake_recon_worker_with(true, json!({"image_sha256": [photo_digest]})).await;
    let server = recon_server_with(root.path(), &endpoint, flame2020_library_with_uv(true)).await;
    let params = json!({
        "images": ["face.png"], "displacement_path": "detail.png", "texture_path": "colour.png"
    });
    let result = server
        .invoke_tool(head_call(
            json!({"action": "reconstruct", "params": params}),
        ))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    let texture = &value["texture"];
    assert_eq!(
        (texture["width"].as_u64(), texture["height"].as_u64()),
        (Some(1024), Some(1024))
    );
    assert!(texture["seen"].as_f64().unwrap() > 0.1, "{value}");
    // Lighting is removed by default, and the shading found is reported for
    // the one photograph.
    assert_eq!(texture["lighting"]["requested"], "remove", "{value}");
    assert_eq!(
        texture["lighting"]["shading"].as_array().map(Vec::len),
        Some(1),
        "{value}"
    );
    // A single-coloured photograph colours the whole map, including what it
    // cannot see, and has no shading to remove.
    let map = image::open(root.path().join("colour.png"))
        .unwrap()
        .into_rgb8();
    assert_eq!(map.dimensions(), (1024, 1024));
    let count = f64::from(map.width() * map.height());
    let mean: Vec<f64> = (0..3)
        .map(|channel| {
            map.pixels()
                .map(|pixel| f64::from(pixel.0[channel]))
                .sum::<f64>()
                / count
        })
        .collect();
    for (channel, expected) in mean.iter().zip([200.0, 40.0, 10.0]) {
        assert!((channel - expected).abs() < 2.0, "{mean:?}");
    }

    let repeat = server
        .invoke_tool(head_call(
            json!({"action": "reconstruct", "params": params}),
        ))
        .await
        .unwrap();
    assert_eq!(repeat.is_error, Some(true), "existing maps need overwrite");
    let asked = seen.lock().unwrap().len();
    task.abort();

    // A photograph the worker fitted with different bytes, or a worker that
    // does not describe its views, leaves the displacement map written and
    // says so.
    for (changes, message) in [
        (
            json!({"image_sha256": [digest(b"another photograph")]}),
            "changed while",
        ),
        (json!({"views": null}), "did not describe"),
    ] {
        let (endpoint, _, task) = fake_recon_worker_with(true, changes).await;
        let server =
            recon_server_with(root.path(), &endpoint, flame2020_library_with_uv(true)).await;
        let params = json!({
            "images": ["face.png"], "displacement_path": "d2.png", "texture_path": "c2.png"
        });
        let result = server
            .invoke_tool(head_call(
                json!({"action": "reconstruct", "params": params}),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{message}");
        let text = format!("{result:?}");
        assert!(
            text.contains(message) && text.contains("wrote the displacement map"),
            "{text}"
        );
        assert!(!root.path().join("c2.png").exists());
        task.abort();
    }

    // A model without a texture layout, one path for both maps, or a
    // photograph over the size limit is refused before the worker is asked.
    std::fs::File::create(root.path().join("huge.png"))
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    let (endpoint, seen, task) = fake_recon_worker(true).await;
    for (library, params, message) in [
        (
            flame2020_library(),
            json!({"images": ["face.png"], "displacement_path": "d.png", "texture_path": "c.png"}),
            "--texture",
        ),
        (
            flame2020_library_with_uv(true),
            json!({"images": ["face.png"], "displacement_path": "d.png", "texture_path": "d.png"}),
            "must differ",
        ),
        (
            flame2020_library_with_uv(true),
            json!({"images": ["face.png"], "displacement_path": "d.png", "texture_path": "./d.png"}),
            "name the same file",
        ),
        (
            flame2020_library_with_uv(true),
            json!({"images": ["huge.png"], "displacement_path": "d.png", "texture_path": "c.png"}),
            "snapshot limit",
        ),
    ] {
        let server = recon_server_with(root.path(), &endpoint, library).await;
        let result = server
            .invoke_tool(head_call(
                json!({"action": "reconstruct", "params": params}),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{params}");
        assert!(
            format!("{result:?}").contains(message),
            "{params}: {result:?}"
        );
    }
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(asked, 1);
    task.abort();
}

#[tokio::test]
async fn head_reconstruct_sizes_the_colour_map_and_paints_the_eyes() {
    let root = tempfile::tempdir().unwrap();
    image::RgbImage::from_pixel(224, 224, image::Rgb([200, 40, 10]))
        .save(root.path().join("face.png"))
        .unwrap();
    let digest: String = {
        use sha2::Digest;
        sha2::Sha256::digest(std::fs::read(root.path().join("face.png")).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    // Every vertex of the fixture's head is an eyeball.
    let eyed = flame2020_library_with_regions(
        true,
        vec![
            ("left_eyeball".into(), vec![0, 1, 2, 3]),
            ("right_eyeball".into(), vec![0]),
        ],
    );
    let sclera = [1u8, 2, 3];
    for (library, painted) in [(eyed, true), (flame2020_library_with_uv(true), false)] {
        let (endpoint, seen, task) =
            fake_recon_worker_with(true, json!({"image_sha256": [digest]})).await;
        let server = recon_server_with(root.path(), &endpoint, library).await;
        let params = json!({
            "images": ["face.png"], "displacement_path": "detail.png",
            "texture_path": "colour.png", "overwrite": true, "texture_size": 512,
            "eyes": "painted", "iris_colour": [10, 200, 30], "sclera_colour": sclera,
        });
        let result = server
            .invoke_tool(head_call(
                json!({"action": "reconstruct", "params": params}),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let value = result.structured_content.unwrap();
        let texture = &value["texture"];
        assert_eq!(texture["width"], 512, "{value}");
        let map = image::open(root.path().join("colour.png"))
            .unwrap()
            .into_rgb8();
        assert_eq!(map.dimensions(), (512, 512));
        let shows_sclera = map.pixels().any(|pixel| pixel.0 == sclera);
        let eyes = &texture["eyes"];
        assert_eq!(eyes["requested"], "painted", "{value}");
        if painted {
            assert_eq!(eyes["applied"], "painted", "{value}");
            assert_eq!(eyes["iris_colour"], json!([10, 200, 30]), "{value}");
            assert_eq!(eyes["iris_source"], "given", "{value}");
            assert!(shows_sclera);
        } else {
            // Without separate eyeballs nothing is painted, and the response
            // says so.
            assert_eq!(eyes["applied"], "photo", "{value}");
            assert!(
                eyes["note"].as_str().unwrap().contains("eyeballs"),
                "{value}"
            );
            assert!(!shows_sclera);
        }
        // The worker is not sent the options the server applies.
        let sent = seen.lock().unwrap();
        for name in ["texture_size", "eyes", "iris_colour", "sclera_colour"] {
            assert!(sent[0].get(name).is_none(), "{name}: {sent:?}");
        }
        drop(sent);
        task.abort();
    }
}

#[tokio::test]
async fn head_reconstruct_paints_eyes_on_the_head_it_returns() {
    let root = tempfile::tempdir().unwrap();
    image::RgbImage::from_pixel(224, 224, image::Rgb([200, 40, 10]))
        .save(root.path().join("face.png"))
        .unwrap();
    let digest: String = {
        use sha2::Digest;
        sha2::Sha256::digest(std::fs::read(root.path().join("face.png")).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    // Identity component 0 scales the head about the origin; every vertex
    // is an eyeball. The worker's views keep the metre-wide head, but the
    // identity it returns shrinks it to a centimetre, about the size of a
    // life-size iris.
    let library = flame2020_library_edited(|fixture| {
        fixture.uv = Some((
            vec![0.1, 0.1, 0.9, 0.1, 0.5, 0.9, 0.5, 0.5],
            vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
        ));
        fixture.regions = vec![
            ("left_eyeball".into(), vec![0, 1, 2, 3]),
            ("right_eyeball".into(), vec![0]),
        ];
        for (index, value) in fixture.template.clone().into_iter().enumerate() {
            fixture.shapedirs[index * 400] = value;
        }
    });
    let iris = [10u8, 200, 30];
    let mut painted = Vec::new();
    for identity in [json!([0.0]), json!([-0.99])] {
        let (endpoint, _, task) = fake_recon_worker_with(
            true,
            json!({"image_sha256": [digest], "identity": identity}),
        )
        .await;
        let server = recon_server_with(root.path(), &endpoint, Arc::clone(&library)).await;
        let params = json!({
            "images": ["face.png"], "displacement_path": "detail.png",
            "texture_path": "colour.png", "overwrite": true, "texture_size": 512,
            "eyes": "painted", "iris_colour": iris,
        });
        let result = server
            .invoke_tool(head_call(
                json!({"action": "reconstruct", "params": params}),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let map = image::open(root.path().join("colour.png"))
            .unwrap()
            .into_rgb8();
        painted.push(map.pixels().filter(|pixel| pixel.0 == iris).count());
        task.abort();
    }
    // On the metre-wide head the iris is a speck; on the returned head it
    // covers much of the front.
    assert!(painted[1] > 20 * painted[0].max(1), "{painted:?}");
}

#[tokio::test]
async fn head_reconstruct_reports_and_pictures_the_fit() {
    let root = tempfile::tempdir().unwrap();
    image::RgbImage::from_pixel(224, 224, image::Rgb([200, 40, 10]))
        .save(root.path().join("face.png"))
        .unwrap();
    let digest: String = {
        use sha2::Digest;
        sha2::Sha256::digest(std::fs::read(root.path().join("face.png")).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    // Labels saying every pixel shows face skin, over the whole photograph.
    let labels = {
        let mut png = std::io::Cursor::new(Vec::new());
        image::GrayImage::from_pixel(16, 16, image::Luma([3]))
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(png.into_inner())
    };
    let views = json!([{
        "shape": [0.0], "expression": [0.0],
        "jaw": [0.0, 0.0, 0.0], "head_rotation": [0.0, 0.0, 0.0],
        "camera": [1.0, -0.5, -0.5],
        "crop": {"left": 0.0, "top": 0.0, "step": 1.0},
        "image_size": [224, 224],
        "segmentation": {
            "labels_png": labels,
            "classes": ["background", "hair", "body_skin", "face_skin", "clothes", "other"],
            "crop": {"left": 0.0, "top": 0.0, "step": 16.0 / 224.0},
            "hair_rgb": null, "hair_pixels": 0
        }
    }]);
    // The whole fixture head is the face; no triangle lies wholly in the scalp.
    let library = flame2020_library_with_regions(
        true,
        vec![("face".into(), vec![0, 1, 2, 3]), ("scalp".into(), vec![0])],
    );
    let (endpoint, _, task) =
        fake_recon_worker_with(true, json!({"image_sha256": [digest], "views": views})).await;
    let server = recon_server_with(root.path(), &endpoint, Arc::clone(&library)).await;
    let params = json!({
        "images": ["face.png"], "displacement_path": "detail.png",
        "texture_path": "colour.png", "fit_path": "fit.png"
    });
    let result = server
        .invoke_tool(head_call(
            json!({"action": "reconstruct", "params": params}),
        ))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    // Every pixel is face skin, so all of the head lands on the person, and
    // the head covers part of the face skin.
    let fit = &value["fit"][0];
    assert_eq!(fit["image"], "face.png", "{value}");
    assert_eq!(fit["head_on_person"], 1.0, "{value}");
    let covered = fit["face_skin_covered"].as_f64().unwrap();
    assert!(covered > 0.05 && covered < 0.5, "{value}");
    // The picture holds the photograph and the grey head side by side.
    let picture = &value["fit_picture"];
    assert_eq!(picture["artifact"]["path"], "fit.png", "{value}");
    let written = image::open(root.path().join("fit.png"))
        .unwrap()
        .into_rgb8();
    assert_eq!(
        [written.width(), written.height()].map(u64::from),
        [
            picture["width"].as_u64().unwrap(),
            picture["height"].as_u64().unwrap()
        ]
    );
    assert!(written.width() > written.height(), "{picture}");
    // Coverage by region adds up with seen; a region with no whole
    // triangle has none to report.
    let texture = &value["texture"];
    assert_eq!(texture["coverage"]["face"], texture["seen"], "{value}");
    assert_eq!(texture["coverage"]["scalp"], Value::Null, "{value}");
    assert!(texture["coverage"].get("ears").is_none(), "{value}");
    task.abort();

    // Without labels the measures are null, and outputs that would overwrite
    // one another are refused before the worker is asked.
    let (endpoint, _seen, task) = fake_recon_worker(true).await;
    let server = recon_server_with(root.path(), &endpoint, library).await;
    let plain = server
        .invoke_tool(head_call(json!({"action": "reconstruct", "params": {
            "images": ["face.png"], "displacement_path": "d2.png"
        }})))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(plain["fit"][0]["head_on_person"], Value::Null, "{plain}");
    task.abort();

    // A worker that describes no views gets no picture.
    let digest: String = {
        use sha2::Digest;
        sha2::Sha256::digest(std::fs::read(root.path().join("face.png")).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    let (endpoint, seen, task) =
        fake_recon_worker_with(true, json!({"image_sha256": [digest], "views": []})).await;
    let server = recon_server_with(root.path(), &endpoint, flame2020_library()).await;
    let refused = server
        .invoke_tool(head_call(json!({"action": "reconstruct", "params": {
            "images": ["face.png"], "displacement_path": "d4.png", "fit_path": "blank.png"
        }})))
        .await
        .unwrap();
    assert_eq!(refused.is_error, Some(true));
    assert!(
        format!("{refused:?}").contains("different number of photographs"),
        "{refused:?}"
    );
    assert!(!root.path().join("blank.png").exists());
    for (fit_path, message) in [
        ("d3.png", "must differ"),
        ("./d3.png", "same file"),
        ("fit.jpg", ".png"),
    ] {
        let refused = server
            .invoke_tool(head_call(json!({"action": "reconstruct", "params": {
                "images": ["face.png"], "displacement_path": "d3.png", "fit_path": fit_path
            }})))
            .await
            .unwrap();
        assert_eq!(refused.is_error, Some(true), "{fit_path}");
        assert!(format!("{refused:?}").contains(message), "{refused:?}");
    }
    assert_eq!(seen.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test]
async fn head_reconstruct_judges_the_fit_of_the_head_it_returns() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("face.jpg"), b"jpeg").unwrap();
    // Labels saying every pixel shows face skin, over the whole photograph.
    let labels = {
        let mut png = std::io::Cursor::new(Vec::new());
        image::GrayImage::from_pixel(16, 16, image::Luma([3]))
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(png.into_inner())
    };
    let views = json!([{
        "shape": [0.0], "expression": [0.0],
        "jaw": [0.0, 0.0, 0.0], "head_rotation": [0.0, 0.0, 0.0],
        "camera": [1.0, -0.5, -0.5],
        "crop": {"left": 0.0, "top": 0.0, "step": 1.0},
        "image_size": [224, 224],
        "segmentation": {
            "labels_png": labels,
            "classes": ["background", "hair", "body_skin", "face_skin", "clothes", "other"],
            "crop": {"left": 0.0, "top": 0.0, "step": 16.0 / 224.0},
            "hair_rgb": null, "hair_pixels": 0
        }
    }]);
    // Identity component 0 scales the head about the origin.
    let library = flame2020_library_edited(|fixture| {
        for (index, value) in fixture.template.clone().into_iter().enumerate() {
            fixture.shapedirs[index * 400] = value;
        }
    });
    let mut covered = Vec::new();
    for identity in [json!([0.0]), json!([-0.5])] {
        let (endpoint, _, task) =
            fake_recon_worker_with(true, json!({"views": views, "identity": identity})).await;
        let server = recon_server_with(root.path(), &endpoint, Arc::clone(&library)).await;
        let value = server
            .invoke_tool(head_call(json!({"action": "reconstruct", "params": {
                "images": ["face.jpg"], "displacement_path": "detail.png", "overwrite": true
            }})))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        covered.push(value["fit"][0]["face_skin_covered"].as_f64().unwrap());
        task.abort();
    }
    // The views are the same, but the returned head is half the size, so it
    // covers much less of the skin; judged on the views' own shape, the two
    // would be equal.
    assert!(covered[1] < 0.6 * covered[0], "{covered:?}");
}

#[tokio::test]
async fn photo_to_bust_makes_the_bust_the_separate_calls_make() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("face.jpg"), b"jpeg").unwrap();
    // The fake worker reports this map without writing it.
    std::fs::write(
        root.path().join("detail.png"),
        displacement_png(64, 64, |x, y| ((x * 1000 + y * 37) % 65_535) as u16),
    )
    .unwrap();
    // A closed tetrahedron with a texture layout, as the FLAME 2020 model.
    let library = flame2020_library_edited(|fixture| {
        fixture.faces = vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3];
        fixture.uv = Some((
            vec![0.1, 0.1, 0.9, 0.1, 0.5, 0.9, 0.5, 0.5],
            vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
        ));
    });
    let (endpoint, seen, task) = fake_recon_worker(true).await;
    let server = recon_server_with(root.path(), &endpoint, library).await;
    let reconstruct = json!({
        "images": ["face.jpg"], "displacement_path": "detail.png", "overwrite": true,
        "neck_turn": 1.0
    });
    let print = json!({"height_mm": 50.0, "edge_length_mm": 2.0, "overwrite": true});
    let detail = json!({"relief_mm": 0.3});

    let mut combined_print = print.clone();
    combined_print["path"] = json!("combined.stl");
    let combined = server
        .invoke_tool(head_call(json!({"action": "photo_to_bust", "params": {
            "reconstruct": reconstruct, "print": combined_print, "detail": detail
        }})))
        .await
        .unwrap();
    assert_eq!(combined.is_error, Some(false), "{combined:?}");
    let combined = combined.structured_content.unwrap();
    assert!(
        head_output_validator(&server).is_valid(&combined),
        "{combined}"
    );

    // The same steps, one at a time.
    let reconstructed = server
        .invoke_tool(head_call(
            json!({"action": "reconstruct", "params": reconstruct}),
        ))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let mut separate_print = print.clone();
    for key in ["variant", "identity", "expression", "pose"] {
        separate_print[key] = reconstructed[key].clone();
    }
    separate_print["path"] = json!("separate.stl");
    separate_print["displacement"] = json!({
        "path": reconstructed["displacement"]["artifact"]["path"],
        "offset_mm": reconstructed["displacement"]["offset_mm"],
        "scale_mm": reconstructed["displacement"]["scale_mm"],
        "relief_mm": 0.3
    });
    let separate = server
        .invoke_tool(head_call(
            json!({"action": "prepare_print", "params": separate_print}),
        ))
        .await
        .unwrap();
    assert_eq!(separate.is_error, Some(false), "{separate:?}");
    assert_eq!(combined["reconstruct"], reconstructed);
    assert!(
        combined["reconstruct"]["pose"]["neck"].is_array(),
        "{combined}"
    );
    assert_eq!(
        std::fs::read(root.path().join("combined.stl")).unwrap(),
        std::fs::read(root.path().join("separate.stl")).unwrap(),
        "the same bust"
    );
    assert_eq!(seen.lock().unwrap().len(), 2);

    // What the reconstruction supplies, and options that are wrong, are
    // refused before the worker is asked.
    for (params, message) in [
        (
            json!({"print": {"path": "x.stl", "identity": [1.0]}}),
            "print.identity",
        ),
        (
            json!({"print": {"path": "x.stl", "pose": {"jaw": [0.1, 0.0, 0.0]}}}),
            "print.pose.jaw",
        ),
        (
            json!({"print": {"path": "x.stl", "pose": {"neck": [0.1, 0.0, 0.0]}}}),
            "print.pose.neck",
        ),
        (
            json!({"print": {"path": "x.stl", "displacement": {"path": "d.png", "offset_mm": 0.0, "scale_mm": 1.0}}}),
            "print.displacement",
        ),
        (
            json!({"print": {"path": "x.stl", "height_mm": 5.0}}),
            "height_mm",
        ),
        (
            json!({"print": {"path": "x.stl"}, "detail": {"gain": 99.0}}),
            "gain",
        ),
        (
            json!({"print": {"path": "x.stl"}, "blender": {"texture": "t.png"}}),
            "blender.texture",
        ),
        (
            json!({"print": {"path": "x.stl"}, "blender": {"appearance": {
                "bump": {"path": "detail.png", "scale_mm": 1.0}
            }}}),
            "needs a colour map",
        ),
        (
            json!({"reconstruct": {"texture_path": "colour.png"}, "print": {"path": "x.stl"},
            "blender": {"appearance": {
                "bump": {"path": "detail.jpg", "scale_mm": 1.0}
            }}}),
            "must be a .png map",
        ),
        // Fields are refused when given, whatever their value.
        (
            json!({"print": {"path": "x.stl", "pose": {"jaw": [0.0, 0.0, 0.0]}}}),
            "print.pose.jaw",
        ),
        (
            json!({"print": {"path": "x.stl", "identity": []}}),
            "print.identity",
        ),
        (
            json!({"print": {"path": "x.stl"}, "blender": {"pose": {"jaw": [0.0, 0.0, 0.0]}}}),
            "blender.pose.jaw",
        ),
        // Head options that need no reconstruction are checked before it.
        (
            json!({"print": {"path": "x.stl", "expression_scale": 2.0}}),
            "expression_scale",
        ),
        (
            json!({"print": {"path": "x.stl", "pose": {"left_eye": [4.0, 0.0, 0.0]}}}),
            "left_eye",
        ),
        (
            json!({"print": {"path": "x.stl"}, "blender": {"expression_scale": 2.0}}),
            "expression_scale",
        ),
        (
            json!({"print": {
                "path": "x.stl", "height_mm": 100.0, "body": "extended_neck",
                "neck_extension_mm": 60.0
            }}),
            "less than 50%",
        ),
    ] {
        let mut params = params;
        let mut request = reconstruct.clone();
        if let Some(extra) = params.get("reconstruct").and_then(Value::as_object) {
            for (key, value) in extra {
                request[key] = value.clone();
            }
        }
        params["reconstruct"] = request;
        let refused = server
            .invoke_tool(head_call(
                json!({"action": "photo_to_bust", "params": params}),
            ))
            .await
            .unwrap();
        assert_eq!(refused.is_error, Some(true), "{params}");
        assert!(
            format!("{refused:?}").contains(message),
            "{message}: {refused:?}"
        );
    }
    assert_eq!(seen.lock().unwrap().len(), 2);

    // What only the head can refuse comes after the reconstruction, from the
    // geometry worker with its own code, and carries the reconstruction's
    // result so that it can be used again.
    let refused = server
        .invoke_tool(head_call(json!({"action": "photo_to_bust", "params": {
            "reconstruct": reconstruct,
            "print": {"path": "thin.stl", "height_mm": 50.0, "edge_length_mm": 2.0,
                "hollow_wall_mm": 20.0},
            "detail": {"apply": false}
        }})))
        .await
        .unwrap();
    assert_eq!(refused.is_error, Some(true));
    let text = format!("{refused:?}");
    assert!(text.contains("geometry worker failed"), "{text}");
    assert!(
        text.contains("result follows") && text.contains("identity"),
        "{text}"
    );
    task.abort();
}

#[tokio::test]
async fn photo_to_bust_closes_the_eyelids_as_reconstructed() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("face.jpg"), b"jpeg").unwrap();
    std::fs::write(
        root.path().join("detail.png"),
        displacement_png(64, 64, |_, _| 0),
    )
    .unwrap();
    let library = flame2020_library_edited(|fixture| {
        fixture.faces = vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3];
        let mut left = vec![0.0; 12];
        left[2] = 0.01; // vertex 0, 10 mm along z
        fixture.eyelids = Some((left, vec![0.0; 12]));
    });
    let (endpoint, _, task) = fake_recon_worker_with(true, json!({"eyelids": [1.0, 0.0]})).await;
    let server = recon_server_with(root.path(), &endpoint, library).await;
    let reconstruct = json!({
        "images": ["face.jpg"], "displacement_path": "detail.png", "overwrite": true
    });
    let print = |path: &str| json!({"path": path, "height_mm": 50.0, "edge_length_mm": 4.0, "overwrite": true});
    let combined = server
        .invoke_tool(head_call(json!({"action": "photo_to_bust", "params": {
            "reconstruct": reconstruct, "print": print("combined.stl"), "detail": {"apply": false}
        }})))
        .await
        .unwrap();
    assert_eq!(combined.is_error, Some(false), "{combined:?}");
    let reconstructed = combined.structured_content.unwrap()["reconstruct"].clone();
    assert_eq!(
        reconstructed["eyelids"],
        json!([1.0, 0.0]),
        "{reconstructed}"
    );
    // The same bust from prepare_print with the reconstruction's eyelids, and
    // a different one without them.
    for (path, eyelids) in [("closed.stl", true), ("open.stl", false)] {
        let mut separate = print(path);
        for key in ["variant", "identity", "expression", "pose"] {
            separate[key] = reconstructed[key].clone();
        }
        if eyelids {
            separate["eyelids"] = reconstructed["eyelids"].clone();
        }
        let made = server
            .invoke_tool(head_call(
                json!({"action": "prepare_print", "params": separate}),
            ))
            .await
            .unwrap();
        assert_eq!(made.is_error, Some(false), "{made:?}");
    }
    let read = |path: &str| std::fs::read(root.path().join(path)).unwrap();
    assert_eq!(read("combined.stl"), read("closed.stl"));
    assert_ne!(read("combined.stl"), read("open.stl"));
    // The eyelids come from the reconstruction, in print and in blender.
    for request in ["print", "blender"] {
        let mut params = json!({"reconstruct": reconstruct, "print": print("x.stl")});
        params[request]["eyelids"] = json!([0.0, 0.0]);
        let refused = server
            .invoke_tool(head_call(
                json!({"action": "photo_to_bust", "params": params}),
            ))
            .await
            .unwrap();
        assert!(
            format!("{refused:?}")
                .contains(&format!("{request}.eyelids comes from the reconstruction")),
            "{refused:?}"
        );
    }
    task.abort();
}

#[tokio::test]
async fn photo_to_bust_can_also_create_the_head_in_blender() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("face.jpg"), b"jpeg").unwrap();
    let library = flame2020_library_edited(|fixture| {
        fixture.faces = vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3];
    });
    let (endpoint, _, task) = fake_recon_worker(true).await;
    let (fake, captured) = recording_addon().await;
    let mut cfg = settings(&fake.host(), fake.port());
    cfg.flame = Some(library);
    let recon = printable_server::recon::ReconClient::configure(&|name| {
        (name == "PRINTABLE_RECON_ENDPOINT").then(|| endpoint.clone())
    })
    .unwrap()
    .unwrap();
    recon.refresh().await;
    cfg.recon = Some(recon);
    let server = PrintableServer::new(
        workspace(Some(root.path())),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(cfg),
    );
    let result = server
        .invoke_tool(head_call(json!({"action": "photo_to_bust", "params": {
            "reconstruct": {"images": ["face.jpg"], "displacement_path": "detail.png"},
            "print": {"path": "bust.stl", "height_mm": 50.0, "edge_length_mm": 2.0},
            "detail": {"apply": false},
            "blender": {"name": "Photographed"}
        }})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    assert_eq!(value["blender"]["name"], "Photographed", "{value}");
    assert_eq!(value["print"]["artifact"]["path"], "bust.stl", "{value}");
    // Without detail the bust takes no displacement.
    assert!(
        value["print"]["bust"].get("displacement").is_none(),
        "{value}"
    );
    let calls = captured.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["name"], "Photographed");
    assert!(calls[0].1.get("texture").is_none());
    task.abort();
}

#[tokio::test]
async fn photo_to_bust_keeps_blenders_own_errors() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("face.jpg"), b"jpeg").unwrap();
    let library = flame2020_library_edited(|fixture| {
        fixture.faces = vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3];
    });
    let generation = "00000000-0000-4000-8000-000000000001";
    // Blender's scene has moved on since the caller looked.
    let fake = FakeAddon::spawn(move |_, _| {
        ResponseSpec::RawWithId(json!({
            "status": "error", "error": "scene changed", "error_code": "stale_scene_state",
            "scene_state": {"generation": generation, "revision": 7},
            "bridge_instance_id": generation,
            "addon_version": printable_blender::BRIDGE_PROTOCOL_VERSION,
        }))
    })
    .await;
    let (endpoint, _, task) = fake_recon_worker(true).await;
    let mut cfg = settings(&fake.host(), fake.port());
    cfg.flame = Some(library);
    let recon = printable_server::recon::ReconClient::configure(&|name| {
        (name == "PRINTABLE_RECON_ENDPOINT").then(|| endpoint.clone())
    })
    .unwrap()
    .unwrap();
    recon.refresh().await;
    cfg.recon = Some(recon);
    let server = PrintableServer::new(
        workspace(Some(root.path())),
        Arc::new(client(&fake.host(), fake.port())),
        Arc::new(cfg),
    );
    let result = server
        .invoke_tool(head_call(json!({"action": "photo_to_bust", "params": {
            "reconstruct": {"images": ["face.jpg"], "displacement_path": "detail.png"},
            "print": {"path": "bust.stl", "height_mm": 50.0, "edge_length_mm": 2.0},
            "detail": {"apply": false},
            "blender": {"expected_scene": {"generation": generation, "revision": 3}}
        }})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true), "{result:?}");
    // The same typed error create gives, with the scene state to recover from.
    let structured = result.structured_content.expect("structured error");
    assert_eq!(
        structured["error"]["code"], "stale_scene_state",
        "{structured}"
    );
    assert_eq!(
        structured["error"]["scene_state"]["revision"], 7,
        "{structured}"
    );
    // The bust was written before Blender was asked.
    assert!(root.path().join("bust.stl").is_file());
    task.abort();
}

#[tokio::test]
async fn head_reconstruct_makes_one_colour_map_at_a_time() {
    let root = tempfile::tempdir().unwrap();
    image::RgbImage::from_pixel(224, 224, image::Rgb([90, 90, 90]))
        .save(root.path().join("face.png"))
        .unwrap();
    let (endpoint, seen, task) =
        fake_recon_worker_with(true, json!({"_delay_ms": 1500, "views": null})).await;
    let server =
        Arc::new(recon_server_with(root.path(), &endpoint, flame2020_library_with_uv(true)).await);
    let request = |texture: &str| {
        head_call(json!({"action": "reconstruct", "params": {
            "images": ["face.png"], "displacement_path": format!("{texture}.d.png"),
            "texture_path": format!("{texture}.png")
        }}))
    };
    let first = {
        let server = Arc::clone(&server);
        let call = request("first");
        tokio::spawn(async move { server.invoke_tool(call).await.unwrap() })
    };
    while seen.lock().unwrap().is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let second = server.invoke_tool(request("second")).await.unwrap();
    assert_eq!(second.is_error, Some(true));
    assert!(
        format!("{second:?}").contains("another colour map is being made"),
        "{second:?}"
    );
    // The first finishes (here with a worker that describes no views), and
    // the next request is admitted.
    first.await.unwrap();
    let third = server.invoke_tool(request("third")).await.unwrap();
    assert!(
        !format!("{third:?}").contains("another colour map is being made"),
        "{third:?}"
    );
    assert_eq!(seen.lock().unwrap().len(), 2);
    task.abort();
}

/// Applies a rippled displacement map to a bust of the model named by
/// `PRINTABLE_TEST_FLAME_MODEL`, which must be converted with `--texture`.
#[tokio::test]
#[ignore = "requires a converted FLAME model with its texture layout"]
async fn head_prepare_print_applies_a_displacement_map_to_a_real_bust() {
    let model = std::env::var_os("PRINTABLE_TEST_FLAME_MODEL")
        .expect("set PRINTABLE_TEST_FLAME_MODEL to a converted .safetensors model");
    let models = tempfile::tempdir().unwrap();
    std::fs::copy(&model, models.path().join("model.safetensors")).unwrap();
    let library = Arc::new(printable_server::flame::FlameLibrary::load(models.path()));
    let root = tempfile::tempdir().unwrap();
    // Ripples of ±0.5 mm at life size, twelve across the texture.
    let side = 256;
    std::fs::write(
        root.path().join("ripples.png"),
        displacement_png(side, side, |x, y| {
            let wave =
                |at: u32| (std::f64::consts::TAU * 6.0 * f64::from(at) / f64::from(side)).sin();
            (32767.5 * (1.0 + wave(x) * wave(y))).round() as u16
        }),
    )
    .unwrap();
    let server = head_print_server(root.path(), library);
    let started = std::time::Instant::now();
    let result = server
        .invoke_tool(head_call(json!({
            "action": "prepare_print",
            "params": {
                "path": "bust.stl",
                "identity": [1.0, -0.5, 0.5],
                "expression": [1.5, 0.5],
                "displacement": {"path": "ripples.png", "offset_mm": -0.5, "scale_mm": 1.0}
            }
        })))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let value = result.structured_content.unwrap();
    eprintln!(
        "bust in {:?}: {}\nvalidation topology: {}",
        started.elapsed(),
        value["bust"],
        value["validation"]["topology"]
    );
    assert!(head_output_validator(&server).is_valid(&value), "{value}");
    assert_eq!(value["validation"]["solid_geometry"], true, "{value}");
    assert_eq!(
        value["validation"]["topology"]["connected_components"], 1,
        "{value}"
    );
    let applied = &value["bust"]["displacement"];
    assert!(applied["lowest_mm"].as_f64().unwrap() < -0.1, "{value}");
    assert!(applied["highest_mm"].as_f64().unwrap() > 0.1, "{value}");
    assert!(value["bust"]["triangles"].as_u64().unwrap() <= 750_000);
}
