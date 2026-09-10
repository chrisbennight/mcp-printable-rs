//! Locating the `openscad` CLI.
//!
//! Discovery checks `OPENSCAD_BIN` first, then `openscad` on `PATH`, then a
//! short list of well-known Linux and macOS install locations. Windows paths
//! are intentionally absent because the service supports Unix deployment.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Well-known absolute install locations, tried in order after `PATH`. `PATH`
/// itself is handled separately (see [`resolve`]) because it is searched
/// directory-by-directory rather than as a fixed file path.
const ABSOLUTE_CANDIDATES: &[&str] = &[
    "/Applications/OpenSCAD.app/Contents/MacOS/OpenSCAD",
    "/usr/bin/openscad",
    "/usr/local/bin/openscad",
];

/// Stable operator guidance when the CLI cannot be found.
pub const NOT_FOUND_MESSAGE: &str = "OpenSCAD CLI not found. Install OpenSCAD \
(https://openscad.org) or set OPENSCAD_BIN to the executable path. Searched: \
OPENSCAD_BIN, PATH, /Applications (macOS), /usr/[local/]bin (Linux).";

/// Resolve the `openscad` binary from injected inputs, so discovery order is
/// testable without touching the real filesystem or environment.
///
/// - `env_bin`: the `OPENSCAD_BIN` value, if set. Used only when it is usable.
/// - `path_dirs`: the `PATH` directories, searched in order for an `openscad`.
/// - `is_usable`: predicate for "a usable binary exists at this path".
pub fn resolve<F: Fn(&Path) -> bool>(
    env_bin: Option<&str>,
    path_dirs: &[PathBuf],
    is_usable: F,
) -> Option<PathBuf> {
    if let Some(bin) = env_bin {
        let p = Path::new(bin);
        if is_usable(p) {
            return Some(p.to_path_buf());
        }
    }
    // `openscad` on PATH takes precedence over the fixed install locations.
    for dir in path_dirs {
        let p = dir.join("openscad");
        if is_usable(&p) {
            return Some(p);
        }
    }
    for candidate in ABSOLUTE_CANDIDATES {
        let p = Path::new(candidate);
        if is_usable(p) {
            return Some(p.to_path_buf());
        }
    }
    None
}

pub(crate) fn is_executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Locate the `openscad` CLI on this host, or `None` if it is not installed.
pub fn find_openscad() -> Option<PathBuf> {
    let env_bin = std::env::var("OPENSCAD_BIN").ok();
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    resolve(env_bin.as_deref(), &path_dirs, is_executable_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake filesystem: the given paths "exist" as files, nothing else does.
    fn exists<'a>(present: &'a [&'a str]) -> impl Fn(&Path) -> bool + 'a {
        move |p: &Path| present.iter().any(|e| Path::new(e) == p)
    }

    #[test]
    fn env_bin_wins_when_it_is_a_file() {
        let found = resolve(
            Some("/opt/openscad"),
            &[PathBuf::from("/usr/bin")],
            exists(&["/opt/openscad", "/usr/bin/openscad"]),
        );
        assert_eq!(found, Some(PathBuf::from("/opt/openscad")));
    }

    #[test]
    fn env_bin_missing_falls_through_to_path() {
        // OPENSCAD_BIN is set but not a file, so discovery continues to PATH.
        let found = resolve(
            Some("/nope/openscad"),
            &[PathBuf::from("/custom/bin")],
            exists(&["/custom/bin/openscad"]),
        );
        assert_eq!(found, Some(PathBuf::from("/custom/bin/openscad")));
    }

    #[test]
    fn path_openscad_precedes_absolute_candidates() {
        // Both a PATH openscad and /usr/bin/openscad exist; PATH comes first.
        let found = resolve(
            None,
            &[PathBuf::from("/custom/bin")],
            exists(&["/custom/bin/openscad", "/usr/bin/openscad"]),
        );
        assert_eq!(found, Some(PathBuf::from("/custom/bin/openscad")));
    }

    #[test]
    fn absolute_candidates_are_tried_in_order() {
        // Nothing on PATH; /usr/bin precedes /usr/local/bin.
        let found = resolve(
            None,
            &[],
            exists(&["/usr/bin/openscad", "/usr/local/bin/openscad"]),
        );
        assert_eq!(found, Some(PathBuf::from("/usr/bin/openscad")));
    }

    #[test]
    fn macos_app_bundle_is_a_candidate() {
        let app = "/Applications/OpenSCAD.app/Contents/MacOS/OpenSCAD";
        let found = resolve(None, &[], exists(&[app]));
        assert_eq!(found, Some(PathBuf::from(app)));
    }

    #[test]
    fn none_when_nothing_is_found() {
        let found = resolve(None, &[PathBuf::from("/somewhere")], |_| false);
        assert_eq!(found, None);
    }

    #[test]
    fn real_discovery_requires_an_executable_regular_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let candidate = directory.path().join("openscad");
        std::fs::write(&candidate, "#!/bin/sh\n").expect("write candidate");
        let mut permissions = std::fs::metadata(&candidate)
            .expect("candidate metadata")
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&candidate, permissions.clone()).expect("set non-executable");
        assert!(!is_executable_file(&candidate));

        permissions.set_mode(0o700);
        std::fs::set_permissions(&candidate, permissions).expect("set executable");
        assert!(is_executable_file(&candidate));
    }
}
