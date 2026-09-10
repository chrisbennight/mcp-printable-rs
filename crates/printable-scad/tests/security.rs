//! Property-based security tests for the confined-source gate.
//!
//! The gate's contract is direct: accepted untrusted OpenSCAD source must never
//! read an un-snapshotted workspace path. These tests assert that
//! directly, and — for the path-containment property — check it against the
//! *known* paths placed in the source rather than re-parsing with the gate's
//! own lexer, so a parser blind spot cannot hide from both sides.

use printable_scad::{GateError, confine_source, import_surface_paths};
use proptest::prelude::*;

const SNAPSHOT_PREFIX: &str = "/snapshots/";

/// A snapshot that maps every path to a fixed marker containing none of the
/// original path, so a surviving original is detectable by substring.
fn snapshot(_path: &str) -> Result<String, GateError> {
    Ok(format!("{SNAPSHOT_PREFIX}s"))
}

proptest! {
    /// However malformed or adversarial the input, the gate must not panic.
    #[test]
    fn never_panics(code in ".*") {
        let _ = confine_source(&code, snapshot);
    }

    /// Every `import`/`surface` path placed in the source is snapshotted: the
    /// original never survives in accepted output. Paths carry a unique `m<i>_`
    /// prefix, so this check does not depend on the gate's own parser.
    #[test]
    fn no_import_path_survives_unsnapshotted(
        names in prop::collection::vec("[a-z]{1,8}", 1..6),
        keywords in prop::collection::vec(prop_oneof![Just("import"), Just("surface")], 1..6),
        fillers in prop::collection::vec(
            prop_oneof![Just("cube(1);\n"), Just("// note\n"), Just("sphere(r=2);\n")],
            0..4,
        ),
    ) {
        let mut src = String::new();
        let mut originals = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let path = format!("m{i}_{name}.stl");
            let keyword = keywords[i % keywords.len()];
            if keyword == "surface" {
                src.push_str(&format!("surface(file = \"{path}\");\n"));
            } else {
                src.push_str(&format!("import(\"{path}\");\n"));
            }
            originals.push(path);
        }
        for filler in &fillers {
            src.push_str(filler);
        }

        if let Ok(confined) = confine_source(&src, snapshot) {
            for original in &originals {
                prop_assert!(
                    !confined.contains(&format!("\"{original}\"")),
                    "raw path {original:?} survived in confined output: {confined:?}",
                );
            }
            for path in import_surface_paths(&confined).unwrap_or_default() {
                prop_assert!(
                    path.starts_with(SNAPSHOT_PREFIX),
                    "confined output references a non-snapshot path: {path:?}",
                );
            }
        }
    }

    /// A standalone forbidden directive is always refused, whatever follows it.
    #[test]
    fn forbidden_directive_is_always_refused(
        directive in prop_oneof![
            Just("include"), Just("use"), Just("import_dxf"), Just("import_off"),
            Just("import_stl"), Just("dxf_dim"), Just("dxf_cross"),
        ],
        trailer in "[a-z0-9 ]{0,12}",
    ) {
        let code = format!("{directive} {trailer}");
        prop_assert!(matches!(
            confine_source(&code, snapshot),
            Err(GateError::Forbidden(_)),
        ));
    }

    /// A rejected source performs no snapshot side effects. Snapshotting copies
    /// up to the workspace transfer cap into a fresh temp directory, so if any
    /// error is returned the callback must never have run — malformed or
    /// forbidden untrusted input cannot drive snapshot work before the gate
    /// reaches its rejection.
    #[test]
    fn rejected_source_snapshots_nothing(code in ".*") {
        let calls = std::cell::Cell::new(0usize);
        let result = confine_source(&code, |_p| {
            calls.set(calls.get() + 1);
            Ok::<_, GateError>(format!("{SNAPSHOT_PREFIX}s"))
        });
        if result.is_err() {
            prop_assert_eq!(calls.get(), 0);
        }
    }
}
