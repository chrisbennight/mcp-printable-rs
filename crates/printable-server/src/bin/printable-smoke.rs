//! Container smoke test: drive the running MCP server over its HTTP surface and
//! assert the end-to-end contract. Built in CI and run from outside the
//! application container (never shipped in the runtime image, which stays minimal).
//!
//! Sequence: `/healthz` → sanitized `/readyz` → `initialize` →
//! `notifications/initialized` →
//! `tools/list` (exact release catalog) → `printable_status` (OpenSCAD and FFmpeg available)
//! → a real cube STL write/read round-trip → `printable_validate_mesh` →
//! `printable_analyze_assembly` with linear and rotational clearance motion → real
//! OpenSCAD compile, PNG render, and SVG cross-section workflows across the
//! reference product corpus. When the paired smoke supplies a durable Blender
//! source, the sequence continues through decoded product presentations and a
//! complete-arc-certified mechanical video.
//! Exits 0 on success and 1 on failure, with a diagnostic on stderr.
//!
//! The `/mcp` transport frames replies as either bare JSON or SSE (`data:` lines)
//! and assigns a session id on `initialize` (returned in the `mcp-session-id`
//! header, echoed on every later request), so this mirrors the wire handshake
//! the `mcp_endpoint_test` integration test exercises in-process.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

static OUTPUT_SCHEMAS: OnceLock<BTreeMap<String, jsonschema::Validator>> = OnceLock::new();
use std::path::{Component, Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use base64::Engine as _;
use reqwest::header::{HOST, HeaderMap, HeaderValue};
use serde_json::{Value, json};

#[path = "smoke/workflow.rs"]
mod workflow;

const SMOKE_ROTATIONAL_CLEARANCE_MM: f64 = 0.5;
const MIN_VISIBLE_CHANNEL_RANGE: u8 = 24;
const MIN_DISTINCT_IMAGE_DIFFERENCE: f64 = 1.0;
const BRACKET_SOURCE: &str = include_str!("../../../../acceptance/products/bracket.scad");
const ENCLOSURE_SOURCE: &str = include_str!("../../../../acceptance/products/enclosure.scad");
const GRIP_SOURCE: &str = include_str!("../../../../acceptance/products/grip.scad");
const HINGE_SOURCE: &str = include_str!("../../../../acceptance/products/hinge.scad");

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let base = args
        .next()
        .or_else(|| std::env::var("PRINTABLE_SMOKE_URL").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8000".to_string());
    let catalog_path = args
        .next()
        .or_else(|| std::env::var("PRINTABLE_SMOKE_TOOLS_FILE").ok())
        .unwrap_or_else(|| "smoke/expected-tools.txt".to_string());
    if args.next().is_some() {
        eprintln!("usage: printable-smoke [base-url] [expected-tools-file]");
        return ExitCode::FAILURE;
    }
    let expected_tools = match load_expected_tools(&catalog_path) {
        Ok(expected) => expected,
        Err(err) => {
            eprintln!("smoke: FAIL ({base}): {err}");
            return ExitCode::FAILURE;
        }
    };
    let host_header = std::env::var("PRINTABLE_SMOKE_HOST").ok();
    let mcp_bearer = match std::env::var("PRINTABLE_SMOKE_BEARER") {
        Ok(value) if !value.is_empty() => value,
        _ => {
            eprintln!("smoke: FAIL ({base}): PRINTABLE_SMOKE_BEARER is required");
            return ExitCode::FAILURE;
        }
    };
    let durable_source = std::env::var("PRINTABLE_SMOKE_DURABLE_SOURCE").ok();
    let workspace_root = std::env::var_os("PRINTABLE_SMOKE_WORKSPACE_ROOT").map(PathBuf::from);
    let require_ready = std::env::var("PRINTABLE_SMOKE_REQUIRE_READY").as_deref() == Ok("1");
    match run(
        &base,
        &expected_tools,
        host_header.as_deref(),
        &mcp_bearer,
        durable_source.as_deref(),
        workspace_root.as_deref(),
        require_ready,
    )
    .await
    {
        Ok(()) => {
            eprintln!("smoke: OK ({base})");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("smoke: FAIL ({base}): {err}");
            ExitCode::FAILURE
        }
    }
}

fn load_expected_tools(path: &str) -> Result<BTreeSet<String>, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("read expected tool catalog {path}: {e}"))?;
    parse_expected_tools(&contents)
}

fn parse_expected_tools(contents: &str) -> Result<BTreeSet<String>, String> {
    let tools: Vec<&str> = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    let unique: BTreeSet<String> = tools.iter().map(|name| (*name).to_string()).collect();
    if unique.is_empty() {
        return Err("expected tool catalog is empty".to_string());
    }
    if unique.len() != tools.len() {
        return Err("expected tool catalog contains a duplicate name".to_string());
    }
    Ok(unique)
}

fn validate_readiness(http_status: u16, body: &Value, require_ready: bool) -> Result<(), String> {
    let object = body
        .as_object()
        .ok_or_else(|| format!("/readyz response is not an object: {body}"))?;
    let keys = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = BTreeSet::from(["status", "checks", "work", "codes"]);
    if keys != expected {
        return Err(format!("/readyz exposed an unexpected shape: {body}"));
    }

    let checks = body["checks"]
        .as_object()
        .ok_or_else(|| format!("/readyz checks are not an object: {body}"))?;
    let check_keys = checks.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected_checks = BTreeSet::from([
        "blender",
        "render_worker",
        "workspace",
        "openscad",
        "ffmpeg",
        "durable_recovery",
    ]);
    if check_keys != expected_checks || checks.values().any(|value| !value.is_boolean()) {
        return Err(format!("/readyz checks are not sanitized booleans: {body}"));
    }

    let work = body["work"]
        .as_object()
        .ok_or_else(|| format!("/readyz work is not an object: {body}"))?;
    let work_keys = work.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if work_keys != BTreeSet::from(["queued", "running"])
        || work.values().any(|value| !value.is_boolean())
    {
        return Err(format!("/readyz work is not sanitized booleans: {body}"));
    }

    let codes = body["codes"]
        .as_array()
        .ok_or_else(|| format!("/readyz codes are not an array: {body}"))?;
    let allowed_codes = BTreeSet::from([
        "blender_unavailable",
        "render_worker_unavailable",
        "workspace_unavailable",
        "openscad_unavailable",
        "ffmpeg_unavailable",
        "durable_recovery_fenced",
        "work_in_progress",
    ]);
    if codes.iter().any(|value| {
        value
            .as_str()
            .is_none_or(|code| !allowed_codes.contains(code))
    }) {
        return Err(format!("/readyz contains an unknown status code: {body}"));
    }

    let mut expected_blockers = Vec::new();
    for (check, code) in [
        ("blender", "blender_unavailable"),
        ("render_worker", "render_worker_unavailable"),
        ("workspace", "workspace_unavailable"),
        ("openscad", "openscad_unavailable"),
        ("ffmpeg", "ffmpeg_unavailable"),
        ("durable_recovery", "durable_recovery_fenced"),
    ] {
        if checks[check] == Value::Bool(false) {
            expected_blockers.push(Value::String(code.to_string()));
        }
    }
    let work_in_progress =
        work["queued"] == Value::Bool(true) || work["running"] == Value::Bool(true);

    match body["status"].as_str() {
        Some("ready")
            if http_status == 200
                && expected_blockers.is_empty()
                && !work_in_progress
                && codes.is_empty() => {}
        Some("busy")
            if http_status == 200
                && expected_blockers.is_empty()
                && work_in_progress
                && codes.as_slice() == [Value::String("work_in_progress".to_string())] => {}
        Some("blocked")
            if http_status == 503 && codes.as_slice() == expected_blockers.as_slice() =>
        {
            if require_ready {
                return Err(format!("paired service is not ready: {body}"));
            }
        }
        _ => {
            return Err(format!(
                "/readyz HTTP status and body disagree ({http_status}): {body}"
            ));
        }
    }
    Ok(())
}

async fn run(
    base: &str,
    expected_tools: &BTreeSet<String>,
    host_header: Option<&str>,
    mcp_bearer: &str,
    durable_source: Option<&str>,
    workspace_root: Option<&Path>,
    require_ready: bool,
) -> Result<(), String> {
    let mut headers = HeaderMap::new();
    if let Some(host) = host_header {
        let value = HeaderValue::from_str(host)
            .map_err(|e| format!("PRINTABLE_SMOKE_HOST is not a valid header: {e}"))?;
        headers.insert(HOST, value);
    }
    let probe_http = reqwest::Client::builder()
        .default_headers(headers.clone())
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {mcp_bearer}"))
        .map_err(|_| "PRINTABLE_SMOKE_BEARER is not a valid header value".to_string())?;
    authorization.set_sensitive(true);
    headers.insert(reqwest::header::AUTHORIZATION, authorization);
    let http = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| format!("build authenticated HTTP client: {e}"))?;
    let mcp = format!("{base}/mcp");

    // 1. Liveness.
    let health = probe_http
        .get(format!("{base}/healthz"))
        .send()
        .await
        .map_err(|e| format!("healthz request: {e}"))?
        .text()
        .await
        .map_err(|e| format!("healthz body: {e}"))?;
    if !health.contains("\"status\":\"ok\"") || !health.contains("printable_blender") {
        return Err(format!("unexpected /healthz body: {health}"));
    }

    // 2. Readiness is a separate dependency contract. Standalone image smoke
    // accepts blocked because Blender is intentionally absent; paired smoke
    // requires every product backend and the shared workspace to be ready.
    let readiness_response = probe_http
        .get(format!("{base}/readyz"))
        .send()
        .await
        .map_err(|e| format!("readyz request: {e}"))?;
    let readiness_status = readiness_response.status();
    let readiness: Value = readiness_response
        .json()
        .await
        .map_err(|e| format!("readyz body: {e}"))?;
    validate_readiness(readiness_status.as_u16(), &readiness, require_ready)?;

    // 3. initialize → capture the session id.
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "printable-smoke", "version": "0" }
        }
    });
    let resp = post(&http, &mcp, None, &init).await?;
    let session = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or("initialize did not assign an mcp-session-id")?;
    let initialized = extract_result(resp).await?;
    tracing::info!(
        instruction_bytes = initialized["instructions"]
            .as_str()
            .unwrap_or_default()
            .len(),
        "initial guidance size; bytes are not model tokens"
    );

    // 4. notifications/initialized (no response body expected).
    let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    expect_empty_success(
        post(&http, &mcp, Some(&session), &note).await?,
        "notifications/initialized",
    )
    .await?;

    // 5. tools/list — exactly the release catalog supplied by the caller.
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    let result = extract_result(post(&http, &mcp, Some(&session), &list).await?).await?;
    check_tool_catalog(&result, expected_tools)?;
    let mut schemas = BTreeMap::new();
    for tool in result["tools"].as_array().ok_or("missing catalog")? {
        let name = tool["name"].as_str().ok_or("missing tool name")?;
        let validator = jsonschema::validator_for(&tool["outputSchema"])
            .map_err(|error| format!("invalid output schema for {name}: {error}"))?;
        schemas.insert(name.to_owned(), validator);
    }
    OUTPUT_SCHEMAS
        .set(schemas)
        .map_err(|_| "catalog already initialized")?;
    tracing::info!(
        catalog_json_bytes = result.to_string().len(),
        tool_count = expected_tools.len(),
        "public interface size; bytes are not model tokens"
    );

    // 6. printable_status — OpenSCAD must be found in the image.
    let status = call_tool(&http, &mcp, &session, 3, "printable_status", json!({})).await?;
    if status["openscad"]["available"] != json!(true) {
        return Err(format!("openscad not available in image: {status}"));
    }
    if status["render_jobs"]["encoder"]["available"] != json!(true) {
        return Err(format!("ffmpeg not available in image: {status}"));
    }
    if status["render_jobs"]["recovery_fenced"] != json!(false) {
        return Err(format!("blender recovery fence is active: {status}"));
    }
    if status["render_jobs"]["recovery_integrity"]["status"] != json!("ok") {
        return Err(format!(
            "durable recovery metadata is not healthy: {status}"
        ));
    }

    if std::env::var("PRINTABLE_SMOKE_REQUIRE_NATIVE_VIEW").as_deref() == Ok("1") {
        if status["blender"]["native_observation"]["available"] != true {
            return Err("live Blender has no native observation capability".into());
        }
        for (index, method) in ["viewport", "editor"].into_iter().enumerate() {
            let observed = call_tool(&http, &mcp, &session, 9000 + index as u64 * 2,
                "printable_native_view", json!({
                    "method": method, "max_size": 512, "include_inline": false,
                    "context": {"area_type": "VIEW_3D", "window": 0, "area_index": 0, "region_type": "WINDOW"},
                    "view": {"axis": "FRONT", "perspective": "ORTHO", "shading": "SOLID", "overlays": true},
                    "expected_scene": status["blender"]["scene_state"]
                })).await?;
            if observed["method"] != method
                || observed["scene_state"] != status["blender"]["scene_state"]
            {
                return Err(
                    "native feedback method or model state differs from the request".into(),
                );
            }
            read_response_png(&http, &mcp, &session, 9001 + index as u64 * 2, &observed).await?;
        }
    }

    // 7. workspace write → read base64 round-trip with a real printable STL.
    let payload = smoke_cube_stl();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&payload);
    call_tool(
        &http,
        &mcp,
        &session,
        4,
        "printable_workspace_write",
        json!({"path": "smoke.stl", "data_base64": encoded}),
    )
    .await?;
    let read = call_tool(
        &http,
        &mcp,
        &session,
        5,
        "printable_workspace_read",
        json!({"path": "smoke.stl"}),
    )
    .await?;
    let got = read["data_base64"]
        .as_str()
        .ok_or("read result has no data_base64")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(got)
        .map_err(|e| format!("read data_base64 not valid base64: {e}"))?;
    if decoded != payload {
        return Err("workspace read did not round-trip the written bytes".to_string());
    }

    // 8. Exercise the native topology + Manifold path in the production image.
    let validation = call_tool(
        &http,
        &mcp,
        &session,
        6,
        "printable_validate_mesh",
        json!({"path": "smoke.stl", "density_g_cm3": 1.24}),
    )
    .await?;
    if validation["report"]["printable"] != json!(true)
        || validation["report"]["topology"]["watertight"] != json!(true)
        || validation["report"]["solid_properties"]["volume_mm3"] != json!(1000.0)
        || validation["report"]["solid_properties"]["mass_g"] != json!(1.24)
        || validation["report"]["overhang"]["requires_support"] != json!(false)
    {
        return Err(format!(
            "cube geometry validation was unexpected: {validation}"
        ));
    }

    // 9. Exercise pairwise Manifold intersection and Parry continuous shape casting.
    let moving_payload = smoke_cube_stl_at([15.0, 0.0, 0.0]);
    let moving_encoded = base64::engine::general_purpose::STANDARD.encode(&moving_payload);
    call_tool(
        &http,
        &mcp,
        &session,
        7,
        "printable_workspace_write",
        json!({"path": "smoke-moving.stl", "data_base64": moving_encoded}),
    )
    .await?;
    let assembly = call_tool(
        &http,
        &mcp,
        &session,
        8,
        "printable_analyze_assembly",
        json!({
            "fixed_path": "smoke.stl",
            "moving_path": "smoke-moving.stl",
            "required_clearance_mm": 4.0,
            "motion": {
                "direction": [-1.0, 0.0, 0.0],
                "travel_mm": 10.0,
                "target_clearance_mm": 1.0
            },
            "rotation": {
                "pivot_mm": [0.0, 0.0, 0.0],
                "axis": [0.0, 0.0, 1.0],
                "angle_degrees": 90.0,
                "target_clearance_mm": SMOKE_ROTATIONAL_CLEARANCE_MM
            }
        }),
    )
    .await?;
    if assembly["report"]["static_analysis"]["relation"] != json!("separated")
        || assembly["report"]["static_analysis"]["clearance_mm"] != json!(5.0)
        || assembly["report"]["static_analysis"]["interference_volume_mm3"] != json!(0.0)
        || assembly["report"]["motion"]["first_blocked_at_mm"] != json!(4.0)
        || assembly["report"]["motion"]["block_reason"] != json!("clearance_threshold")
        || assembly["report"]["motion"]["retained"] != json!(false)
        || assembly["report"]["rotation"]["can_rotate_full_angle"] != json!(true)
        || assembly["report"]["rotation"]["minimum_certified_clearance_mm"]
            .as_f64()
            .is_none_or(|clearance| clearance < SMOKE_ROTATIONAL_CLEARANCE_MM)
    {
        return Err(format!("assembly analysis was unexpected: {assembly}"));
    }

    // 9. Compile a support-free bracket through the generic product kit.
    let definitions = json!({
        "width_mm": 54.0,
        "include_ribs": true,
        "mounting_x_positions_mm": [-20.0, 20.0]
    });
    let design_profile = json!({
        "kit": "product_v1",
        "manufacturing": {
            "nozzle_diameter_mm": 0.4,
            "layer_height_mm": 0.2,
            "minimum_wall_mm": 2.0,
            "moving_clearance_mm": 0.35,
            "maximum_overhang_degrees": 50.0
        },
        "form": {
            "primary_radius_mm": 3.0,
            "secondary_radius_mm": 1.5,
            "edge_break_mm": 0.5,
            "transition_length_mm": 8.0
        }
    });
    let product_definition_names = json!([
        "include_ribs",
        "mounting_x_positions_mm",
        "pbl_edge_break_mm",
        "pbl_layer_height_mm",
        "pbl_maximum_overhang_degrees",
        "pbl_minimum_wall_mm",
        "pbl_moving_clearance_mm",
        "pbl_nozzle_diameter_mm",
        "pbl_primary_radius_mm",
        "pbl_secondary_radius_mm",
        "pbl_transition_length_mm",
        "pbl_variant",
        "width_mm"
    ]);
    let compiled = call_tool(
        &http,
        &mcp,
        &session,
        9,
        "printable_scad_compile",
        json!({
            "source": BRACKET_SOURCE,
            "path": "smoke-bracket.stl",
            "defines": definitions.clone(),
            "variant": "print",
            "design_profile": design_profile.clone(),
            "overwrite": true,
        }),
    )
    .await?;
    validate_compiled_product("bracket", &compiled, "smoke-bracket.stl", 50.0)?;
    if compiled["definitions"]["names"] != product_definition_names
        || compiled["design_profile"] != design_profile
    {
        return Err(format!(
            "OpenSCAD bracket parameters were unexpected: {compiled}"
        ));
    }

    // Keep the cutter honest at a strict policy in both horizontal axes. The
    // shallow roofs need a tall coupon, so the normal bracket above remains the
    // representative product workflow.
    let strict_bore_source = r#"
difference() {
    union() {
        pbl_panel(size=[70, 40, 4]);
        translate([-30, -6, 3.8]) cube([20, 12, 70.2]);
        translate([14, -10, 3.8]) cube([12, 20, 70.2]);
    }
    translate([-20, 0, 15])
        pbl_horizontal_bore_cutter(length=22, diameter=8, axis="x");
    translate([20, 0, 15])
        pbl_horizontal_bore_cutter(length=22, diameter=8, axis="y");
}
"#;
    let mut strict_design_profile = design_profile.clone();
    strict_design_profile["manufacturing"]["maximum_overhang_degrees"] = json!(5.0);
    let strict_bores = call_tool(
        &http,
        &mcp,
        &session,
        90,
        "printable_scad_compile",
        json!({
            "source": strict_bore_source,
            "path": "smoke-strict-horizontal-bores.stl",
            "design_profile": strict_design_profile,
            "overwrite": true,
        }),
    )
    .await?;
    if strict_bores["artifact"]["path"] != json!("smoke-strict-horizontal-bores.stl")
        || strict_bores["validation"]["printable"] != json!(true)
        || strict_bores["validation"]["overhang"]["threshold_degrees"] != json!(5.0)
        || strict_bores["validation"]["overhang"]["requires_support"] != json!(false)
    {
        return Err(format!(
            "strict horizontal-bore compile was unexpected: {strict_bores}"
        ));
    }

    // 10. Compile and render a rounded enclosure through the same kit and profile.
    let compiled_enclosure = call_tool(
        &http,
        &mcp,
        &session,
        91,
        "printable_scad_compile",
        json!({
            "source": ENCLOSURE_SOURCE,
            "path": "smoke-enclosure.stl",
            "variant": "print",
            "design_profile": design_profile.clone(),
            "overwrite": true,
        }),
    )
    .await?;
    validate_compiled_product(
        "enclosure",
        &compiled_enclosure,
        "smoke-enclosure.stl",
        50.0,
    )?;

    let mut thick_wall_profile = design_profile.clone();
    thick_wall_profile["manufacturing"]["minimum_wall_mm"] = json!(4.0);
    let thick_wall_shell = call_tool(
        &http,
        &mcp,
        &session,
        92,
        "printable_scad_compile",
        json!({
            "source": "pbl_shell();",
            "path": "smoke-thick-wall-shell.stl",
            "design_profile": thick_wall_profile,
            "overwrite": true,
        }),
    )
    .await?;
    if thick_wall_shell["artifact"]["path"] != json!("smoke-thick-wall-shell.stl")
        || thick_wall_shell["validation"]["printable"] != json!(true)
        || thick_wall_shell["validation"]["topology"]["watertight"] != json!(true)
        || thick_wall_shell["validation"]["topology"]["manifold"] != json!(true)
        || thick_wall_shell["validation"]["topology"]["consistently_oriented"] != json!(true)
        || thick_wall_shell["validation"]["topology"]["connected_components"] != json!(1)
        || thick_wall_shell["validation"]["overhang"]["requires_support"] != json!(false)
    {
        return Err(format!(
            "profile-derived thick-wall shell was unexpected: {thick_wall_shell}"
        ));
    }

    let mut pronounced_edge_profile = design_profile.clone();
    pronounced_edge_profile["manufacturing"]["minimum_wall_mm"] = json!(0.4);
    let pronounced_edge_shell = call_tool(
        &http,
        &mcp,
        &session,
        93,
        "printable_scad_compile",
        json!({
            "source": "pbl_shell();",
            "path": "smoke-pronounced-edge-shell.stl",
            "design_profile": pronounced_edge_profile,
            "overwrite": true,
        }),
    )
    .await?;
    if pronounced_edge_shell["artifact"]["path"] != json!("smoke-pronounced-edge-shell.stl")
        || pronounced_edge_shell["validation"]["printable"] != json!(true)
        || pronounced_edge_shell["validation"]["topology"]["watertight"] != json!(true)
        || pronounced_edge_shell["validation"]["topology"]["manifold"] != json!(true)
        || pronounced_edge_shell["validation"]["topology"]["consistently_oriented"] != json!(true)
        || pronounced_edge_shell["validation"]["topology"]["connected_components"] != json!(1)
        || pronounced_edge_shell["validation"]["overhang"]["requires_support"] != json!(false)
    {
        return Err(format!(
            "profile-derived pronounced-edge shell was unexpected: {pronounced_edge_shell}"
        ));
    }

    let rendered = call_tool(
        &http,
        &mcp,
        &session,
        10,
        "printable_scad_render",
        json!({
            "source": ENCLOSURE_SOURCE,
            "path": "smoke-enclosure.png",
            "variant": "print",
            "design_profile": design_profile.clone(),
            "view": "iso",
            "size": 128,
            "preview": true,
            "overwrite": true,
            "include_inline": true,
        }),
    )
    .await?;
    if rendered["artifact"]["path"] != json!("smoke-enclosure.png")
        || rendered["artifact"]["size_bytes"].as_u64().unwrap_or(0) == 0
        || rendered["view"] != json!("iso")
        || rendered["size"] != json!(128)
        || rendered["inline"]["included"] != json!(true)
        || rendered["definitions"]["count"] != json!(10)
        || rendered["design_profile"] != design_profile
    {
        return Err(format!("OpenSCAD render was unexpected: {rendered}"));
    }

    // 11. Compile and section a capsule grip with a restrained tapered neck.
    let compiled_grip = call_tool(
        &http,
        &mcp,
        &session,
        94,
        "printable_scad_compile",
        json!({
            "source": GRIP_SOURCE,
            "path": "smoke-grip.stl",
            "variant": "print",
            "design_profile": design_profile.clone(),
            "overwrite": true,
        }),
    )
    .await?;
    validate_compiled_product("grip", &compiled_grip, "smoke-grip.stl", 50.0)?;

    let section = call_tool(
        &http,
        &mcp,
        &session,
        11,
        "printable_scad_cross_section",
        json!({
            "source": GRIP_SOURCE,
            "path": "smoke-grip-section.svg",
            "variant": "print",
            "design_profile": design_profile.clone(),
            "z_mm": 5.0,
            "overwrite": true,
        }),
    )
    .await?;
    if section["artifact"]["path"] != json!("smoke-grip-section.svg")
        || section["artifact"]["size_bytes"].as_u64().unwrap_or(0) == 0
        || section["z_mm"] != json!(5.0)
        || section["definitions"]["variant_applied"] != json!(true)
        || section["definitions"]["count"] != json!(10)
    {
        return Err(format!("OpenSCAD cross-section was unexpected: {section}"));
    }

    // 12. Export the articulated fixture as two rigid bodies and certify its
    // complete intended arc before any mechanical presentation is allowed.
    let fixed_hinge = call_tool(
        &http,
        &mcp,
        &session,
        95,
        "printable_scad_compile",
        json!({
            "source": HINGE_SOURCE,
            "path": "smoke-hinge-fixed.stl",
            "variant": "fixed",
            "design_profile": design_profile.clone(),
            "overwrite": true,
        }),
    )
    .await?;
    validate_compiled_product(
        "hinge fixed body",
        &fixed_hinge,
        "smoke-hinge-fixed.stl",
        50.0,
    )?;

    let moving_hinge = call_tool(
        &http,
        &mcp,
        &session,
        96,
        "printable_scad_compile",
        json!({
            "source": HINGE_SOURCE,
            "path": "smoke-hinge-moving.stl",
            "variant": "moving",
            "design_profile": design_profile,
            "overwrite": true,
        }),
    )
    .await?;
    validate_compiled_product(
        "hinge moving body",
        &moving_hinge,
        "smoke-hinge-moving.stl",
        50.0,
    )?;

    let hinge_assembly = call_tool(
        &http,
        &mcp,
        &session,
        97,
        "printable_analyze_assembly",
        json!({
            "fixed_path": "smoke-hinge-fixed.stl",
            "moving_path": "smoke-hinge-moving.stl",
            "required_clearance_mm": 0.35,
            "rotation": {
                "pivot_mm": [0.0, 0.0, 6.4],
                "axis": [1.0, 0.0, 0.0],
                "angle_degrees": 90.0,
                "target_clearance_mm": 0.35
            }
        }),
    )
    .await?;
    validate_hinge_clearance(&hinge_assembly)?;

    let Some(source_blend) = durable_source else {
        return Ok(());
    };
    let workspace_root = workspace_root.ok_or(
        "PRINTABLE_SMOKE_WORKSPACE_ROOT is required when paired Blender acceptance is enabled",
    )?;

    // 13. Exercise product presentation and certified animation through the
    // paired Blender service.
    let restored = call_tool(
        &http,
        &mcp,
        &session,
        100,
        "printable_scene_restore",
        json!({"path": source_blend}),
    )
    .await?;
    if restored["scene"].as_str().is_none()
        || restored["object_count"]
            .as_u64()
            .is_none_or(|count| count == 0)
    {
        return Err(format!(
            "paired Blender source was not restored through the public tool: {restored}"
        ));
    }
    smoke_modeling_inspection(&http, &mcp, &session).await?;
    smoke_product_presentations(&http, &mcp, &session).await?;
    smoke_mechanical_render_job(&http, &mcp, &session, workspace_root).await?;

    Ok(())
}

fn validate_compiled_product(
    product: &str,
    compiled: &Value,
    expected_path: &str,
    overhang_degrees: f64,
) -> Result<(), String> {
    if compiled["artifact"]["path"] != json!(expected_path)
        || compiled["validation"]["printable"] != json!(true)
        || compiled["validation"]["topology"]["watertight"] != json!(true)
        || compiled["validation"]["topology"]["manifold"] != json!(true)
        || compiled["validation"]["topology"]["consistently_oriented"] != json!(true)
        || compiled["validation"]["topology"]["connected_components"] != json!(1)
        || compiled["validation"]["overhang"]["bed_contact"]["faces"]
            .as_u64()
            .is_none_or(|faces| faces == 0)
        || compiled["validation"]["overhang"]["threshold_degrees"] != json!(overhang_degrees)
        || compiled["validation"]["overhang"]["requires_support"] != json!(false)
        || compiled["manufacturing_evidence"]["global_minimum_wall"]["status"]
            != json!("not_certified")
        || compiled["manufacturing_evidence"]["moving_clearance"]["status"] != json!("not_run")
    {
        return Err(format!(
            "{product} compile did not satisfy the product acceptance contract: {compiled}"
        ));
    }
    Ok(())
}

fn validate_hinge_clearance(assembly: &Value) -> Result<(), String> {
    let clearance = assembly["report"]["rotation"]["minimum_certified_clearance_mm"].as_f64();
    if assembly["report"]["static_analysis"]["relation"] != json!("separated")
        || assembly["report"]["static_analysis"]["clearance_mm"]
            .as_f64()
            .is_none_or(|value| value < 0.35)
        || assembly["report"]["rotation"]["can_rotate_full_angle"] != json!(true)
        || clearance.is_none_or(|value| value < 0.35)
        || assembly["report"]["rotation"]["target_clearance_mm"] != json!(0.35)
    {
        return Err(format!(
            "articulated product did not certify the complete requested arc: {assembly}"
        ));
    }
    Ok(())
}

async fn smoke_modeling_inspection(
    http: &reqwest::Client,
    mcp: &str,
    session: &str,
) -> Result<(), String> {
    call_tool(http, mcp, session, 800, "printable_scene_clear", json!({})).await?;
    let guide = include_str!("../../resources/blender-modeling-v1.md");
    let examples: Vec<&str> = guide
        .split("```python\n")
        .skip(1)
        .map(|part| part.split("```").next().expect("example body"))
        .collect();
    if examples.len() != 3 {
        return Err("modeling guide must supply query, creation, and revision examples".into());
    }
    for (id, code, width) in [
        (801, examples[1], 2.0),
        (802, examples[2], 3.0),
        (803, examples[0], 3.0),
    ] {
        let executed = call_tool(
            http,
            mcp,
            session,
            id,
            "printable_blender_execute",
            json!({"code": code}),
        )
        .await?;
        if executed["result"]["bevel_width"].as_f64() != Some(width) {
            return Err(format!(
                "modeling guide execution did not observe the edited bevel: {executed}"
            ));
        }
    }
    let setup = r#"
obj = bpy.data.objects["Handle"]
material = bpy.data.materials.new("HandleMaterial")
material.use_nodes = True
obj.data.materials.append(material)
group = bpy.data.node_groups.new("HandleGeometry", "GeometryNodeTree")
group.interface.new_socket(name="Geometry", in_out="INPUT", socket_type="NodeSocketGeometry")
group.interface.new_socket(name="Geometry", in_out="OUTPUT", socket_type="NodeSocketGeometry")
source = group.nodes.new("NodeGroupInput")
sink = group.nodes.new("NodeGroupOutput")
group.links.new(source.outputs["Geometry"], sink.inputs["Geometry"])
modifier = obj.modifiers.new("PassThrough", "NODES")
modifier.node_group = group
result = {"object": obj.name}
"#;
    call_tool(
        http,
        mcp,
        session,
        804,
        "printable_blender_execute",
        json!({"code": setup}),
    )
    .await?;
    let scene = call_tool(http, mcp, session, 805, "printable_scene_get",
        json!({"name_contains":"handle", "object_type":"MESH", "include_transforms":false,"limit":1})).await?;
    if scene["objects"] != json!([{"name":"Handle","type":"MESH"}]) {
        return Err(format!("filtered modeling scene mismatch: {scene}"));
    }
    let materials = call_tool(
        http,
        mcp,
        session,
        806,
        "printable_object_get",
        json!({"name":"Handle", "section":"materials"}),
    )
    .await?;
    let modifiers = call_tool(
        http,
        mcp,
        session,
        807,
        "printable_object_get",
        json!({"name":"Handle", "section":"modifiers", "limit":1,"offset":1}),
    )
    .await?;
    if materials["items"][0]["material"] != "HandleMaterial"
        || modifiers["items"][0]["node_group"] != "HandleGeometry"
    {
        return Err(format!(
            "modeling object details mismatch: {materials}, {modifiers}"
        ));
    }
    for (id, kind, name) in [
        (808, "material", "HandleMaterial"),
        (809, "geometry", "HandleGeometry"),
    ] {
        let nodes = call_tool(
            http,
            mcp,
            session,
            id,
            "printable_node_tree_get",
            json!({"name":name,"kind":kind,"limit":1}),
        )
        .await?;
        let links = call_tool(
            http,
            mcp,
            session,
            id + 2,
            "printable_node_tree_get",
            json!({"name":name,"kind":kind,"section":"links"}),
        )
        .await?;
        if nodes["items"].as_array().map(Vec::len) != Some(1)
            || nodes["next_offset"] != 1
            || links["items"].as_array().is_none_or(Vec::is_empty)
        {
            return Err(format!("modeling node topology mismatch: {nodes}, {links}"));
        }
    }
    call_tool(
        http,
        mcp,
        session,
        812,
        "printable_render_product",
        json!({
            "path":"smoke/modeling-handle.png","objects":["Handle"],
            "presentation":{"profile":"engineering"},"width":160,"height":120,
            "engine":"EEVEE","include_inline":false
        }),
    )
    .await?;
    Ok(())
}

async fn smoke_product_presentations(
    http: &reqwest::Client,
    mcp: &str,
    session: &str,
) -> Result<(), String> {
    call_tool(http, mcp, session, 101, "printable_scene_clear", json!({})).await?;
    import_and_rename(
        http,
        mcp,
        session,
        102,
        103,
        "smoke-enclosure.stl",
        "AcceptanceEnclosure",
    )
    .await?;
    let enclosure_scene =
        call_tool(http, mcp, session, 104, "printable_scene_get", json!({})).await?;
    let mut product_images = Vec::new();
    for (request_id, read_id, profile, camera_type) in [
        (105, 106, "engineering", "orthographic"),
        (107, 108, "studio_neutral", "perspective"),
        (109, 110, "studio_dark", "perspective"),
    ] {
        let path = format!("smoke/enclosure-{profile}.png");
        let rendered = call_tool(
            http,
            mcp,
            session,
            request_id,
            "printable_render_product",
            json!({
                "path": path,
                "objects": ["AcceptanceEnclosure"],
                "presentation": {
                    "profile": profile,
                    "view": {
                        "azimuth_degrees": 40.0,
                        "elevation_degrees": 24.0
                    },
                    "materials": [{
                        "objects": ["AcceptanceEnclosure"],
                        "base_color_srgb": [0.74, 0.31, 0.12],
                        "metallic": 0.0,
                        "roughness": 0.34
                    }]
                },
                "width": 160,
                "height": 120,
                "engine": "EEVEE",
                "include_inline": false
            }),
        )
        .await?;
        validate_product_render(
            &rendered,
            &path,
            profile,
            camera_type,
            "AcceptanceEnclosure",
        )?;
        product_images.push(read_png(http, mcp, session, read_id, &path, 160, 120).await?);
    }

    if product_images
        .iter()
        .any(|image| !image.has_visible_contrast())
        || !product_images[0].has_uniform_border(4, 8)
        || mean_absolute_rgb_difference(&product_images[0], &product_images[1]) < 4.0
        || mean_absolute_rgb_difference(&product_images[1], &product_images[2]) < 4.0
        || mean_absolute_rgb_difference(&product_images[0], &product_images[2]) < 4.0
    {
        return Err(format!(
            "named presentation profiles were cropped, blank, or not visibly distinct: engineering_border={}, differences={:.2},{:.2},{:.2}",
            product_images[0].has_uniform_border(4, 8),
            mean_absolute_rgb_difference(&product_images[0], &product_images[1]),
            mean_absolute_rgb_difference(&product_images[1], &product_images[2]),
            mean_absolute_rgb_difference(&product_images[0], &product_images[2]),
        ));
    }

    let enclosure_after =
        call_tool(http, mcp, session, 111, "printable_scene_get", json!({})).await?;
    require_unchanged_scene("product stills", &enclosure_scene, &enclosure_after)?;

    call_tool(http, mcp, session, 112, "printable_scene_clear", json!({})).await?;
    import_and_rename(
        http,
        mcp,
        session,
        113,
        114,
        "smoke-bracket.stl",
        "AcceptanceBracket",
    )
    .await?;
    let bracket_scene =
        call_tool(http, mcp, session, 115, "printable_scene_get", json!({})).await?;
    let gallery = call_tool(
        http,
        mcp,
        session,
        116,
        "printable_render_gallery",
        json!({
            "path": "smoke/bracket-gallery.png",
            "views": ["front", "right", "isometric"],
            "columns": 3,
            "presentation": {
                "profile": "studio_neutral",
                "materials": [{
                    "objects": ["AcceptanceBracket"],
                    "base_color_srgb": [0.22, 0.36, 0.58],
                    "metallic": 0.0,
                    "roughness": 0.42
                }]
            },
            "width": 96,
            "height": 72,
            "engine": "EEVEE",
            "include_inline": false
        }),
    )
    .await?;
    validate_composite_render(
        &gallery,
        "gallery",
        "smoke/bracket-gallery.png",
        "studio_neutral",
        3,
    )?;
    let gallery_views =
        read_response_view_pngs(http, mcp, session, 8_100, &gallery, 96, 72).await?;
    require_visibly_distinct_images("product gallery", &gallery_views)?;
    read_response_png(http, mcp, session, 117, &gallery).await?;
    let bracket_after =
        call_tool(http, mcp, session, 118, "printable_scene_get", json!({})).await?;
    require_unchanged_scene("product gallery", &bracket_scene, &bracket_after)?;

    call_tool(http, mcp, session, 119, "printable_scene_clear", json!({})).await?;
    import_and_rename(
        http,
        mcp,
        session,
        120,
        121,
        "smoke-grip.stl",
        "AcceptanceGrip",
    )
    .await?;
    let grip_scene = call_tool(http, mcp, session, 122, "printable_scene_get", json!({})).await?;
    let turntable = call_tool(
        http,
        mcp,
        session,
        123,
        "printable_render_turntable",
        json!({
            "path": "smoke/grip-turntable.png",
            "frames": 4,
            "columns": 2,
            "elevation_degrees": 24.0,
            "presentation": {
                "profile": "studio_dark",
                "materials": [{
                    "objects": ["AcceptanceGrip"],
                    "base_color_srgb": [0.68, 0.28, 0.10],
                    "metallic": 0.05,
                    "roughness": 0.32
                }]
            },
            "width": 96,
            "height": 72,
            "engine": "EEVEE",
            "include_inline": false
        }),
    )
    .await?;
    validate_composite_render(
        &turntable,
        "turntable",
        "smoke/grip-turntable.png",
        "studio_dark",
        4,
    )?;
    if turntable["orbit"]["frames"] != json!(4) {
        return Err(format!(
            "product turntable orbit was unexpected: {turntable}"
        ));
    }
    let turntable_views =
        read_response_view_pngs(http, mcp, session, 8_200, &turntable, 96, 72).await?;
    require_visibly_distinct_images("product turntable", &turntable_views)?;
    read_response_png(http, mcp, session, 124, &turntable).await?;
    let grip_after = call_tool(http, mcp, session, 125, "printable_scene_get", json!({})).await?;
    require_unchanged_scene("product turntable", &grip_scene, &grip_after)?;

    Ok(())
}

async fn smoke_mechanical_render_job(
    http: &reqwest::Client,
    mcp: &str,
    session: &str,
    workspace_root: &Path,
) -> Result<(), String> {
    call_tool(http, mcp, session, 130, "printable_scene_clear", json!({})).await?;
    import_and_rename(
        http,
        mcp,
        session,
        131,
        132,
        "smoke-hinge-fixed.stl",
        "AcceptanceHingeFixed",
    )
    .await?;
    import_and_rename(
        http,
        mcp,
        session,
        133,
        134,
        "smoke-hinge-moving.stl",
        "AcceptanceHingeMoving",
    )
    .await?;
    let source_scene = call_tool(http, mcp, session, 135, "printable_scene_get", json!({})).await?;
    let checkpoint = call_tool(
        http,
        mcp,
        session,
        136,
        "printable_scene_checkpoint",
        json!({"path": "smoke/hinge-product.blend"}),
    )
    .await?;
    if checkpoint["path"] != json!("smoke/hinge-product.blend") {
        return Err(format!(
            "mechanical product checkpoint was unexpected: {checkpoint}"
        ));
    }
    let submitted = call_tool(
        http,
        mcp,
        session,
        137,
        "printable_render_job_submit",
        json!({
            "source_blend": "smoke/hinge-product.blend",
            "kind": "mechanical_rotation",
            "mechanical_rotation": {
                "fixed_objects": ["AcceptanceHingeFixed"],
                "moving_objects": ["AcceptanceHingeMoving"],
                "pivot_mm": [0.0, 0.0, 6.4],
                "axis": [1.0, 0.0, 0.0],
                "angle_degrees": 90.0,
                "target_clearance_mm": 0.35
            },
            "frame_start": 1,
            "frame_end": 3,
            "frame_step": 1,
            "frames_per_second": 3,
            "width": 96,
            "height": 72,
            "engine": "EEVEE",
            "presentation": {
                "profile": "studio_dark",
                "view": {
                    "azimuth_degrees": 42.0,
                    "elevation_degrees": 24.0
                },
                "materials": [
                    {
                        "objects": ["AcceptanceHingeFixed"],
                        "base_color_srgb": [0.20, 0.28, 0.38],
                        "metallic": 0.05,
                        "roughness": 0.34
                    },
                    {
                        "objects": ["AcceptanceHingeMoving"],
                        "base_color_srgb": [0.76, 0.30, 0.10],
                        "metallic": 0.05,
                        "roughness": 0.32
                    }
                ]
            },
            "frame_timeout_seconds": 300.0,
            "encode_timeout_seconds": 300.0
        }),
    )
    .await?;
    let job_id = submitted["job_id"]
        .as_str()
        .ok_or_else(|| format!("mechanical render submission returned no job id: {submitted}"))?
        .to_string();
    let isolated = std::env::var_os("PRINTABLE_SMOKE_REQUIRE_ISOLATED_WORKER").is_some();
    let mut live_edit_state = None;
    if isolated {
        if submitted["execution"]["mode"] != "isolated_worker" {
            return Err("paired job did not select the isolated worker".into());
        }
        tokio::time::timeout(Duration::from_secs(300), async {
            loop {
                let current = call_tool(http, mcp, session, 30_000, "printable_render_job_status",
                    json!({"job_id": job_id})).await?;
                if current["progress"]["phase"] == "rendering" { break; }
                if matches!(current["state"].as_str(), Some("succeeded" | "failed" | "cancelled")) {
                    return Err(format!("worker terminated before concurrent observation: {current}"));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let edited = tokio::time::timeout(Duration::from_secs(5),
                call_tool(http, mcp, session, 30_001, "printable_blender_execute",
                    json!({"code": "bpy.context.scene['printable_worker_probe'] = 'live edit during rendering'\nresult = True"})))
                .await.map_err(|_| "live editing was blocked by the render worker".to_string())??;
            if edited["result"] != true { return Err("live edit was not acknowledged".into()); }
            live_edit_state = Some(edited["scene_state"].clone());
            Ok::<_, String>(())
        }).await.map_err(|_| "worker did not enter rendering before the concurrency deadline".to_string())??;
    }
    let completed = wait_for_render_job(http, mcp, session, &job_id, 138).await?;
    let execution_verified = if isolated {
        completed["execution"]["blender_finished"] == true
            && completed["session_checkpoint_captured"] == false
            && completed["session_checkpoint_restored"] == false
    } else {
        completed["session_checkpoint_captured"] == true
            && completed["session_checkpoint_restored"] == true
    };
    if !execution_verified
        || completed["progress"]["completed_frames"] != json!(3)
        || completed["progress"]["total_frames"] != json!(3)
        || completed["mechanical_analysis"]["certified"] != json!(true)
        || completed["mechanical_analysis"]["report"]["rotation"]["can_rotate_full_angle"]
            != json!(true)
        || completed["mechanical_analysis"]["report"]["rotation"]["minimum_certified_clearance_mm"]
            .as_f64()
            .is_none_or(|clearance| clearance < 0.35)
        || completed["spec"]["presentation"]["profile"] != json!("studio_dark")
        || completed["presentation_bounds"].is_null()
        || !completed["failure"].is_null()
    {
        return Err(format!(
            "mechanical product render did not retain its clearance certificate: {completed}"
        ));
    }
    let artifacts = call_tool(
        http,
        mcp,
        session,
        10_000,
        "printable_render_job_artifacts",
        json!({"job_id": job_id}),
    )
    .await?;
    validate_mechanical_artifacts(http, mcp, session, &artifacts, workspace_root).await?;

    let restored_scene =
        call_tool(http, mcp, session, 10_010, "printable_scene_get", json!({})).await?;
    require_unchanged_scene("mechanical render job", &source_scene, &restored_scene)?;
    if let Some(expected) = live_edit_state
        && (expected.is_null() || restored_scene["scene_state"] != expected)
    {
        return Err(
            "worker execution changed the live scene revision after the concurrent edit".into(),
        );
    }
    if isolated {
        let retained = call_tool(http, mcp, session, 30_002, "printable_blender_execute",
            json!({"code": "result = bpy.context.scene.get('printable_worker_probe')\nif result is not None: del bpy.context.scene['printable_worker_probe']"})).await?;
        if retained["result"] != "live edit during rendering" {
            return Err("render completion replaced the live scene edit".into());
        }
    }
    let status = call_tool(http, mcp, session, 10_011, "printable_status", json!({})).await?;
    if status["render_jobs"]["recovery_fenced"] != json!(false) {
        return Err(format!(
            "mechanical render left ordinary Blender access fenced: {status}"
        ));
    }
    Ok(())
}

async fn wait_for_render_job(
    http: &reqwest::Client,
    mcp: &str,
    session: &str,
    job_id: &str,
    first_request_id: u64,
) -> Result<Value, String> {
    tokio::time::timeout(Duration::from_secs(600), async {
        let mut request_id = first_request_id;
        loop {
            let status = call_tool(
                http,
                mcp,
                session,
                request_id,
                "printable_render_job_status",
                json!({"job_id": job_id}),
            )
            .await?;
            request_id = request_id.saturating_add(1);
            match status["state"].as_str() {
                Some("succeeded") => return Ok(status),
                Some("failed" | "cancelled") => {
                    return Err(format!("durable render job ended unsuccessfully: {status}"));
                }
                Some("queued" | "running") => tokio::time::sleep(Duration::from_secs(1)).await,
                _ => {
                    return Err(format!(
                        "durable render job returned invalid state: {status}"
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| "durable render job did not finish within the CI smoke watchdog".to_string())?
}

async fn import_and_rename(
    http: &reqwest::Client,
    mcp: &str,
    session: &str,
    import_request_id: u64,
    rename_request_id: u64,
    path: &str,
    name: &str,
) -> Result<(), String> {
    let imported = call_tool(
        http,
        mcp,
        session,
        import_request_id,
        "printable_stl_import",
        json!({"path": path}),
    )
    .await?;
    let objects = imported["objects"]
        .as_array()
        .filter(|objects| objects.len() == 1)
        .ok_or_else(|| format!("STL import did not create exactly one object: {imported}"))?;
    let imported_name = objects[0]["name"]
        .as_str()
        .ok_or_else(|| format!("STL import omitted the object name: {imported}"))?;
    let renamed = call_tool(
        http,
        mcp,
        session,
        rename_request_id,
        "printable_object_rename",
        json!({"name": imported_name, "new_name": name}),
    )
    .await?;
    if renamed["name"] != json!(name) || renamed["type"] != json!("MESH") {
        return Err(format!(
            "imported product was not renamed as requested: {renamed}"
        ));
    }
    Ok(())
}

fn validate_product_render(
    rendered: &Value,
    path: &str,
    profile: &str,
    camera_type: &str,
    object: &str,
) -> Result<(), String> {
    if rendered["path"] != json!(path)
        || rendered["size_bytes"].as_u64().is_none_or(|size| size == 0)
        || rendered["sha256"]
            .as_str()
            .is_none_or(|digest| digest.len() != 64)
        || rendered["width"] != json!(160)
        || rendered["height"] != json!(120)
        || rendered["presentation"]["profile"] != json!(profile)
        || rendered["presentation"]["camera"]["type"] != json!(camera_type)
        || rendered["presentation"]["framing"]["margin_percent"] != json!(15.0)
        || rendered["presentation"]["materials"]["overrides"][0]["objects"] != json!([object])
        || rendered["source_state_verified"] != json!(true)
        || rendered["cleanup_verified"] != json!(true)
        || rendered["inline"]["included"] != json!(false)
    {
        return Err(format!(
            "{profile} product render did not satisfy the presentation contract: {rendered}"
        ));
    }
    Ok(())
}

fn validate_composite_render(
    rendered: &Value,
    kind: &str,
    path: &str,
    profile: &str,
    view_count: usize,
) -> Result<(), String> {
    if rendered["kind"] != json!(kind)
        || rendered["path"] != json!(path)
        || rendered["size_bytes"].as_u64().is_none_or(|size| size == 0)
        || rendered["width"].as_u64().is_none_or(|width| width == 0)
        || rendered["height"].as_u64().is_none_or(|height| height == 0)
        || rendered["views"]
            .as_array()
            .is_none_or(|views| views.len() != view_count)
        || rendered["presentation"]["profile"] != json!(profile)
        || rendered["inline"]["included"] != json!(false)
    {
        return Err(format!(
            "{kind} did not satisfy the product presentation contract: {rendered}"
        ));
    }
    Ok(())
}

fn require_unchanged_scene(context: &str, before: &Value, after: &Value) -> Result<(), String> {
    let mut before = before.clone();
    let mut after = after.clone();
    for scene in [&mut before, &mut after] {
        if let Some(object) = scene.as_object_mut() {
            object.remove("scene_state");
        }
    }
    if before != after {
        return Err(format!(
            "{context} changed the authored Blender scene: before={before}, after={after}"
        ));
    }
    Ok(())
}

struct DecodedPng {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

impl DecodedPng {
    fn has_visible_contrast(&self) -> bool {
        if self.rgb.len() != self.width as usize * self.height as usize * 3 {
            return false;
        }
        let mut minimum = [u8::MAX; 3];
        let mut maximum = [u8::MIN; 3];
        for pixel in self.rgb.chunks_exact(3) {
            for channel in 0..3 {
                minimum[channel] = minimum[channel].min(pixel[channel]);
                maximum[channel] = maximum[channel].max(pixel[channel]);
            }
        }
        (0..3).any(|channel| {
            maximum[channel].saturating_sub(minimum[channel]) >= MIN_VISIBLE_CHANNEL_RANGE
        })
    }

    fn has_uniform_border(&self, thickness: u32, tolerance: u8) -> bool {
        if thickness == 0
            || self.width < thickness.saturating_mul(2)
            || self.height < thickness.saturating_mul(2)
            || self.rgb.len() != self.width as usize * self.height as usize * 3
        {
            return false;
        }
        let reference = &self.rgb[..3];
        (0..self.height).all(|y| {
            (0..self.width).all(|x| {
                if x >= thickness
                    && x < self.width - thickness
                    && y >= thickness
                    && y < self.height - thickness
                {
                    return true;
                }
                let offset = (y as usize * self.width as usize + x as usize) * 3;
                self.rgb[offset..offset + 3]
                    .iter()
                    .zip(reference)
                    .all(|(actual, expected)| actual.abs_diff(*expected) <= tolerance)
            })
        })
    }
}

fn mean_absolute_rgb_difference(left: &DecodedPng, right: &DecodedPng) -> f64 {
    if left.width != right.width || left.height != right.height || left.rgb.len() != right.rgb.len()
    {
        return 0.0;
    }
    let difference: u64 = left
        .rgb
        .iter()
        .zip(&right.rgb)
        .map(|(left, right)| u64::from(left.abs_diff(*right)))
        .sum();
    difference as f64 / left.rgb.len() as f64
}

fn require_visibly_distinct_images(context: &str, images: &[DecodedPng]) -> Result<(), String> {
    if images.len() < 2 {
        return Err(format!(
            "{context} did not provide enough images to demonstrate distinct views"
        ));
    }
    for left_index in 0..images.len() {
        for right_index in left_index + 1..images.len() {
            let difference =
                mean_absolute_rgb_difference(&images[left_index], &images[right_index]);
            if difference < MIN_DISTINCT_IMAGE_DIFFERENCE {
                return Err(format!(
                    "{context} images {} and {} were not visibly distinct: mean RGB difference {difference:.2}",
                    left_index + 1,
                    right_index + 1
                ));
            }
        }
    }
    Ok(())
}

async fn read_response_view_pngs(
    http: &reqwest::Client,
    mcp: &str,
    session: &str,
    first_request_id: u64,
    response: &Value,
    expected_width: u32,
    expected_height: u32,
) -> Result<Vec<DecodedPng>, String> {
    let views = response["views"]
        .as_array()
        .ok_or_else(|| format!("render response omitted its source views: {response}"))?;
    let mut images = Vec::with_capacity(views.len());
    for (index, view) in views.iter().enumerate() {
        let path = view["path"]
            .as_str()
            .ok_or_else(|| format!("rendered view omitted its path: {view}"))?;
        if view["media_type"] != json!("image/png")
            || view["width"] != json!(expected_width)
            || view["height"] != json!(expected_height)
        {
            return Err(format!(
                "rendered view did not retain its PNG dimensions: {view}"
            ));
        }
        let image = read_png(
            http,
            mcp,
            session,
            first_request_id + index as u64,
            path,
            expected_width,
            expected_height,
        )
        .await?;
        if !image.has_visible_contrast() {
            return Err(format!("rendered source view was visually uniform: {path}"));
        }
        images.push(image);
    }
    Ok(images)
}

async fn read_response_png(
    http: &reqwest::Client,
    mcp: &str,
    session: &str,
    request_id: u64,
    response: &Value,
) -> Result<DecodedPng, String> {
    let path = response["path"]
        .as_str()
        .ok_or_else(|| format!("render response omitted its path: {response}"))?;
    let width = response["width"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| format!("render response omitted its width: {response}"))?;
    let height = response["height"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| format!("render response omitted its height: {response}"))?;
    let image = read_png(http, mcp, session, request_id, path, width, height).await?;
    if !image.has_visible_contrast() {
        return Err(format!("rendered PNG was visually uniform: {path}"));
    }
    Ok(image)
}

async fn read_png(
    http: &reqwest::Client,
    mcp: &str,
    session: &str,
    request_id: u64,
    path: &str,
    expected_width: u32,
    expected_height: u32,
) -> Result<DecodedPng, String> {
    let bytes = read_workspace_bytes(http, mcp, session, request_id, path).await?;
    let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .map_err(|error| format!("decode PNG {path}: {error}"))?
        .to_rgb8();
    if image.width() != expected_width || image.height() != expected_height {
        return Err(format!(
            "PNG {path} dimensions were {}x{}, expected {expected_width}x{expected_height}",
            image.width(),
            image.height()
        ));
    }
    Ok(DecodedPng {
        width: image.width(),
        height: image.height(),
        rgb: image.into_raw(),
    })
}

async fn read_workspace_bytes(
    http: &reqwest::Client,
    mcp: &str,
    session: &str,
    request_id: u64,
    path: &str,
) -> Result<Vec<u8>, String> {
    let read = call_tool(
        http,
        mcp,
        session,
        request_id,
        "printable_workspace_read",
        json!({"path": path}),
    )
    .await?;
    let encoded = read["data_base64"]
        .as_str()
        .ok_or_else(|| format!("workspace read omitted data_base64 for {path}: {read}"))?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("workspace artifact {path} was not valid base64: {error}"))
}

async fn validate_mechanical_artifacts(
    http: &reqwest::Client,
    mcp: &str,
    session: &str,
    artifacts: &Value,
    workspace_root: &Path,
) -> Result<(), String> {
    if artifacts["state"] != json!("succeeded")
        || artifacts["available_frames"] != json!(3)
        || artifacts["total_frames"] != json!(3)
        || artifacts["mechanical_analysis"]["certified"] != json!(true)
    {
        return Err(format!(
            "mechanical artifact listing did not retain its certificate: {artifacts}"
        ));
    }
    let frames = artifacts["frames"]
        .as_array()
        .filter(|frames| frames.len() == 3)
        .ok_or_else(|| format!("mechanical job did not expose three frames: {artifacts}"))?;
    let mut frame_images = Vec::with_capacity(frames.len());
    for (index, frame) in frames.iter().enumerate() {
        let ordinal = index as u64 + 1;
        let path = frame["path"]
            .as_str()
            .ok_or_else(|| format!("mechanical frame omitted its path: {frame}"))?;
        if frame["sequence_index"] != json!(ordinal)
            || frame["source_frame"] != json!(ordinal)
            || frame["media_type"] != json!("image/png")
        {
            return Err(format!("mechanical frame sequence was invalid: {frame}"));
        }
        let image = read_png(http, mcp, session, 10_001 + ordinal, path, 96, 72).await?;
        if !image.has_visible_contrast() {
            return Err(format!("mechanical frame was visually uniform: {path}"));
        }
        frame_images.push(image);
    }
    require_visibly_distinct_images("mechanical animation", &frame_images)?;

    let video = &artifacts["video"];
    let video_path = video["path"]
        .as_str()
        .ok_or_else(|| format!("mechanical job omitted its video path: {artifacts}"))?;
    if video["media_type"] != json!("video/mp4")
        || video["size_bytes"].as_u64().is_none_or(|size| size == 0)
    {
        return Err(format!(
            "mechanical job did not expose its MP4 artifact: {artifacts}"
        ));
    }
    if !video_path.starts_with(".printable/jobs/") {
        return Err(format!(
            "mechanical video was not retained as a durable workspace path: {artifacts}"
        ));
    }
    validate_mechanical_video(workspace_root, video_path, 3, 1.0).await?;
    Ok(())
}

async fn validate_mechanical_video(
    workspace_root: &Path,
    video_path: &str,
    expected_frames: u64,
    expected_duration_seconds: f64,
) -> Result<(), String> {
    let video = resolve_smoke_workspace_file(workspace_root, video_path)?;
    let ffmpeg = std::env::var_os("FFMPEG_BIN").unwrap_or_else(|| "/usr/bin/ffmpeg".into());
    let mut decode = tokio::process::Command::new(ffmpeg);
    decode
        .args(["-v", "error", "-xerror", "-nostdin", "-i"])
        .arg(&video)
        .args(["-map", "0:v:0", "-f", "null", "-"]);
    let decoded = run_media_command(decode, "FFmpeg video decode").await?;
    if !decoded.status.success() {
        return Err(format!(
            "mechanical MP4 did not fully decode: {}",
            bounded_stderr(&decoded.stderr)
        ));
    }

    let ffprobe = std::env::var_os("FFPROBE_BIN").unwrap_or_else(|| "/usr/bin/ffprobe".into());
    let mut probe = tokio::process::Command::new(ffprobe);
    probe
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames:format=duration",
            "-of",
            "json",
        ])
        .arg(&video);
    let probed = run_media_command(probe, "FFprobe video inspection").await?;
    if !probed.status.success() {
        return Err(format!(
            "mechanical MP4 could not be inspected: {}",
            bounded_stderr(&probed.stderr)
        ));
    }
    let metadata: Value = serde_json::from_slice(&probed.stdout)
        .map_err(|error| format!("FFprobe returned invalid JSON: {error}"))?;
    let (frames, duration_seconds) = parse_video_probe(&metadata)?;
    if frames != expected_frames || (duration_seconds - expected_duration_seconds).abs() > 0.05 {
        return Err(format!(
            "mechanical MP4 timing was {frames} frames over {duration_seconds:.3} seconds, expected {expected_frames} frames over {expected_duration_seconds:.3} seconds"
        ));
    }
    Ok(())
}

fn resolve_smoke_workspace_file(
    workspace_root: &Path,
    relative_path: &str,
) -> Result<PathBuf, String> {
    let relative = Path::new(relative_path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "mechanical video path was not a confined relative path: {relative_path}"
        ));
    }
    let root = std::fs::canonicalize(workspace_root).map_err(|error| {
        format!(
            "resolve smoke workspace root {}: {error}",
            workspace_root.display()
        )
    })?;
    let target = std::fs::canonicalize(root.join(relative))
        .map_err(|error| format!("resolve mechanical video {relative_path}: {error}"))?;
    if !target.starts_with(&root) {
        return Err(format!(
            "mechanical video resolved outside the smoke workspace: {relative_path}"
        ));
    }
    let metadata = std::fs::metadata(&target)
        .map_err(|error| format!("inspect mechanical video {relative_path}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!(
            "mechanical video was not a regular file: {relative_path}"
        ));
    }
    Ok(target)
}

async fn run_media_command(
    mut command: tokio::process::Command,
    context: &str,
) -> Result<std::process::Output, String> {
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command
        .spawn()
        .map_err(|error| format!("start {context}: {error}"))?;
    tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .map_err(|_| format!("{context} exceeded 60 seconds"))?
        .map_err(|error| format!("wait for {context}: {error}"))
}

fn parse_video_probe(metadata: &Value) -> Result<(u64, f64), String> {
    let frames = metadata["streams"][0]["nb_read_frames"]
        .as_str()
        .ok_or_else(|| format!("FFprobe omitted decoded frame count: {metadata}"))?
        .parse::<u64>()
        .map_err(|error| format!("FFprobe returned invalid frame count: {error}"))?;
    let duration_seconds = metadata["format"]["duration"]
        .as_str()
        .ok_or_else(|| format!("FFprobe omitted video duration: {metadata}"))?
        .parse::<f64>()
        .map_err(|error| format!("FFprobe returned invalid duration: {error}"))?;
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return Err(format!(
            "FFprobe returned invalid video duration: {duration_seconds}"
        ));
    }
    Ok((frames, duration_seconds))
}

fn bounded_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(&stderr[..stderr.len().min(2048)]).into_owned()
}

fn smoke_cube_stl() -> Vec<u8> {
    smoke_cube_stl_at([0.0; 3])
}

fn smoke_cube_stl_at(offset: [f32; 3]) -> Vec<u8> {
    let size = 10.0_f32;
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
    let triangles = [
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
    ];
    let mut bytes = vec![0; 80];
    bytes.extend_from_slice(&(triangles.len() as u32).to_le_bytes());
    for triangle in triangles {
        for component in [0.0_f32; 3] {
            bytes.extend_from_slice(&component.to_le_bytes());
        }
        for index in triangle {
            for component in vertices[index] {
                bytes.extend_from_slice(&component.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
    }
    bytes
}

fn check_tool_catalog(result: &Value, expected_tools: &BTreeSet<String>) -> Result<(), String> {
    let tools = result["tools"]
        .as_array()
        .ok_or("tools/list result has no tools array")?;
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .ok_or("tools/list contains a tool without a string name")
        })
        .collect::<Result<_, _>>()?;
    let actual: BTreeSet<String> = names.iter().map(|name| (*name).to_string()).collect();
    if actual.len() != names.len() {
        return Err(format!(
            "tools/list contains duplicate tool names: {names:?}"
        ));
    }
    if &actual != expected_tools {
        let missing: Vec<&String> = expected_tools.difference(&actual).collect();
        let unexpected: Vec<&String> = actual.difference(expected_tools).collect();
        return Err(format!(
            "tool catalog mismatch; missing={missing:?}, unexpected={unexpected:?}"
        ));
    }
    Ok(())
}

/// Call one tool and return its parsed JSON result object (the tool text content,
/// re-parsed), failing on an `is_error` result.
async fn call_tool(
    http: &reqwest::Client,
    mcp: &str,
    session: &str,
    id: u64,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let (name, arguments) = workflow::request(name, arguments)?;
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    let result = extract_result(post(http, mcp, Some(session), &req).await?).await?;
    if result["isError"] == json!(true) {
        return Err(format!("tool {name} returned is_error: {result}"));
    }
    let text = result["content"][0]["text"]
        .as_str()
        .ok_or_else(|| format!("tool {name} result has no text content: {result}"))?;
    let parsed: Value =
        serde_json::from_str(text).map_err(|e| format!("tool {name} text is not JSON: {e}"))?;
    if result["structuredContent"] != parsed {
        return Err(format!("tool {name} text and structured content disagree"));
    }
    let schema = OUTPUT_SCHEMAS
        .get()
        .and_then(|schemas| schemas.get(name))
        .ok_or_else(|| format!("missing discovered output schema for {name}"))?;
    if let Err(error) = schema.validate(&parsed) {
        return Err(format!(
            "tool {name} violates its discovered output schema: {error}"
        ));
    }
    tracing::info!(
        tool = name,
        request_json_bytes = req.to_string().len(),
        result_json_bytes = parsed.to_string().len(),
        "workflow context measurement; bytes are not model tokens"
    );
    Ok(parsed)
}

/// POST one JSON-RPC message with the MCP content-negotiation headers, forwarding
/// the session id once assigned.
async fn post(
    http: &reqwest::Client,
    url: &str,
    session: Option<&str>,
    body: &Value,
) -> Result<reqwest::Response, String> {
    let mut req = http
        .post(url)
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("mcp-protocol-version", "2024-11-05")
        .json(body);
    if let Some(sid) = session {
        req = req.header("mcp-session-id", sid);
    }
    req.send().await.map_err(|e| format!("POST {url}: {e}"))
}

/// Extract the JSON-RPC `result` from a response that may be bare JSON or an SSE
/// stream (`data: {json}` lines).
async fn extract_result(resp: reqwest::Response) -> Result<Value, String> {
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await.map_err(|e| format!("body: {e}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}"));
    }
    let envelope: Value = if content_type.contains("text/event-stream") {
        let data: String = body
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|l| l.trim())
            .collect();
        serde_json::from_str(&data).map_err(|e| format!("SSE data not JSON ({data}): {e}"))?
    } else {
        serde_json::from_str(&body).map_err(|e| format!("body not JSON ({body}): {e}"))?
    };
    if let Some(err) = envelope.get("error").filter(|e| !e.is_null()) {
        return Err(format!("JSON-RPC error: {err}"));
    }
    Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
}

async fn expect_empty_success(resp: reqwest::Response, context: &str) -> Result<(), String> {
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("body: {e}"))?;
    if !status.is_success() {
        return Err(format!("{context} returned HTTP {status}: {body}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn scene_preservation_checks_authored_data_separately_from_revision() {
        let before =
            serde_json::json!({"objects": [{"name": "Part"}], "scene_state": {"revision": 1}});
        let mut after = before.clone();
        after["scene_state"]["revision"] = serde_json::json!(2);
        super::require_unchanged_scene("render", &before, &after).unwrap();
        after["objects"][0]["name"] = serde_json::json!("Changed");
        assert!(super::require_unchanged_scene("render", &before, &after).is_err());
    }

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn release_catalog_matches_server_catalog() {
        let expected = parse_expected_tools(include_str!("../../../../smoke/expected-tools.txt"))
            .expect("release catalog parses");
        let actual: BTreeSet<String> = printable_server::tools::workflows::TOOLS
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(expected, actual);
    }

    #[test]
    fn exact_catalog_rejects_same_count_with_wrong_name() {
        let expected = BTreeSet::from(["one".to_string(), "two".to_string()]);
        let result = json!({"tools": [{"name": "one"}, {"name": "wrong"}]});
        let error = check_tool_catalog(&result, &expected).expect_err("wrong catalog rejected");
        assert!(error.contains("missing=[\"two\"]"), "{error}");
        assert!(error.contains("unexpected=[\"wrong\"]"), "{error}");
    }

    #[test]
    fn expected_catalog_rejects_duplicates() {
        let error = parse_expected_tools("one\none\n").expect_err("duplicate rejected");
        assert_eq!(error, "expected tool catalog contains a duplicate name");
    }

    #[test]
    fn assembly_smoke_fixture_certifies_its_requested_rotation_clearance() {
        let report = printable_geom::analyze_assembly_stl(
            &smoke_cube_stl(),
            &smoke_cube_stl_at([15.0, 0.0, 0.0]),
            printable_geom::AssemblyOptions {
                required_clearance_mm: Some(4.0),
                motion: Some(printable_geom::LinearMotion {
                    direction: [-1.0, 0.0, 0.0],
                    travel_mm: 10.0,
                    target_clearance_mm: Some(1.0),
                }),
                rotation: Some(printable_geom::RotationalMotion {
                    pivot_mm: [0.0, 0.0, 0.0],
                    axis: [0.0, 0.0, 1.0],
                    angle_degrees: 90.0,
                    target_clearance_mm: Some(SMOKE_ROTATIONAL_CLEARANCE_MM),
                }),
            },
        )
        .expect("analyze smoke assembly");

        let rotation = report.rotation.expect("rotation report");
        assert!(rotation.can_rotate_full_angle, "{rotation:?}");
        assert!(
            rotation
                .minimum_certified_clearance_mm
                .is_some_and(|clearance| clearance >= SMOKE_ROTATIONAL_CLEARANCE_MM),
            "{rotation:?}"
        );
    }

    #[test]
    fn visual_acceptance_rejects_uniform_or_indistinguishable_images() {
        let dark = DecodedPng {
            width: 1,
            height: 2,
            rgb: vec![0, 0, 0, 64, 64, 64],
        };
        let light = DecodedPng {
            width: 1,
            height: 2,
            rgb: vec![128, 128, 128, 255, 255, 255],
        };
        let uniform = DecodedPng {
            width: 1,
            height: 2,
            rgb: vec![32; 6],
        };
        let uniform_colored = DecodedPng {
            width: 1,
            height: 2,
            rgb: vec![200, 50, 20, 200, 50, 20],
        };

        assert!(dark.has_visible_contrast());
        assert!(light.has_visible_contrast());
        assert!(!uniform.has_visible_contrast());
        assert!(!uniform_colored.has_visible_contrast());
        assert!(mean_absolute_rgb_difference(&dark, &light) > 100.0);
        assert_eq!(mean_absolute_rgb_difference(&dark, &dark), 0.0);
        require_visibly_distinct_images("distinct", &[dark, light])
            .expect("different images accepted");

        let repeated = [
            DecodedPng {
                width: 1,
                height: 2,
                rgb: vec![0, 0, 0, 64, 64, 64],
            },
            DecodedPng {
                width: 1,
                height: 2,
                rgb: vec![0, 0, 0, 64, 64, 64],
            },
        ];
        require_visibly_distinct_images("repeated", &repeated)
            .expect_err("repeated views rejected");

        let framed = DecodedPng {
            width: 4,
            height: 4,
            rgb: (0..16)
                .flat_map(|pixel| {
                    if pixel == 5 {
                        [200, 100, 20]
                    } else {
                        [10, 10, 10]
                    }
                })
                .collect(),
        };
        let mut cropped_rgb = framed.rgb.clone();
        cropped_rgb[0..3].copy_from_slice(&[200, 100, 20]);
        let cropped = DecodedPng {
            width: 4,
            height: 4,
            rgb: cropped_rgb,
        };
        assert!(framed.has_uniform_border(1, 0));
        assert!(!cropped.has_uniform_border(1, 0));
    }

    #[test]
    fn articulated_acceptance_requires_static_and_complete_arc_clearance() {
        let certified = json!({
            "report": {
                "static_analysis": {
                    "relation": "separated",
                    "clearance_mm": 0.5
                },
                "rotation": {
                    "can_rotate_full_angle": true,
                    "minimum_certified_clearance_mm": 0.4,
                    "target_clearance_mm": 0.35
                }
            }
        });
        validate_hinge_clearance(&certified).expect("complete arc accepted");

        let mut blocked = certified;
        blocked["report"]["rotation"]["can_rotate_full_angle"] = json!(false);
        validate_hinge_clearance(&blocked).expect_err("blocked arc rejected");
    }

    #[test]
    fn video_probe_requires_exact_decoded_count_and_finite_duration() {
        let metadata = json!({
            "streams": [{"nb_read_frames": "3"}],
            "format": {"duration": "1.000000"}
        });
        assert_eq!(
            parse_video_probe(&metadata).expect("probe accepted"),
            (3, 1.0)
        );

        for invalid in [
            json!({"streams": [{}], "format": {"duration": "1.0"}}),
            json!({"streams": [{"nb_read_frames": "three"}], "format": {"duration": "1.0"}}),
            json!({"streams": [{"nb_read_frames": "3"}], "format": {"duration": "NaN"}}),
            json!({"streams": [{"nb_read_frames": "3"}], "format": {"duration": "0"}}),
        ] {
            parse_video_probe(&invalid).expect_err("invalid probe rejected");
        }
    }

    #[test]
    fn smoke_video_path_cannot_escape_its_workspace() {
        let temporary = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(temporary.path().join(".printable/jobs/id"))
            .expect("job directory");
        std::fs::write(
            temporary.path().join(".printable/jobs/id/video.mp4"),
            b"video",
        )
        .expect("video fixture");

        let resolved =
            resolve_smoke_workspace_file(temporary.path(), ".printable/jobs/id/video.mp4")
                .expect("confined video accepted");
        assert!(resolved.starts_with(
            std::fs::canonicalize(temporary.path()).expect("canonical temporary root")
        ));
        for invalid in [
            "../video.mp4",
            ".printable/jobs/../../video.mp4",
            "/workspace/video.mp4",
        ] {
            resolve_smoke_workspace_file(temporary.path(), invalid)
                .expect_err("escaping video rejected");
        }
    }

    #[tokio::test]
    async fn initialized_notification_rejects_non_success_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback fake");
        let address = listener.local_addr().expect("loopback address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.expect("read request");
            stream
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 18\r\nConnection: close\r\n\r\nhandshake rejected",
                )
                .await
                .expect("write response");
        });

        let response = reqwest::get(format!("http://{address}"))
            .await
            .expect("request fake");
        let error = expect_empty_success(response, "notifications/initialized")
            .await
            .expect_err("non-success notification rejected");
        assert!(error.contains("HTTP 400 Bad Request"), "{error}");
        assert!(error.contains("handshake rejected"), "{error}");
        server.await.expect("fake server task");
    }

    #[tokio::test]
    async fn smoke_runner_propagates_an_unreachable_service() {
        let error = run(
            "http://127.0.0.1:0",
            &BTreeSet::from(["printable_status".to_string()]),
            None,
            concat!(
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef",
                "0123456789abcdef"
            ),
            None,
            None,
            false,
        )
        .await
        .expect_err("unreachable service rejected");
        assert!(error.contains("healthz request"), "{error}");
    }

    #[test]
    fn readiness_contract_rejects_diagnostics_and_accepts_busy_work() {
        validate_readiness(
            200,
            &json!({
                "status": "busy",
                "checks": {
                    "blender": true,
                    "render_worker": true,
                    "workspace": true,
                    "openscad": true,
                    "ffmpeg": true,
                    "durable_recovery": true
                },
                "work": {"queued": true, "running": false},
                "codes": ["work_in_progress"]
            }),
            true,
        )
        .expect("busy is deployable");

        let error = validate_readiness(
            503,
            &json!({
                "status": "blocked",
                "checks": {
                    "blender": false,
                    "render_worker": true,
                    "workspace": true,
                    "openscad": true,
                    "ffmpeg": true,
                    "durable_recovery": true
                },
                "work": {"queued": false, "running": false},
                "codes": ["ffmpeg_unavailable"]
            }),
            false,
        )
        .expect_err("blocked codes must identify the failed check");
        assert!(error.contains("status and body disagree"), "{error}");

        let error = validate_readiness(
            503,
            &json!({
                "status": "blocked",
                "checks": {
                    "blender": false,
                    "render_worker": true,
                    "workspace": true,
                    "openscad": true,
                    "ffmpeg": true,
                    "durable_recovery": true
                },
                "work": {"queued": false, "running": false},
                "codes": ["blender_unavailable"],
                "error": "connection refused at private-host:9876"
            }),
            false,
        )
        .expect_err("diagnostics must stay off the readiness route");
        assert!(error.contains("unexpected shape"), "{error}");
    }

    #[test]
    fn readiness_requires_worker_for_paired_delivery() {
        let blocked = json!({
            "status": "blocked",
            "checks": {"blender": true, "render_worker": false, "workspace": true,
                "openscad": true, "ffmpeg": true, "durable_recovery": true},
            "work": {"queued": false, "running": false},
            "codes": ["render_worker_unavailable"]
        });
        validate_readiness(503, &blocked, false).expect("standalone image has no worker");
        let error = validate_readiness(503, &blocked, true)
            .expect_err("paired delivery requires its worker");
        assert!(error.contains("paired service is not ready"), "{error}");
    }

    #[tokio::test]
    async fn mechanical_smoke_propagates_an_unreachable_service() {
        let error = smoke_mechanical_render_job(
            &reqwest::Client::new(),
            "http://127.0.0.1:0/mcp",
            "session",
            Path::new("/unreachable"),
        )
        .await
        .expect_err("unreachable service rejected");
        assert!(error.contains("POST"), "{error}");
    }
}
