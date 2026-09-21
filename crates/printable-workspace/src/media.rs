/// Artifact suffixes accepted by the workspace, sorted, with leading dots.
/// The set and its order are a stable tool surface because they appear verbatim
/// in the unsupported-type error message.
pub const ALLOWED_ARTIFACT_SUFFIXES: &[&str] = &[
    ".3mf", ".blend", ".bmp", ".dxf", ".glb", ".gltf", ".jpeg", ".jpg", ".json", ".mp4", ".obj",
    ".off", ".ply", ".png", ".py", ".scad", ".step", ".stl", ".stp", ".svg", ".tif", ".tiff",
    ".webp",
];

/// Stable media type per allowed suffix. The explicit table prevents host MIME
/// databases from changing tool output across Linux and Darwin.
pub fn media_type_for(suffix: &str) -> &'static str {
    match suffix {
        ".bmp" => "image/bmp",
        ".dxf" => "image/vnd.dxf",
        ".glb" => "model/gltf-binary",
        ".gltf" => "model/gltf+json",
        ".jpeg" | ".jpg" => "image/jpeg",
        ".json" => "application/json",
        ".mp4" => "video/mp4",
        ".obj" => "application/x-tgif",
        ".png" => "image/png",
        ".py" => "text/x-python",
        ".step" | ".stp" => "model/step",
        ".stl" => "model/stl",
        ".svg" => "image/svg+xml",
        ".tif" | ".tiff" => "image/tiff",
        ".webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// The allowed suffix of `name` (its final extension, case-sensitive), or
/// `None` when the name has no extension or an unsupported one.
pub(crate) fn allowed_suffix(name: &str) -> Option<&'static str> {
    let dot = name.rfind('.')?;
    let suffix = &name[dot..];
    ALLOWED_ARTIFACT_SUFFIXES
        .iter()
        .find(|allowed| **allowed == suffix)
        .copied()
}

pub(crate) fn allowed_suffix_list() -> String {
    ALLOWED_ARTIFACT_SUFFIXES.join(", ")
}
