//! Bounded reads from sliced 3MF packages: plate thumbnails and the project
//! settings that record printer, process and filament choices.

use std::{fs::File, io::Read, path::Path};

use serde::Serialize;
use serde_json::Value;
use zip::ZipArchive;

use crate::error::ToolError;

const MAX_ENTRIES: usize = 100_000;
const MAX_SETTINGS_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FILAMENTS: usize = 64;
pub const MAX_THUMBNAIL_BYTES: u64 = 16 * 1024 * 1024;
const SETTINGS_ENTRY: &str = "Metadata/project_settings.config";

/// Facts read from a sliced package without trusting its sizes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PackageFacts {
    /// Plates that have sliced toolpaths (`Metadata/plate_N.gcode`).
    pub plates: Vec<u32>,
    /// Plates that have a full-size thumbnail (`Metadata/plate_N.png`).
    pub thumbnails: Vec<u32>,
    pub printer_profile: Option<String>,
    pub process_profile: Option<String>,
    pub filaments: Vec<PackageFilament>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct PackageFilament {
    pub profile: Option<String>,
    pub filament_type: Option<String>,
    /// `#RRGGBB` or `#RRGGBBAA`; other values are not reported.
    pub colour: Option<String>,
}

fn invalid(message: &str) -> ToolError {
    ToolError::Validation(format!("sliced 3MF package {message}"))
}

fn open(path: &Path) -> Result<ZipArchive<File>, ToolError> {
    let archive = ZipArchive::new(File::open(path)?)
        .map_err(|error| invalid(&format!("cannot be read: {error}")))?;
    if archive.len() > MAX_ENTRIES {
        return Err(invalid("has too many entries"));
    }
    Ok(archive)
}

fn plate_number(name: &str, suffix: &str) -> Option<u32> {
    name.strip_prefix("Metadata/plate_")?
        .strip_suffix(suffix)?
        .parse()
        .ok()
        .filter(|plate| *plate > 0)
}

/// Read one entry, refusing both a declared and an actual size above `max`.
fn read_entry(archive: &mut ZipArchive<File>, name: &str, max: u64) -> Result<Vec<u8>, ToolError> {
    let mut entry = archive
        .by_name(name)
        .map_err(|error| invalid(&format!("entry {name} cannot be read: {error}")))?;
    if entry.size() > max {
        return Err(invalid(&format!("entry {name} exceeds {max} bytes")));
    }
    let mut bytes = Vec::new();
    (&mut entry).take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(invalid(&format!("entry {name} exceeds {max} bytes")));
    }
    Ok(bytes)
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
        .map(str::to_owned)
}

fn colour(value: &Value) -> Option<String> {
    let text = value.as_str()?.trim();
    let hex = text.strip_prefix('#')?;
    (matches!(hex.len(), 6 | 8) && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| text.to_ascii_uppercase())
}

fn list(settings: &Value, key: &str) -> Vec<Value> {
    match &settings[key] {
        Value::Array(values) => values.iter().take(MAX_FILAMENTS).cloned().collect(),
        Value::String(_) => vec![settings[key].clone()],
        _ => Vec::new(),
    }
}

pub fn facts(path: &Path) -> Result<PackageFacts, ToolError> {
    let mut archive = open(path)?;
    let mut facts = PackageFacts::default();
    for name in archive.file_names().filter_map(Result::ok) {
        if let Some(plate) = plate_number(&name, ".gcode") {
            facts.plates.push(plate);
        }
        if let Some(plate) = plate_number(&name, ".png") {
            facts.thumbnails.push(plate);
        }
    }
    for plates in [&mut facts.plates, &mut facts.thumbnails] {
        plates.sort_unstable();
        plates.dedup();
    }
    if archive.index_for_name(SETTINGS_ENTRY).is_some() {
        let bytes = read_entry(&mut archive, SETTINGS_ENTRY, MAX_SETTINGS_BYTES)?;
        let settings: Value = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("has unreadable project settings"))?;
        facts.printer_profile = text(&settings["printer_settings_id"]);
        facts.process_profile = text(&settings["print_settings_id"]);
        let profiles = list(&settings, "filament_settings_id");
        let types = list(&settings, "filament_type");
        let colours = list(&settings, "filament_colour");
        let count = profiles.len().max(types.len()).max(colours.len());
        facts.filaments = (0..count)
            .map(|index| PackageFilament {
                profile: profiles.get(index).and_then(text),
                filament_type: types.get(index).and_then(text),
                colour: colours.get(index).and_then(colour),
            })
            .collect();
    }
    Ok(facts)
}

/// The full-size thumbnail of one plate, as stored PNG bytes.
pub fn plate_thumbnail(path: &Path, plate: u32) -> Result<Vec<u8>, ToolError> {
    let mut archive = open(path)?;
    let name = format!("Metadata/plate_{plate}.png");
    if plate == 0 || archive.index_for_name(&name).is_none() {
        return Err(invalid(&format!("has no thumbnail for plate {plate}")));
    }
    read_entry(&mut archive, &name, MAX_THUMBNAIL_BYTES)
}

#[cfg(test)]
pub(crate) mod fixture {
    use std::io::Write;

    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    /// Write a small sliced-package fixture with deflate-compressed entries.
    pub fn package(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    pub fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbImage::from_pixel(width, height, image::Rgb([200, 120, 40]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &tempfile::TempDir, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join("model.gcode.3mf");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn facts_read_plates_thumbnails_profiles_and_colours() {
        let dir = tempfile::tempdir().unwrap();
        let settings = json!({
            "printer_settings_id": "Bambu Lab X1 Carbon 0.4 nozzle",
            "print_settings_id": "0.20mm Standard @BBL X1C",
            "filament_settings_id": ["Bambu PLA Basic @BBL X1C", "Bambu PETG HF @BBL X1C"],
            "filament_type": ["PLA", "PETG"],
            "filament_colour": ["#ff8800", "not a colour"],
        })
        .to_string();
        let thumbnail = fixture::png(4, 3);
        let path = write(
            &dir,
            &fixture::package(&[
                ("Metadata/plate_2.gcode", b"G28"),
                ("Metadata/plate_1.gcode", b"G28"),
                ("Metadata/plate_1.png", &thumbnail),
                ("Metadata/plate_1_small.png", &thumbnail),
                ("Metadata/plate_0.gcode", b"G28"),
                (SETTINGS_ENTRY, settings.as_bytes()),
            ]),
        );
        let facts = facts(&path).unwrap();
        assert_eq!(facts.plates, [1, 2]);
        assert_eq!(facts.thumbnails, [1]);
        assert_eq!(
            facts.printer_profile.as_deref(),
            Some("Bambu Lab X1 Carbon 0.4 nozzle")
        );
        assert_eq!(facts.filaments.len(), 2);
        assert_eq!(facts.filaments[0].colour.as_deref(), Some("#FF8800"));
        assert_eq!(facts.filaments[1].colour, None);
        assert_eq!(facts.filaments[1].filament_type.as_deref(), Some("PETG"));
        assert_eq!(plate_thumbnail(&path, 1).unwrap(), thumbnail);
        assert!(plate_thumbnail(&path, 2).is_err());
        assert!(plate_thumbnail(&path, 0).is_err());
    }

    #[test]
    fn packages_without_settings_and_malformed_packages_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            &fixture::package(&[("Metadata/plate_1.gcode", b"G28")]),
        );
        let facts = facts(&path).unwrap();
        assert_eq!(facts.plates, [1]);
        assert!(facts.filaments.is_empty());
        let path = write(&dir, b"not a zip");
        assert!(super::facts(&path).is_err());
        let path = write(&dir, &fixture::package(&[(SETTINGS_ENTRY, b"{")]));
        assert!(super::facts(&path).is_err());
    }

    #[test]
    fn oversized_entries_are_refused_without_reading_them_whole() {
        let dir = tempfile::tempdir().unwrap();
        let large = vec![0u8; usize::try_from(MAX_THUMBNAIL_BYTES).unwrap() + 1];
        let path = write(&dir, &fixture::package(&[("Metadata/plate_1.png", &large)]));
        let error = plate_thumbnail(&path, 1).unwrap_err().to_string();
        assert!(error.contains("exceeds"), "{error}");
    }
}
