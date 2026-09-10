//! OpenSCAD subprocess backend.
//!
//! OpenSCAD source is untrusted input: it is never interpolated into a shell
//! command (argv arrays only), and in confined mode a lexer-based source gate
//! forbids hidden file reads (`include`/`use`/`import_*`) and rewrites literal
//! `import()`/`surface()` paths to snapshotted workspace copies. Subprocess
//! fan-out is capped by a semaphore.
//!
//! The runner owns bounded diagnostics, caller-selected work budgets, process
//! cleanup, and a concurrency permit that can cover staging through artifact
//! commit. Callers prepare confined source and argv before invoking it.

pub mod args;
pub mod camera;
pub mod defines;
pub mod discovery;
pub mod gate;
pub mod product;
pub mod runner;

pub use args::{compile_args, cross_section_source, render_args};
pub use camera::{VIEW_CAMERAS, camera};
pub use defines::{
    DefineError, DefineValue, MAX_DEFINITIONS, MAX_SERIALIZED_BYTES, MAX_STRING_BYTES,
    MAX_VARIANT_CHARS, MAX_VECTOR_ELEMENTS, SerializedDefinitions, serialize_definitions,
    serialize_product_definitions,
};
pub use discovery::{NOT_FOUND_MESSAGE, find_openscad, resolve};
pub use gate::{GateError, Token, confine_source, import_surface_paths, tokenize};
pub use product::{
    FormProfile, ManufacturingProfile, PRODUCT_V1_CALLER_FILE, PRODUCT_V1_KIT_FILE,
    PRODUCT_V1_SOURCE, ProductProfile, ProductProfileError, product_v1_wrapper,
    validate_product_v1_caller,
};
pub use runner::{RunOutput, ScadError, ScadPermit, ScadRunner};
