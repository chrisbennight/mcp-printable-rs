//! FLAME head models discovered in an operator-mounted directory.
//!
//! `PRINTABLE_FLAME_MODEL_DIR` names a directory of files written by
//! `scripts/flame_model_convert.py`. Each valid model is loaded once at startup
//! and keyed by its variant label. The `head` workflow tool is advertised only
//! when at least one model loads. A file that cannot be used is skipped and
//! reported through `status` rather than stopping a server whose other tools
//! work.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use printable_flame::FlameModel;
use serde_json::{Value, json};

/// Converted models are about 27 MB; a far larger file is not one.
const MAX_MODEL_BYTES: u64 = 256 << 20;
const EXTENSION: &str = "safetensors";

/// SHA-256 of `generic_model.pkl` from the FLAME 2020 release, the model
/// that MICA, SMIRK and DECA were trained with.
pub const FLAME_2020_GENERIC_SHA256: &str =
    "efcd14cc4a69f3a3d9af8ded80146b5b6b50df3bd74cf69108213b144eba725b";
/// Reconstruction gives MICA's 300 identity coefficients and SMIRK's first
/// 50 expression coefficients, so the expression basis must start right after
/// the 300 identity components.
const RECONSTRUCTION_IDENTITY_COMPONENTS: usize = 300;
const RECONSTRUCTION_EXPRESSION_COMPONENTS: usize = 50;

/// Models available to the `head` tool, keyed by variant.
pub struct FlameLibrary {
    models: BTreeMap<String, Arc<FlameModel>>,
    skipped: Vec<Skipped>,
}

struct Skipped {
    file: String,
    reason: String,
}

impl fmt::Debug for FlameLibrary {
    // Model tensors are megabytes of floats; describe the library instead.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FlameLibrary")
            .field("variants", &self.models.keys().collect::<Vec<_>>())
            .field("skipped", &self.skipped.len())
            .finish()
    }
}

/// Load the models named by `PRINTABLE_FLAME_MODEL_DIR`, if it is set.
pub fn configure(get: &impl Fn(&str) -> Option<String>) -> Option<Arc<FlameLibrary>> {
    let directory = get("PRINTABLE_FLAME_MODEL_DIR").filter(|value| !value.trim().is_empty())?;
    Some(Arc::new(FlameLibrary::load(Path::new(&directory))))
}

impl FlameLibrary {
    /// Load every converted model directly inside `directory`, in file-name order.
    pub fn load(directory: &Path) -> Self {
        let mut library = Self {
            models: BTreeMap::new(),
            skipped: Vec::new(),
        };
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) => {
                library.skip(
                    directory.display().to_string(),
                    format!("model directory is not readable: {error}"),
                );
                return library;
            }
        };
        let mut paths: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == EXTENSION)
            })
            .collect();
        paths.sort();
        for path in paths {
            let file = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let outcome = read_model(&path).and_then(|model| {
                match library.models.entry(model.info().variant.clone()) {
                    Entry::Vacant(slot) => {
                        slot.insert(Arc::new(model));
                        Ok(())
                    }
                    Entry::Occupied(slot) => Err(format!(
                        "variant {} is already provided by another file",
                        slot.key()
                    )),
                }
            });
            if let Err(reason) = outcome {
                library.skip(file, reason);
            }
        }
        library
    }

    /// Whether any model loaded, which decides whether `head` is advertised.
    pub fn is_available(&self) -> bool {
        !self.models.is_empty()
    }

    /// The mounted model that photo reconstruction's coefficients belong to:
    /// FLAME 2020's generic model, recognised by the digest of its published
    /// file, converted with its own split of identity and expression.
    pub fn reconstruction_variant(&self) -> Option<&str> {
        self.models
            .iter()
            .find(|(_, model)| {
                let info = model.info();
                info.source_sha256 == FLAME_2020_GENERIC_SHA256
                    && info.identity_components == RECONSTRUCTION_IDENTITY_COMPONENTS
                    && info.expression_components >= RECONSTRUCTION_EXPRESSION_COMPONENTS
                    && info.expression_source == "flame"
            })
            .map(|(variant, _)| variant.as_str())
    }

    /// Choose a model by variant. The variant may be omitted when exactly one
    /// model is mounted.
    pub fn select(&self, variant: Option<&str>) -> Result<&Arc<FlameModel>, String> {
        match variant {
            Some(variant) => self.models.get(variant).ok_or_else(|| {
                format!(
                    "unknown head model variant {variant}; available: {}",
                    self.variants()
                )
            }),
            None if self.models.len() == 1 => Ok(self.models.values().next().expect("one model")),
            None => Err(format!(
                "choose a head model variant; available: {}",
                self.variants()
            )),
        }
    }

    /// Mounted models and skipped files, for the `models` action and `status`.
    pub fn report(&self) -> Value {
        let models: Vec<_> = self
            .models
            .values()
            .map(|model| {
                let info = model.info();
                json!({
                    "variant": info.variant,
                    "identity_components": info.identity_components,
                    "expression_components": info.expression_components,
                    "expression_source": info.expression_source,
                    "vertices": model.vertex_count(),
                    "triangles": model.faces().len(),
                    "texture_layout": model.uv_layout().is_some(),
                    "eyelids": model.has_eyelids(),
                    "regions": model.regions().keys().collect::<Vec<_>>(),
                    "source_sha256": info.source_sha256,
                })
            })
            .collect();
        let skipped: Vec<_> = self
            .skipped
            .iter()
            .map(|skipped| json!({"file": skipped.file, "reason": skipped.reason}))
            .collect();
        json!({"models": models, "skipped": skipped})
    }

    fn variants(&self) -> String {
        self.models.keys().cloned().collect::<Vec<_>>().join(", ")
    }

    fn skip(&mut self, file: String, reason: String) {
        tracing::warn!(file = %file, reason = %reason, "skipping FLAME model");
        self.skipped.push(Skipped { file, reason });
    }
}

fn read_model(path: &Path) -> Result<FlameModel, String> {
    use std::os::unix::fs::OpenOptionsExt;
    // Non-blocking open so a named pipe is rejected by the type check below
    // instead of waiting for a writer; regular files ignore the flag.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("not readable: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("not readable: {error}"))?;
    if !metadata.is_file() {
        return Err("not a regular file".into());
    }
    // Read through a bound so a file that grows after the check cannot exceed it.
    let mut bytes = Vec::new();
    file.take(MAX_MODEL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("not readable: {error}"))?;
    if bytes.len() as u64 > MAX_MODEL_BYTES {
        return Err(format!("larger than {} MiB", MAX_MODEL_BYTES >> 20));
    }
    FlameModel::from_safetensors(&bytes).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use printable_flame::fixture::Fixture;

    fn model_bytes(variant: &str) -> Vec<u8> {
        let mut fixture = Fixture::default();
        fixture.metadata.insert("variant".into(), variant.into());
        fixture.bytes()
    }

    #[test]
    fn unset_directory_configures_nothing() {
        assert!(configure(&|_| None).is_none());
        assert!(configure(&|_| Some("  ".into())).is_none());
    }

    #[test]
    fn valid_models_load_by_variant_and_others_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a.safetensors"), model_bytes("alpha")).unwrap();
        std::fs::write(directory.path().join("b.safetensors"), model_bytes("alpha")).unwrap();
        std::fs::write(directory.path().join("c.safetensors"), b"not a model").unwrap();
        std::fs::write(directory.path().join("notes.txt"), b"ignored").unwrap();
        let library = FlameLibrary::load(directory.path());
        assert!(library.is_available());
        let report = library.report();
        assert_eq!(report["models"][0]["variant"], "alpha");
        assert_eq!(report["models"][0]["texture_layout"], false);
        assert_eq!(report["models"][0]["eyelids"], false);
        assert_eq!(report["models"][0]["regions"], json!([]));
        assert_eq!(report["models"].as_array().unwrap().len(), 1);
        let skipped: Vec<_> = report["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["file"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(skipped, ["b.safetensors", "c.safetensors"]);
    }

    #[test]
    fn a_named_pipe_is_skipped_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a.safetensors"), model_bytes("alpha")).unwrap();
        let pipe = std::ffi::CString::new(
            directory
                .path()
                .join("b.safetensors")
                .into_os_string()
                .into_encoded_bytes(),
        )
        .unwrap();
        // SAFETY: `pipe` is a valid NUL-terminated path for the duration of the call.
        assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);
        let library = FlameLibrary::load(directory.path());
        assert!(library.is_available());
        let report = library.report();
        assert_eq!(report["skipped"][0]["file"], "b.safetensors");
        assert_eq!(report["skipped"][0]["reason"], "not a regular file");
    }

    #[test]
    fn a_missing_directory_leaves_the_tool_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let library = FlameLibrary::load(&directory.path().join("absent"));
        assert!(!library.is_available());
        assert_eq!(library.report()["skipped"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn reconstruction_needs_flame_2020_with_its_own_basis_split() {
        let model = |identity: usize, expression: usize, digest: &str| {
            let mut fixture = Fixture {
                shapedirs: vec![0.0; 4 * 3 * (identity + expression)],
                ..Default::default()
            };
            for (key, value) in [
                (
                    "variant",
                    format!("v{identity}_{expression}_{}", &digest[..4]),
                ),
                ("identity_components", identity.to_string()),
                ("expression_components", expression.to_string()),
                ("source_sha256", digest.to_owned()),
            ] {
                fixture.metadata.insert(key.into(), value);
            }
            fixture.bytes()
        };
        let other = "1".repeat(64);
        for (identity, expression, digest, recognised) in [
            (300, 100, FLAME_2020_GENERIC_SHA256, true),
            (300, 50, FLAME_2020_GENERIC_SHA256, true),
            (350, 50, FLAME_2020_GENERIC_SHA256, false),
            (300, 20, FLAME_2020_GENERIC_SHA256, false),
            (300, 100, other.as_str(), false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("m.safetensors"),
                model(identity, expression, digest),
            )
            .unwrap();
            let library = FlameLibrary::load(directory.path());
            assert!(library.is_available(), "{}", library.report());
            assert_eq!(
                library.reconstruction_variant().is_some(),
                recognised,
                "{identity} + {expression}"
            );
        }
    }

    #[test]
    fn variant_selection_requires_a_choice_only_when_ambiguous() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a.safetensors"), model_bytes("alpha")).unwrap();
        let single = FlameLibrary::load(directory.path());
        assert_eq!(single.select(None).unwrap().info().variant, "alpha");
        std::fs::write(directory.path().join("b.safetensors"), model_bytes("beta")).unwrap();
        let several = FlameLibrary::load(directory.path());
        assert!(several.select(None).unwrap_err().contains("alpha, beta"));
        assert_eq!(several.select(Some("beta")).unwrap().info().variant, "beta");
        assert!(
            several
                .select(Some("gamma"))
                .unwrap_err()
                .contains("unknown")
        );
    }
}
