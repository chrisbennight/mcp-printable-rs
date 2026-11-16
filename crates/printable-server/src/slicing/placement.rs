//! Precise edits to an immutable 3MF snapshot. Unselected ZIP members are copied
//! as compressed bytes; selected XML items retain every unrelated attribute.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::Path,
};

use printable_geom::plate::{Transform3mf, validate_transform};
use quick_xml::{Writer, events::Event, name::ResolveResult, reader::NsReader};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use zip::ZipWriter;

use crate::error::ToolError;

use super::model;

const CORE: &str = "http://schemas.microsoft.com/3dmanufacturing/core/2015/02";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstancePlacement {
    /// Zero-based build index returned by source inspection.
    pub build_index: usize,
    /// Absolute 3MF transform in the source document frame, preserving scale.
    pub transform_3mf: Transform3mf,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TowerPosition {
    /// One-based native plate identity.
    pub plate: u16,
    /// Native bed-local coordinates; document tiling offsets are not added.
    pub position_mm: [f64; 2],
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LayoutControls {
    /// Required source precondition, obtained from source inspection.
    pub source_sha256: String,
    #[serde(default)]
    pub placements: Vec<InstancePlacement>,
    #[serde(default)]
    pub tower_positions: Vec<TowerPosition>,
    /// Source solid gap; final generated support/brim spacing is checked separately.
    pub minimum_gap_mm: Option<f64>,
    /// Deliberate fused/material interfaces, bound to the same authored source.
    #[serde(default)]
    pub intended_interfaces: Vec<super::layout::IntendedInterface>,
}

fn invalid(message: &str) -> ToolError {
    ToolError::Validation(format!("3MF placement: {message}"))
}

impl LayoutControls {
    pub fn validate(&self) -> Result<(), ToolError> {
        super::layout::InspectOptions {
            minimum_gap_mm: self.minimum_gap_mm.unwrap_or(0.),
            intended_interfaces: self.intended_interfaces.clone(),
            ..Default::default()
        }
        .validate()?;
        if self.source_sha256.len() != 64
            || !self.source_sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || self.placements.len() > 512
            || self.tower_positions.len() > 256
            || self
                .minimum_gap_mm
                .is_some_and(|gap| !gap.is_finite() || !(0. ..=1000.).contains(&gap))
        {
            return Err(invalid(
                "require a source SHA-256, bounded selections and a finite nonnegative gap",
            ));
        }
        let mut instances = BTreeSet::new();
        for placement in &self.placements {
            validate_transform(&placement.transform_3mf)?;
            if !instances.insert(placement.build_index)
                || placement.transform_3mf.iter().any(|v| v.abs() > 1e7)
            {
                return Err(invalid(
                    "instance selections must be unique and transform coefficients within the supported range",
                ));
            }
        }
        let mut plates = BTreeSet::new();
        for tower in &self.tower_positions {
            if tower.plate == 0
                || !plates.insert(tower.plate)
                || tower
                    .position_mm
                    .iter()
                    .any(|v| !v.is_finite() || v.abs() > 1e7)
            {
                return Err(invalid(
                    "tower positions require unique positive plates and finite coordinates",
                ));
            }
        }
        Ok(())
    }
}

fn preserves_shape(original: &Transform3mf, changed: &Transform3mf) -> bool {
    let determinant = |m: &Transform3mf| {
        m[0] * (m[4] * m[8] - m[7] * m[5]) - m[3] * (m[1] * m[8] - m[7] * m[2])
            + m[6] * (m[1] * m[5] - m[4] * m[2])
    };
    let old = determinant(original);
    let new = determinant(changed);
    if !old.is_finite() || !new.is_finite() || old.abs() < 1e-12 || old.signum() != new.signum() {
        return false;
    }
    (0..3).all(|i| {
        (0..3).all(|j| {
            let dot = |m: &Transform3mf| (0..3).map(|k| m[i * 3 + k] * m[j * 3 + k]).sum::<f64>();
            let (a, b) = (dot(original), dot(changed));
            (a - b).abs() <= 1e-8 * a.abs().max(b.abs()).max(1.)
        })
    })
}

fn rewrite_model(
    bytes: &[u8],
    placements: &BTreeMap<usize, Transform3mf>,
) -> Result<Vec<u8>, ToolError> {
    let mut reader = NsReader::from_reader(bytes);
    let mut writer = Writer::new(Vec::new());
    let mut stack = Vec::<(bool, Vec<u8>)>::new();
    let mut buffer = Vec::new();
    let mut index = 0usize;
    let mut changed = BTreeSet::new();
    loop {
        printable_geom::limit::check()?;
        let event = reader
            .read_event_into(&mut buffer)
            .map_err(|_| invalid("malformed primary model"))?;
        match event {
            Event::Start(ref start) | Event::Empty(ref start) => {
                let (namespace, local) = reader.resolver().resolve_element(start.name());
                let core = matches!(namespace,ResolveResult::Bound(namespace) if namespace.as_ref()==CORE.as_bytes());
                let is_item = core
                    && local.as_ref() == b"item"
                    && stack.as_slice() == [(true, b"model".to_vec()), (true, b"build".to_vec())];
                let replacement = if is_item {
                    let replacement = placements.get(&index);
                    if replacement.is_some() {
                        changed.insert(index);
                    }
                    index += 1;
                    replacement
                } else {
                    None
                };
                let mut output = start.to_owned();
                if let Some(transform) = replacement {
                    output.clear_attributes();
                    for attribute in start.attributes().with_checks(true) {
                        let attribute =
                            attribute.map_err(|_| invalid("ambiguous model attributes"))?;
                        if attribute.key.as_ref() != b"transform" {
                            output.push_attribute(attribute);
                        }
                    }
                    let transform = transform
                        .iter()
                        .map(f64::to_string)
                        .collect::<Vec<_>>()
                        .join(" ");
                    output.push_attribute(("transform", transform.as_str()));
                }
                if matches!(event, Event::Start(_)) {
                    stack.push((core, local.as_ref().to_vec()));
                    writer.write_event(Event::Start(output))?;
                } else {
                    writer.write_event(Event::Empty(output))?;
                }
            }
            Event::End(ref end) => {
                let (namespace, local) = reader.resolver().resolve_element(end.name());
                let core = matches!(namespace,ResolveResult::Bound(namespace) if namespace.as_ref()==CORE.as_bytes());
                if stack.pop() != Some((core, local.as_ref().to_vec())) {
                    return Err(invalid("unmatched model element"));
                }
                writer.write_event(event)?;
            }
            Event::DocType(_) => return Err(invalid("document type declarations are unsupported")),
            Event::Eof => break,
            _ => writer.write_event(event)?,
        }
        buffer.clear();
    }
    if changed.len() != placements.len() {
        return Err(invalid("selected build instance was not found"));
    }
    Ok(writer.into_inner())
}

/// Called only by the geometry worker after bounded inspection of the same
/// private snapshot. This never writes back to the source path.
pub fn write_snapshot(
    source: &Path,
    destination: &Path,
    archive: &model::SourceArchive,
    controls: &LayoutControls,
) -> Result<(), ToolError> {
    controls.validate()?;
    if source == destination {
        return Err(invalid("source and prepared snapshot must be distinct"));
    }
    if super::hash_file(source)? != controls.source_sha256 {
        return Err(invalid("source changed since layout selection"));
    }
    let build = &archive.documents[&archive.primary_model].build;
    let mut placements = BTreeMap::new();
    for placement in &controls.placements {
        let item = build
            .get(placement.build_index)
            .ok_or_else(|| invalid("unknown build instance"))?;
        if !item.printable || !preserves_shape(&item.component.transform, &placement.transform_3mf)
        {
            return Err(invalid(
                "placement must retain a printable instance's intrinsic scale and handedness",
            ));
        }
        placements.insert(placement.build_index, placement.transform_3mf);
    }
    let mut replacements = BTreeMap::new();
    let mut source_zip = super::package::open(source)?;
    if !placements.is_empty() {
        let mut bytes = Vec::new();
        source_zip
            .by_name(&archive.primary_model)
            .map_err(|_| invalid("primary model missing"))?
            .take(archive.xml_bytes_read + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > archive.xml_bytes_read {
            return Err(invalid("model exceeds its inspected byte budget"));
        }
        replacements.insert(
            archive.primary_model.clone(),
            rewrite_model(&bytes, &placements)?,
        );
    }
    if !controls.tower_positions.is_empty() {
        let name = source_zip
            .file_names()
            .filter_map(Result::ok)
            .find(|name| name.eq_ignore_ascii_case("Metadata/project_settings.config"))
            .ok_or_else(|| invalid("tower placement requires native project settings"))?
            .into_owned();
        let mut bytes = Vec::new();
        source_zip
            .by_name(&name)
            .map_err(|_| invalid("project settings missing"))?
            .take(archive.xml_bytes_read + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > archive.xml_bytes_read {
            return Err(invalid("settings exceed their inspected byte budget"));
        }
        let mut settings: serde_json::Value = serde_json::from_slice(&bytes)?;
        for tower in &controls.tower_positions {
            if !archive
                .native
                .plates
                .iter()
                .any(|plate| plate.plate == Some(u32::from(tower.plate)))
            {
                return Err(invalid(
                    "tower selection has no verified native plate identity",
                ));
            }
            for (key, value) in [
                ("wipe_tower_x", tower.position_mm[0]),
                ("wipe_tower_y", tower.position_mm[1]),
            ] {
                let values = settings[key]
                    .as_array_mut()
                    .ok_or_else(|| invalid("native tower coordinate vectors are unavailable"))?;
                let item = values
                    .get_mut(usize::from(tower.plate) - 1)
                    .ok_or_else(|| invalid("tower plate is outside native coordinate vectors"))?;
                *item = json!(value.to_string());
            }
        }
        replacements.insert(name, serde_json::to_vec(&settings)?);
    }
    let file = File::create_new(destination)?;
    let mut writer = ZipWriter::new(file);
    writer
        .set_raw_comment(source_zip.comment().into())
        .map_err(|_| invalid("cannot preserve archive comment"))?;
    for index in 0..source_zip.len() {
        printable_geom::limit::check()?;
        let entry = source_zip
            .by_index(index)
            .map_err(|_| invalid("archive member cannot be copied"))?;
        let name = entry
            .name()
            .map_err(|_| invalid("archive member name cannot be decoded"))?;
        if let Some(bytes) = replacements.get(name.as_ref()) {
            writer
                .start_file(name, entry.options())
                .map_err(|_| invalid("cannot write modified archive member"))?;
            writer.write_all(bytes)?;
        } else {
            writer
                .raw_copy_file(entry)
                .map_err(|_| invalid("cannot preserve archive member"))?;
        }
    }
    writer
        .finish()
        .map_err(|_| invalid("cannot finish prepared snapshot"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use printable_geom::plate::IDENTITY_3MF;

    fn package() -> tempfile::NamedTempFile {
        let model = format!(
            "<model xmlns='{CORE}' xmlns:p='http://schemas.microsoft.com/3dmanufacturing/production/2015/06' xmlns:custom='urn:authored' unit='millimeter'><metadata name='authored'>keep me</metadata><resources><object id='1'><mesh><vertices><vertex x='0' y='0' z='0'/><vertex x='2' y='0' z='0'/><vertex x='0' y='2' z='0'/></vertices><triangles><triangle v1='0' v2='1' v3='2' pid='4' p1='1' custom:mask='opaque-painted-data'/></triangles></mesh></object></resources><build><item objectid='1' transform='2 0 0 0 1 0 0 0 1 10 20 0' custom:identity='first'/><item objectid='1' transform='1 0 0 0 1 0 0 0 1 100 100 0' custom:identity='second'/></build></model>"
        );
        let relationships=b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='model' Type='http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel' Target='/3D/3dmodel.model'/></Relationships>";
        let native=b"<config><object id='1'><metadata key='extruder' value='2'/><part id='1' subtype='normal_part'><metadata key='support_interface_filament' value='1'/></part></object><plate><metadata key='plater_id' value='1'/><model_instance><metadata key='object_id' value='1'/><metadata key='instance_id' value='0'/></model_instance></plate><plate><metadata key='plater_id' value='2'/><model_instance><metadata key='object_id' value='1'/><metadata key='instance_id' value='1'/></model_instance></plate></config>";
        let settings=b"{\"wipe_tower_x\":[\"10\",\"20\"],\"wipe_tower_y\":[\"30\",\"40\"],\"unrelated_setting\":{\"nested\":[1,2]},\"filament_settings_id\":[\"first material\",\"second material\"]}";
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&super::super::package::fixture::package(&[
            ("_rels/.rels", relationships),
            ("3D/3dmodel.model", model.as_bytes()),
            ("Metadata/model_settings.config", native),
            ("Metadata/project_settings.config", settings),
            ("Metadata/authored-mask.bin", b"opaque authored features"),
        ]))
        .unwrap();
        file
    }

    #[test]
    fn selected_placements_preserve_other_instances_materials_and_unknown_members() {
        let source = package();
        let original = model::read_archive(source.path(), 1024 * 1024, || true).unwrap();
        let mut transform = original.documents[&original.primary_model].build[0]
            .component
            .transform;
        transform[9] += 5.;
        let controls = LayoutControls {
            source_sha256: super::super::hash_file(source.path()).unwrap(),
            placements: vec![InstancePlacement {
                build_index: 0,
                transform_3mf: transform,
            }],
            tower_positions: vec![TowerPosition {
                plate: 2,
                position_mm: [70., 80.],
            }],
            minimum_gap_mm: Some(3.),
            intended_interfaces: Vec::new(),
        };
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("prepared.3mf");
        write_snapshot(source.path(), &destination, &original, &controls).unwrap();
        assert_eq!(
            super::super::hash_file(source.path()).unwrap(),
            controls.source_sha256
        );
        let changed = model::read_archive(&destination, 1024 * 1024, || true).unwrap();
        let build = &changed.documents[&changed.primary_model].build;
        assert_eq!(build[0].component.transform, transform);
        assert_eq!(
            build[1].component.transform,
            original.documents[&original.primary_model].build[1]
                .component
                .transform
        );
        assert_eq!(changed.native.objects[&1].settings["extruder"], "2");
        assert_eq!(
            changed.native.objects[&1].parts[0].settings["support_interface_filament"],
            "1"
        );
        let mut before = super::super::package::open(source.path()).unwrap();
        let mut after = super::super::package::open(&destination).unwrap();
        for name in [
            "_rels/.rels",
            "Metadata/model_settings.config",
            "Metadata/authored-mask.bin",
        ] {
            let mut a = Vec::new();
            let mut b = Vec::new();
            before.by_name(name).unwrap().read_to_end(&mut a).unwrap();
            after.by_name(name).unwrap().read_to_end(&mut b).unwrap();
            assert_eq!(a, b, "{name}");
        }
        let mut bytes = String::new();
        after
            .by_name(&changed.primary_model)
            .unwrap()
            .read_to_string(&mut bytes)
            .unwrap();
        assert!(
            bytes.contains("custom:mask=\"opaque-painted-data\"")
                || bytes.contains("custom:mask='opaque-painted-data'")
        );
        assert!(
            bytes.contains("custom:identity=\"second\"")
                || bytes.contains("custom:identity='second'")
        );
        let mut settings = String::new();
        after
            .by_name("Metadata/project_settings.config")
            .unwrap()
            .read_to_string(&mut settings)
            .unwrap();
        let settings: serde_json::Value = serde_json::from_str(&settings).unwrap();
        assert_eq!(settings["wipe_tower_x"], json!(["10", "70"]));
        assert_eq!(settings["wipe_tower_y"], json!(["30", "80"]));
        assert_eq!(settings["unrelated_setting"], json!({"nested":[1,2]}));
        assert_eq!(
            settings["filament_settings_id"],
            json!(["first material", "second material"])
        );
    }

    #[test]
    fn placement_preserves_existing_nonuniform_scale_and_rejects_design_changes() {
        let original = [2., 0., 0., 0., 1., 0., 0., 0., 3., 10., 20., 0.];
        let rigid = [0., 2., 0., -1., 0., 0., 0., 0., 3., 30., 40., 0.];
        assert!(preserves_shape(&original, &rigid));
        assert!(!preserves_shape(&original, &IDENTITY_3MF));
        let mut mirrored = rigid;
        mirrored[1] = -2.;
        assert!(!preserves_shape(&original, &mirrored));
        let source = package();
        let archive = model::read_archive(source.path(), 1024 * 1024, || true).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("not-created.3mf");
        let controls = LayoutControls {
            source_sha256: "0".repeat(64),
            placements: Vec::new(),
            tower_positions: Vec::new(),
            minimum_gap_mm: None,
            intended_interfaces: Vec::new(),
        };
        assert!(write_snapshot(source.path(), &destination, &archive, &controls).is_err());
        assert!(!destination.exists());
    }
}
