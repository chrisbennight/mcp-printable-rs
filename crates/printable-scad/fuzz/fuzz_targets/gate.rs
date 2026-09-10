#![no_main]
//! Fuzz the confined-source gate for **robustness**: arbitrary OpenSCAD source
//! must never panic the tokenizer or the span-splicing rewrite, however
//! malformed or adversarial (the `replace_range` arithmetic on lexer byte
//! offsets is the main risk surface).
//!
//! The confinement invariant — no un-snapshotted path survives in accepted
//! output — is property-tested independently in `tests/security.rs`, which
//! checks against the *known* paths it injects rather than re-parsing with this
//! crate's own lexer. That independence is the point, so this target focuses on
//! the no-panic contract over truly arbitrary input.

use libfuzzer_sys::fuzz_target;
use printable_scad::{GateError, confine_source};

fuzz_target!(|code: &str| {
    let _ = confine_source(code, |path| Ok::<_, GateError>(format!("/snapshots/{}", path.len())));
});
