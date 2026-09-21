//! Behavioural coverage for the tool catalog, driving `tools::dispatch` directly
//! (no MCP wire framing): workspace list/read/write round-trip, the single-shot
//! write cap, the chunked-upload round-trip and its bounds (unknown upload,
//! oversized chunk, total-transfer cap, concurrent-upload cap), and
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
        blender_workspace_root: None,
        openscad_bin: None,
        scad_concurrency: 2,
        ffmpeg_bin: PathBuf::from("ffmpeg"),
        render_job_queue_depth: 16,
        geometry_worker_bin: Some(test_geometry_worker()),
        geometry_worker_memory_bytes: 1024 * 1024 * 1024,
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
    assert_eq!(scene_schema["properties"]["limit"]["maximum"], json!(1000));
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
        json!(1000)
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
    assert_eq!(render_schema["properties"]["width"]["maximum"], json!(8192));
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
    assert_eq!(gallery_schema["properties"]["views"]["maxItems"], json!(7));
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
        json!(1000)
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
        json!(1_048_576)
    );
    assert_eq!(
        scad_compile_schema["properties"]["timeout_seconds"]["default"],
        json!(3600.0)
    );
    assert_eq!(
        scad_compile_schema["properties"]["variant"]["maxLength"],
        json!(64)
    );
    assert_eq!(
        scad_compile_schema["$defs"]["ScadDefinitions"]["maxProperties"],
        json!(64)
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
        json!(8192)
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
        json!(36)
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
        json!(4096)
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
            "printable_scad_render",
            json!({
                "source": "cube(1);",
                "path": "oversized.png",
                "size": 8193,
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
            "printable_scad_cross_section",
            json!({
                "source": "cube(1);",
                "path": "vector.svg",
                "defines": {"samples": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]},
                "timeout_seconds": 5.0,
            }),
        ),
        (
            "printable_scad_compile",
            json!({
                "source": "cube(1);",
                "path": "variant.stl",
                "variant": "v".repeat(65),
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
    let paths: Vec<&str> = listed
        .as_array()
        .expect("list is an array")
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
    assert_eq!(listed[0]["path"], json!("preview.mp4"));
    assert_eq!(listed[0]["media_type"], json!("video/mp4"));
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
async fn chunk_enforces_the_total_transfer_cap() {
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

    // Fill exactly to the transfer cap in chunk-sized pieces (reusing one
    // encoded chunk), so the total sits at MAX_TRANSFER_BYTES.
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
        .expect("chunk within cap ok");
    }

    // One more byte pushes the cumulative size past the cap.
    let err = dispatch(
        &ws,
        &up,
        &blender,
        &cfg,
        "printable_workspace_write_chunk",
        json!({"upload_id": id, "data_base64": b64(b"!")}),
    )
    .await
    .expect_err("chunk over the total cap rejected");
    assert!(
        matches!(err, ToolError::Workspace(WsError::WriteTooLarge)),
        "got {err:?}"
    );
    assert_eq!(err.code(), "write_too_large");
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

    for (tool, arguments, command, expected_params) in cases {
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
                "controller_name": "x".repeat(256),
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
        assert_eq!(requests[0]["max_output_bytes"], json!(64 * 1024 * 1024));
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
    let mut invalid_exposure = base();
    invalid_exposure["presentation"]["exposure_stops"] = json!(10.1);
    let mut invalid_intensity = base();
    invalid_intensity["presentation"]["light_intensity_scale"] = json!(-0.1);
    invalid_azimuth["presentation"]["view"] = json!({"azimuth_degrees": 361.0});
    let mut invalid_elevation = base();
    invalid_elevation["presentation"]["view"] = json!({"elevation_degrees": 90.0});
    let mut occluded_studio_view = base();
    occluded_studio_view["presentation"]["view"] = json!({"elevation_degrees": -1.0});
    let mut oversized_pixels = base();
    oversized_pixels["width"] = json!(8192);
    oversized_pixels["height"] = json!(8192);
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
        (invalid_exposure, "exposure_stops"),
        (invalid_intensity, "light_intensity_scale"),
        (invalid_elevation, "elevation_degrees"),
        (occluded_studio_view, "ground cannot occlude"),
        (oversized_pixels, "pixel output limit"),
        (unknown_material_object, "not selected"),
        (duplicate_assignment, "assigned more than once"),
        (invalid_color, "base_color_srgb"),
        (invalid_metallic, "metallic"),
        (invalid_roughness, "roughness"),
        (invalid_samples, "samples"),
        (too_many_materials, "at most 64"),
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

    let boundary_objects = (0..64)
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
                "view": {"elevation_degrees": 0.0}
            }
        }),
        json!({
            "path": "renders/pixel-boundary.png",
            "objects": ["Body"],
            "presentation": {"profile": "engineering"},
            "width": 8192,
            "height": 2048
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
            json!({"name": "Cube", "limit": 101}),
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
            json!({"name": "Material", "offset": 1000001}),
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
            json!({"offset": 1_000_001}),
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
            json!({"primitive": "cylinder", "vertices": 1025}),
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
            json!({"path": "preview.png", "engine": "CYCLES", "samples": 4097}),
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
            json!({"path": "gallery.png", "width": 8192, "height": 8192}),
            "validation",
        ),
        (
            "printable_render_turntable",
            json!({"path": "turntable.png", "frames": 2}),
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
