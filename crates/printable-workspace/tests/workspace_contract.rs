//! Behavioral contract of the confined workspace: stable operator errors,
//! deterministic artifact metadata, and filesystem attack scenarios.
//!
//! Hermetic: every test runs in its own tempdir; no network, no shared state.

use std::path::Path;
use std::sync::{Arc, Barrier};

use printable_workspace::{ALLOWED_ARTIFACT_SUFFIXES, MAX_TRANSFER_BYTES, Workspace, WsError};

fn ws(root: &Path) -> Workspace {
    Workspace::open(Some(root), None).expect("open workspace")
}

fn tmp() -> tempfile::TempDir {
    tempfile::TempDir::new().expect("tempdir")
}

#[test]
fn stat_inspects_large_artifacts_without_transfer_or_snapshot() {
    let dir = tmp();
    let workspace = ws(dir.path());
    let file = std::fs::File::create(dir.path().join("large.mp4")).unwrap();
    file.set_len(MAX_TRANSFER_BYTES + 1).unwrap();
    let meta = workspace.stat_artifact("large.mp4").unwrap();
    assert_eq!(meta.path, "large.mp4");
    assert_eq!(meta.size_bytes, MAX_TRANSFER_BYTES + 1);
    assert_eq!(meta.media_type, "video/mp4");
    file.set_len(7).unwrap();
    assert_eq!(workspace.stat_artifact("large.mp4").unwrap().size_bytes, 7);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn stat_refuses_unsafe_or_missing_artifacts() {
    let dir = tmp();
    let outside = tmp();
    let workspace = ws(dir.path());
    std::fs::write(outside.path().join("model.stl"), b"outside").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("model.stl"),
        dir.path().join("link.stl"),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("directory.stl")).unwrap();
    for (path, code) in [
        ("../model.stl", "path_escapes"),
        ("linked/model.stl", "symlink_refused"),
        ("link.stl", "symlink_refused"),
        ("missing.stl", "not_found"),
        ("directory.stl", "not_regular_file"),
    ] {
        assert_eq!(
            workspace.stat_artifact(path).unwrap_err().code(),
            code,
            "{path}"
        );
    }
    assert!(
        Workspace::open(None, None)
            .unwrap()
            .stat_artifact("model.stl")
            .is_err()
    );
}

// --- stable error-string surface -------------------------------------------

#[test]
fn error_strings_are_stable() {
    let cases: Vec<(WsError, &str)> = vec![
        (
            WsError::Unconfined,
            "workspace tools require PRINTABLE_WORKSPACE_ROOT",
        ),
        (
            WsError::RootNotDirectory("/x".into()),
            "PRINTABLE_WORKSPACE_ROOT is not a directory: /x",
        ),
        (
            WsError::PathEscapes,
            "path escapes PRINTABLE_WORKSPACE_ROOT",
        ),
        (
            WsError::SymlinkRefused,
            "workspace path changed or symbolic link escapes PRINTABLE_WORKSPACE_ROOT",
        ),
        (
            WsError::RefusingSymlinkReplace,
            "refusing to replace a symbolic link",
        ),
        (
            WsError::ReservedPath,
            "path is reserved for Printable internal state",
        ),
        (
            WsError::UnsupportedArtifactType,
            "unsupported artifact type; allowed: .3mf, .blend, .bmp, .dxf, .glb, .gltf, \
             .jpeg, .jpg, .json, .mp4, .obj, .off, .ply, .png, .py, .scad, .step, .stl, .stp, .svg, .tif, .tiff, .webp, .zip",
        ),
        (
            WsError::NotRegularFile,
            "workspace artifact is not a regular file",
        ),
        (
            WsError::AlreadyExists("a.stl".into()),
            "artifact already exists: a.stl",
        ),
        (
            WsError::WriteTooLarge,
            "decoded artifact exceeds 26214400 bytes",
        ),
        (
            WsError::ReadTooLarge,
            "artifact exceeds MCP transfer limit of 26214400 bytes",
        ),
        (
            WsError::NonTransferableArtifact,
            "video artifacts are path-addressable and cannot be read as base64",
        ),
        (
            WsError::SnapshotTooLarge(1073741824),
            "artifact exceeds caller-selected snapshot limit of 1073741824 bytes",
        ),
        (
            WsError::ChangedWhileReading,
            "workspace artifact changed while being read",
        ),
        (WsError::InvalidLimit, "limit must be between 1 and 1000"),
    ];
    for (err, expected) in cases {
        assert_eq!(err.to_string(), expected, "code={}", err.code());
    }
}

// --- round trips ------------------------------------------------------------

#[test]
fn write_read_roundtrip_with_nested_parents() {
    let dir = tmp();
    let ws = ws(dir.path());
    let meta = ws.write_artifact("a/b/c.stl", b"solid x\n", false).unwrap();
    assert_eq!(meta.path, "a/b/c.stl");
    assert_eq!(meta.size_bytes, 8);
    assert_eq!(meta.media_type, "model/stl");
    assert!(meta.modified_ns > 0);

    let (rmeta, bytes) = ws.read_artifact("a/b/c.stl").unwrap();
    assert_eq!(bytes, b"solid x\n");
    assert_eq!(rmeta, meta);
}

#[test]
fn stl_encodings_share_mesh_metadata_across_artifact_operations() {
    let dir = tmp();
    let ws = ws(dir.path());
    let ascii = b"solid triangle\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid triangle\n";
    let mut binary = vec![0_u8; 80];
    binary.extend_from_slice(&1_u32.to_le_bytes());
    for value in [0_f32, 0., 1., 0., 0., 0., 1., 0., 0., 0., 1., 0.] {
        binary.extend_from_slice(&value.to_le_bytes());
    }
    binary.extend_from_slice(&0_u16.to_le_bytes());
    for (name, bytes) in [
        ("ascii.stl", ascii.as_slice()),
        ("binary.stl", binary.as_slice()),
    ] {
        let written = ws.write_artifact(name, bytes, false).unwrap();
        assert_eq!(written.media_type, "model/stl");
        let (read, content) = ws.read_artifact(name).unwrap();
        assert_eq!(read.media_type, "model/stl");
        assert_eq!(content, bytes);
        let snapshot = ws.snapshot_artifact(name).unwrap();
        assert_eq!(snapshot.meta().media_type, "model/stl");
    }
    let listed = ws.list_artifacts(".", 10).unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|entry| entry.media_type == "model/stl"));
}

#[test]
fn dotdot_inside_root_normalizes() {
    let dir = tmp();
    let ws = ws(dir.path());
    let meta = ws.write_artifact("a/../n.stl", b"z", true).unwrap();
    assert_eq!(meta.path, "n.stl");
    assert!(dir.path().join("n.stl").is_file());
}

#[test]
fn reserved_namespace_has_distinct_public_and_internal_mutation_capabilities() {
    let dir = tmp();
    let ws = ws(dir.path());

    for path in [
        ".printable/jobs/job.json",
        "ordinary/../.printable/jobs/job.json",
    ] {
        assert_eq!(
            ws.validate_public_mutation_path(path).unwrap_err().code(),
            "reserved_path"
        );
        assert_eq!(
            ws.write_artifact(path, b"public", true).unwrap_err().code(),
            "reserved_path"
        );
    }
    ws.validate_public_mutation_path("ordinary/job.json")
        .expect("ordinary mutation destination");
    assert!(!dir.path().join(".printable/jobs/job.json").exists());

    let meta = ws
        .write_reserved_artifact(".printable/jobs/job.json", b"internal", false)
        .expect("reserved write");
    assert_eq!(meta.path, ".printable/jobs/job.json");
    assert_eq!(
        ws.read_artifact(".printable/jobs/job.json").unwrap().1,
        b"internal"
    );
    assert_eq!(
        ws.write_reserved_artifact("ordinary/job.json", b"internal", false)
            .unwrap_err()
            .code(),
        "reserved_path"
    );

    let source_dir = tmp();
    let source = source_dir.path().join("video.mp4");
    std::fs::write(&source, b"video").unwrap();
    assert_eq!(
        ws.commit_generated_artifact_bounded(".printable/jobs/video.mp4", &source, false, 1024,)
            .unwrap_err()
            .code(),
        "reserved_path"
    );
    let video = ws
        .commit_reserved_generated_artifact_bounded(
            ".printable/jobs/video.mp4",
            &source,
            false,
            1024,
        )
        .expect("reserved generated artifact commit");
    assert_eq!(video.path, ".printable/jobs/video.mp4");
}

#[test]
fn overwrite_semantics() {
    let dir = tmp();
    let ws = ws(dir.path());
    ws.write_artifact("x.stl", b"one", false).unwrap();
    let err = ws.write_artifact("x.stl", b"two", false).unwrap_err();
    assert_eq!(err.to_string(), "artifact already exists: x.stl");
    ws.write_artifact("x.stl", b"two", true).unwrap();
    assert_eq!(ws.read_artifact("x.stl").unwrap().1, b"two");
}

#[test]
fn media_type_is_exact_for_every_allowed_suffix() {
    let dir = tmp();
    let ws = ws(dir.path());
    // Every allowed suffix's write must return its declared stable media type,
    // not merely a non-empty string.
    let expected: &[(&str, &str)] = &[
        (".3mf", "application/octet-stream"),
        (".blend", "application/octet-stream"),
        (".bmp", "image/bmp"),
        (".dxf", "image/vnd.dxf"),
        (".glb", "model/gltf-binary"),
        (".gltf", "model/gltf+json"),
        (".jpeg", "image/jpeg"),
        (".jpg", "image/jpeg"),
        (".json", "application/json"),
        (".mp4", "video/mp4"),
        (".obj", "application/x-tgif"),
        (".off", "application/octet-stream"),
        (".ply", "application/octet-stream"),
        (".png", "image/png"),
        (".py", "text/x-python"),
        (".scad", "application/octet-stream"),
        (".step", "model/step"),
        (".stl", "model/stl"),
        (".stp", "model/step"),
        (".svg", "image/svg+xml"),
        (".tif", "image/tiff"),
        (".tiff", "image/tiff"),
        (".webp", "image/webp"),
        (".zip", "application/zip"),
    ];
    // Guard: the table under test and this expectation cover the same suffixes.
    let expected_suffixes: Vec<&str> = expected.iter().map(|(s, _)| *s).collect();
    assert_eq!(
        expected_suffixes.as_slice(),
        ALLOWED_ARTIFACT_SUFFIXES,
        "expectation table drifted from ALLOWED_ARTIFACT_SUFFIXES"
    );
    for (suffix, media) in expected {
        let meta = ws
            .write_artifact(&format!("probe{suffix}"), b"x", true)
            .unwrap();
        assert_eq!(meta.media_type, *media, "suffix {suffix}");
    }
}

// --- path validation --------------------------------------------------------

#[test]
fn escapes_are_refused() {
    let dir = tmp();
    let ws = ws(dir.path());
    for path in ["../esc.stl", "/etc/passwd.stl", "a/../../esc.stl", ".."] {
        let err = ws.write_artifact(path, b"x", false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "path escapes PRINTABLE_WORKSPACE_ROOT",
            "path={path}"
        );
    }
    assert!(!dir.path().parent().unwrap().join("esc.stl").exists());
}

#[test]
fn unsupported_suffixes_are_refused() {
    let dir = tmp();
    let ws = ws(dir.path());
    for path in ["x.exe", "plain", "", "x.STL", "x.stl.bak"] {
        let err = ws.write_artifact(path, b"x", false).unwrap_err();
        assert_eq!(err.code(), "unsupported_type", "path={path}");
    }
}

#[test]
fn read_missing_and_dir_targets() {
    let dir = tmp();
    let ws = ws(dir.path());
    let err = ws.read_artifact("nope.stl").unwrap_err();
    assert_eq!(err.code(), "not_found");

    std::fs::create_dir(dir.path().join("dirish.stl")).unwrap();
    let err = ws.read_artifact("dirish.stl").unwrap_err();
    assert_eq!(err.to_string(), "workspace artifact is not a regular file");
}

// --- symlink attacks --------------------------------------------------------

#[test]
fn symlinked_component_is_refused() {
    let dir = tmp();
    let outside = tmp();
    let ws = ws(dir.path());
    std::os::unix::fs::symlink(outside.path(), dir.path().join("lnkdir")).unwrap();

    let werr = ws.write_artifact("lnkdir/z.stl", b"x", false).unwrap_err();
    assert_eq!(
        werr.to_string(),
        "workspace path changed or symbolic link escapes PRINTABLE_WORKSPACE_ROOT"
    );
    let rerr = ws.read_artifact("lnkdir/z.stl").unwrap_err();
    assert_eq!(rerr.code(), "symlink_refused");
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn symlinked_file_is_refused_even_inside_root() {
    let dir = tmp();
    let ws = ws(dir.path());
    ws.write_artifact("real.stl", b"x", false).unwrap();
    std::os::unix::fs::symlink(dir.path().join("real.stl"), dir.path().join("lnk.stl")).unwrap();

    assert_eq!(
        ws.read_artifact("lnk.stl").unwrap_err().code(),
        "symlink_refused"
    );
    let err = ws.write_artifact("lnk.stl", b"y", true).unwrap_err();
    assert_eq!(err.to_string(), "refusing to replace a symbolic link");
    // The symlink target must be untouched.
    assert_eq!(std::fs::read(dir.path().join("real.stl")).unwrap(), b"x");
}

#[test]
fn symlink_pointing_outside_is_never_followed() {
    let dir = tmp();
    let outside = tmp();
    let target = outside.path().join("victim.stl");
    std::fs::write(&target, b"victim").unwrap();
    let ws = ws(dir.path());
    std::os::unix::fs::symlink(&target, dir.path().join("evil.stl")).unwrap();

    assert_eq!(
        ws.read_artifact("evil.stl").unwrap_err().code(),
        "symlink_refused"
    );
    assert_eq!(
        ws.write_artifact("evil.stl", b"pwn", true)
            .unwrap_err()
            .code(),
        "symlink_replace"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"victim");
}

#[test]
fn list_skips_symlinks() {
    let dir = tmp();
    let outside = tmp();
    std::fs::write(outside.path().join("out.stl"), b"x").unwrap();
    let ws = ws(dir.path());
    ws.write_artifact("keep.stl", b"x", false).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("lnkdir")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("out.stl"), dir.path().join("lnk.stl")).unwrap();

    let paths: Vec<String> = ws
        .list_artifacts("", 100)
        .unwrap()
        .into_iter()
        .map(|m| m.path)
        .collect();
    assert_eq!(paths, vec!["keep.stl".to_string()]);
}

// --- size caps ---------------------------------------------------------------

#[test]
fn write_cap_is_enforced_at_boundary() {
    let dir = tmp();
    let ws = ws(dir.path());
    let max = usize::try_from(MAX_TRANSFER_BYTES).unwrap();
    ws.write_artifact("max.stl", &vec![0u8; max], true).unwrap();
    let err = ws
        .write_artifact("over.stl", &vec![0u8; max + 1], true)
        .unwrap_err();
    assert_eq!(err.to_string(), "decoded artifact exceeds 26214400 bytes");
}

#[test]
fn read_cap_is_enforced() {
    let dir = tmp();
    let ws = ws(dir.path());
    let max = usize::try_from(MAX_TRANSFER_BYTES).unwrap();
    std::fs::write(dir.path().join("big.stl"), vec![0u8; max + 1]).unwrap();
    let err = ws.read_artifact("big.stl").unwrap_err();
    assert_eq!(
        err.to_string(),
        "artifact exceeds MCP transfer limit of 26214400 bytes"
    );
}

// --- listing ------------------------------------------------------------------

#[test]
fn list_filters_sorts_and_limits() {
    let dir = tmp();
    let ws = ws(dir.path());
    ws.write_artifact("b.stl", b"x", false).unwrap();
    ws.write_artifact("a/c.stl", b"x", false).unwrap();
    ws.write_artifact("a.png", b"x", false).unwrap();
    std::fs::write(dir.path().join("notallowed.txt"), b"x").unwrap();

    let paths: Vec<String> = ws
        .list_artifacts("", 100)
        .unwrap()
        .into_iter()
        .map(|m| m.path)
        .collect();
    assert_eq!(paths, vec!["a.png", "a/c.stl", "b.stl"]);

    let limited = ws.list_artifacts("", 2).unwrap();
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0].path, "a.png");

    let sub: Vec<String> = ws
        .list_artifacts("a", 10)
        .unwrap()
        .into_iter()
        .map(|m| m.path)
        .collect();
    assert_eq!(sub, vec!["a/c.stl"]);
}

#[test]
fn list_limit_bounds() {
    let dir = tmp();
    let ws = ws(dir.path());
    for bad in [0usize, 1001] {
        let err = ws.list_artifacts("", bad).unwrap_err();
        assert_eq!(
            err.to_string(),
            "limit must be between 1 and 1000",
            "limit={bad}"
        );
    }
    assert!(ws.list_artifacts("", 1).is_ok());
    assert!(ws.list_artifacts("", 1000).is_ok());
}

#[test]
fn list_error_targets() {
    let dir = tmp();
    let ws = ws(dir.path());
    ws.write_artifact("f.stl", b"x", false).unwrap();
    assert_eq!(
        ws.list_artifacts("no-such-dir", 5).unwrap_err().code(),
        "not_found"
    );
    // Listing a file path fails because the walk requires a directory.
    assert_eq!(
        ws.list_artifacts("f.stl", 5).unwrap_err().code(),
        "symlink_refused"
    );
}

// The scan-cap behavior is exercised by a crate-internal unit test in
// `src/workspace.rs`, because the scan_cap parameter is private (the public
// API only exposes the fixed MAX_LIST_SCAN_ENTRIES ceiling).

// --- concurrency ---------------------------------------------------------------

#[test]
fn concurrent_no_overwrite_has_exactly_one_winner() {
    let dir = tmp();
    let root = dir.path().to_path_buf();
    let threads = 8;
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();
    for i in 0..threads {
        let barrier = Arc::clone(&barrier);
        let root = root.clone();
        handles.push(std::thread::spawn(move || {
            let ws = Workspace::open(Some(&root), None).unwrap();
            let payload = format!("writer-{i}");
            barrier.wait();
            ws.write_artifact("race.stl", payload.as_bytes(), false)
                .map(|_| payload)
        }));
    }
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let winners: Vec<&String> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "exactly one writer must win");
    for r in &results {
        if let Err(e) = r {
            assert_eq!(e.code(), "already_exists");
        }
    }
    let on_disk = std::fs::read_to_string(root.join("race.stl")).unwrap();
    assert_eq!(&on_disk, winners[0]);
    // No temp droppings left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".printable-"))
        .collect();
    assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");
}

// --- snapshots -------------------------------------------------------------------

#[test]
fn snapshot_copies_and_detects_mutation() {
    let dir = tmp();
    let ws = ws(dir.path());
    ws.write_artifact("s.stl", b"stable", false).unwrap();

    let snap = ws.snapshot_artifact("s.stl").unwrap();
    assert_eq!(std::fs::read(snap.path()).unwrap(), b"stable");
    assert_eq!(snap.meta().path, "s.stl");
    assert_eq!(snap.meta().size_bytes, 6);
    assert_eq!(snap.meta().media_type, "model/stl");
    // The snapshot lives outside the workspace root.
    assert!(!snap.path().starts_with(dir.path()));

    let mutated = ws.snapshot_with_hook("s.stl", || {
        std::fs::write(dir.path().join("s.stl"), b"mutated!").unwrap();
    });
    assert_eq!(
        mutated.unwrap_err().to_string(),
        "workspace artifact changed while being read"
    );
}

#[test]
fn snapshot_refuses_symlink() {
    let dir = tmp();
    let ws = ws(dir.path());
    ws.write_artifact("real.stl", b"x", false).unwrap();
    std::os::unix::fs::symlink(dir.path().join("real.stl"), dir.path().join("lnk.stl")).unwrap();
    assert_eq!(
        ws.snapshot_artifact("lnk.stl").unwrap_err().code(),
        "symlink_refused"
    );
}

#[test]
fn snapshot_source_verification_detects_replacement_and_in_place_changes() {
    let dir = tmp();
    let ws = ws(dir.path());
    ws.write_artifact("part.stl", b"original", false).unwrap();
    let original = ws.snapshot_artifact("part.stl").unwrap();
    ws.verify_snapshot_source(&original).unwrap();
    ws.write_artifact("part.stl", b"original", true).unwrap();
    assert_eq!(
        ws.verify_snapshot_source(&original).unwrap_err().code(),
        "changed_while_reading"
    );
    let replaced = ws.snapshot_artifact("part.stl").unwrap();
    // Equal length ensures the check does not rely on size alone.
    std::fs::write(dir.path().join("part.stl"), b"modified").unwrap();
    assert_eq!(
        ws.verify_snapshot_source(&replaced).unwrap_err().code(),
        "changed_while_reading"
    );
    assert_eq!(std::fs::read(original.path()).unwrap(), b"original");
}

#[test]
fn snapshot_source_verification_refuses_removed_or_redirected_sources() {
    let dir = tmp();
    let other = tmp();
    let workspace = ws(dir.path());
    workspace
        .write_artifact("part.stl", b"original", false)
        .unwrap();
    let original = workspace.snapshot_artifact("part.stl").unwrap();
    let unrelated = ws(other.path());
    unrelated
        .write_artifact("part.stl", b"original", false)
        .unwrap();
    assert!(unrelated.verify_snapshot_source(&original).is_err());
    std::fs::remove_file(dir.path().join("part.stl")).unwrap();
    assert!(workspace.verify_snapshot_source(&original).is_err());
    std::os::unix::fs::symlink(other.path().join("part.stl"), dir.path().join("part.stl")).unwrap();
    assert_eq!(
        workspace
            .verify_snapshot_source(&original)
            .unwrap_err()
            .code(),
        "symlink_refused"
    );
}

// --- unconfined + construction ----------------------------------------------------

#[test]
fn unconfined_mode_refuses_artifact_ops() {
    let ws = Workspace::open(None, None).unwrap();
    assert!(!ws.confined());
    assert!(!ws.ready());
    let expected = "workspace tools require PRINTABLE_WORKSPACE_ROOT";
    assert_eq!(
        ws.write_artifact("x.stl", b"x", false)
            .unwrap_err()
            .to_string(),
        expected
    );
    assert_eq!(ws.read_artifact("x.stl").unwrap_err().to_string(), expected);
    assert_eq!(ws.list_artifacts("", 5).unwrap_err().to_string(), expected);
    assert_eq!(
        ws.snapshot_artifact("x.stl").unwrap_err().to_string(),
        expected
    );
}

#[test]
fn confined_workspace_is_ready_for_artifacts() {
    let dir = tmp();
    let ws = ws(dir.path());

    assert!(ws.ready());
}

#[test]
fn root_must_be_a_directory() {
    let dir = tmp();
    let file = dir.path().join("f.stl");
    std::fs::write(&file, b"x").unwrap();
    for bad in [file.as_path(), Path::new("/no/such/root/xyz")] {
        let err = Workspace::open(Some(bad), None).unwrap_err();
        assert_eq!(err.code(), "root_not_directory");
        assert!(
            err.to_string()
                .starts_with("PRINTABLE_WORKSPACE_ROOT is not a directory: "),
            "{err}"
        );
    }
}

// --- blender path mapping ------------------------------------------------------------

#[test]
fn blender_path_mapping() {
    let dir = tmp();
    let ws = Workspace::open(Some(dir.path()), Some(Path::new("/Volumes/nas/printable"))).unwrap();
    ws.write_artifact("a/b/c.stl", b"x", false).unwrap();

    assert_eq!(
        ws.to_blender_path("a/b/c.stl", true).unwrap(),
        "/Volumes/nas/printable/a/b/c.stl"
    );
    assert_eq!(
        ws.to_blender_path("nope/x.stl", false).unwrap(),
        "/Volumes/nas/printable/nope/x.stl"
    );
    let err = ws.to_blender_path("nope/x.stl", true).unwrap_err();
    assert_eq!(err.to_string(), "file not found: nope/x.stl");
    assert_eq!(
        ws.blender_authority_root().as_deref(),
        Some("/Volumes/nas/printable")
    );
}

#[test]
fn blender_path_without_blender_root_uses_local_root() {
    let dir = tmp();
    let ws = ws(dir.path());
    ws.write_artifact("c.stl", b"x", false).unwrap();
    let mapped = ws.to_blender_path("c.stl", true).unwrap();
    let canonical_root = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(mapped, canonical_root.join("c.stl").display().to_string());
}

// --- transfer-cap enforcement on the commit and snapshot paths ----------------------
// Public paths must not bypass the transfer cap.

#[test]
fn commit_generated_artifact_enforces_cap_before_reading() {
    let dir = tmp();
    let ws = ws(dir.path());
    let src_dir = tmp();
    let big = src_dir.path().join("big.stl");
    // A source larger than the cap must be rejected by size check, never read
    // wholesale into memory.
    let max = usize::try_from(MAX_TRANSFER_BYTES).unwrap();
    std::fs::write(&big, vec![0u8; max + 1]).unwrap();
    let err = ws
        .commit_generated_artifact("out.stl", &big, false)
        .unwrap_err();
    assert_eq!(err.to_string(), "decoded artifact exceeds 26214400 bytes");
    assert!(!dir.path().join("out.stl").exists());

    // A source at the boundary commits and matches byte-for-byte.
    let ok_src = src_dir.path().join("ok.stl");
    std::fs::write(&ok_src, b"generated stl").unwrap();
    let meta = ws
        .commit_generated_artifact("gen/out.stl", &ok_src, false)
        .unwrap();
    assert_eq!(meta.path, "gen/out.stl");
    assert_eq!(ws.read_artifact("gen/out.stl").unwrap().1, b"generated stl");
}

#[test]
fn bounded_generated_media_can_exceed_the_mcp_transfer_cap_without_entering_memory() {
    let dir = tmp();
    let ws = ws(dir.path());
    let source_dir = tmp();
    let video = source_dir.path().join("video.mp4");
    let video_bytes = MAX_TRANSFER_BYTES + 1;
    std::fs::File::create(&video)
        .unwrap()
        .set_len(video_bytes)
        .unwrap();

    let too_small = ws
        .commit_generated_artifact_bounded("jobs/video.mp4", &video, false, MAX_TRANSFER_BYTES)
        .unwrap_err();
    assert_eq!(too_small.code(), "write_too_large");
    assert!(!dir.path().join("jobs/video.mp4").exists());

    let meta = ws
        .commit_generated_artifact_bounded("jobs/video.mp4", &video, false, video_bytes)
        .expect("caller-budgeted video commit");
    assert_eq!(meta.size_bytes, video_bytes);
    assert_eq!(meta.media_type, "video/mp4");
    assert_eq!(
        ws.read_artifact("jobs/video.mp4").unwrap_err().code(),
        "non_transferable_artifact"
    );
}

#[test]
fn video_artifacts_never_enter_the_base64_read_path_at_any_size() {
    let dir = tmp();
    let ws = ws(dir.path());
    ws.write_artifact("small.mp4", b"small video", false)
        .expect("small video artifact");

    assert_eq!(
        ws.read_artifact("small.mp4").unwrap_err().code(),
        "non_transferable_artifact"
    );
    assert_eq!(
        ws.list_artifacts("", 10).unwrap()[0].media_type,
        "video/mp4"
    );
}

#[test]
fn snapshot_enforces_cap() {
    let dir = tmp();
    let ws = ws(dir.path());
    // Place an over-cap file directly in the workspace (write_artifact can't
    // create one), then snapshot must refuse it rather than copy unbounded.
    let max = usize::try_from(MAX_TRANSFER_BYTES).unwrap();
    std::fs::write(dir.path().join("huge.stl"), vec![0u8; max + 1]).unwrap();
    let err = ws.snapshot_artifact("huge.stl").unwrap_err();
    assert_eq!(err.code(), "read_too_large");
}

#[test]
fn caller_bounded_snapshot_streams_server_internal_inputs_above_the_mcp_cap() {
    let dir = tmp();
    let ws = ws(dir.path());
    let bytes = MAX_TRANSFER_BYTES + 1;
    std::fs::File::create(dir.path().join("scene.blend"))
        .unwrap()
        .set_len(bytes)
        .unwrap();

    let snapshot = ws
        .snapshot_artifact_bounded("scene.blend", bytes)
        .expect("large internal snapshot");
    assert_eq!(std::fs::metadata(snapshot.path()).unwrap().len(), bytes);
    assert_eq!(
        ws.snapshot_artifact_bounded("scene.blend", MAX_TRANSFER_BYTES)
            .unwrap_err()
            .code(),
        "read_too_large"
    );
}

// --- resolve / to_blender_path refuse a final symlink -------------------------------

#[test]
fn resolve_refuses_final_symlink_when_existence_required() {
    let dir = tmp();
    let ws = ws(dir.path());
    ws.write_artifact("real.stl", b"x", false).unwrap();
    std::os::unix::fs::symlink(dir.path().join("real.stl"), dir.path().join("lnk.stl")).unwrap();

    // A following path must never be handed out for subprocess/Blender use.
    assert_eq!(
        ws.resolve("lnk.stl", true).unwrap_err().code(),
        "symlink_refused"
    );
    assert_eq!(
        ws.to_blender_path("lnk.stl", true).unwrap_err().code(),
        "symlink_refused"
    );
    // A real file resolves fine.
    let canonical_root = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(
        ws.resolve("real.stl", true).unwrap(),
        canonical_root.join("real.stl")
    );
    // Without an existence requirement there is no filesystem check.
    assert!(ws.resolve("lnk.stl", false).is_ok());
}

// --- non-regular files (FIFO) must not block the open ------------------------------

#[test]
fn reading_a_fifo_does_not_block_and_is_rejected() {
    use std::ffi::CString;
    let dir = tmp();
    let ws = ws(dir.path());
    let fifo = dir.path().join("pipe.stl");
    let cpath = CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // Create a FIFO with an allowed suffix and no writer attached.
    let rc = unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo failed");

    // Must return NotRegularFile promptly, not block waiting for a writer.
    let start = std::time::Instant::now();
    assert_eq!(
        ws.stat_artifact("pipe.stl").unwrap_err().code(),
        "not_regular_file"
    );
    assert_eq!(
        ws.read_artifact("pipe.stl").unwrap_err().code(),
        "not_regular_file"
    );
    assert_eq!(
        ws.snapshot_artifact("pipe.stl").unwrap_err().code(),
        "not_regular_file"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "open must not block on the FIFO"
    );
}

// --- property: no operation ever escapes the root -----------------------------------

mod escape_property {
    use super::*;
    use proptest::prelude::*;

    fn snapshot_outside(base: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(base)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn writes_never_escape_the_root(path in "[a-zA-Z0-9_./-]{0,48}") {
            // Layout: base/{root,canary.stl}. Any escape from root would have
            // to land in base (or above); asserting base's entry set is
            // unchanged catches `../`-style breakouts.
            let base = tempfile::TempDir::new().unwrap();
            std::fs::create_dir(base.path().join("root")).unwrap();
            std::fs::write(base.path().join("canary.stl"), b"canary").unwrap();
            let before = snapshot_outside(base.path());

            let ws = Workspace::open(Some(&base.path().join("root")), None).unwrap();
            if let Ok(meta) = ws.write_artifact(&path, b"probe", true) {
                // A successful write must exist under the root at its
                // normalized relative path.
                prop_assert!(base.path().join("root").join(&meta.path).is_file());
            }
            let _ = ws.read_artifact(&path);
            let _ = ws.list_artifacts(&path, 5);

            prop_assert_eq!(snapshot_outside(base.path()), before);
            prop_assert_eq!(
                std::fs::read(base.path().join("canary.stl")).unwrap(),
                b"canary".to_vec()
            );
        }
    }
}
