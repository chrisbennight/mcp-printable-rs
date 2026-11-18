//! Allowlisted native identities and settings. Unknown metadata stays in the
//! source archive; it is never flattened into a replacement project.

use super::*;
use serde_json::Value;

const LOCAL_SETTINGS: &[&str] = &[
    "extruder",
    "support_filament",
    "support_interface_filament",
    "layer_height",
    "wall_loops",
    "sparse_infill_density",
    "brim_width",
    "enable_support",
    "support_type",
    "top_shell_layers",
    "bottom_shell_layers",
    "flush_into_infill",
    "flush_into_objects",
    "ironing_type",
    "seam_position",
    "part_type",
    "volume_type",
];
const PROJECT_SETTINGS: &[&str] = &[
    "printer_settings_id",
    "printer_model",
    "print_settings_id",
    "filament_settings_id",
    "filament_type",
    "filament_colour",
    "nozzle_diameter",
    "printable_area",
    "bed_exclude_area",
    "print_sequence",
    "enable_prime_tower",
    "prime_tower_width",
    "prime_tower_brim_width",
    "prime_tower_enable_framework",
    "wipe_tower_no_sparse_layers",
    "wipe_tower_sparse_layers_combination",
    "wipe_tower_x",
    "wipe_tower_y",
    "wipe_tower_rotation_angle",
    "wipe_tower_cone_angle",
    "wipe_tower_type",
    "wipe_tower_wall_type",
    "support_filament",
    "support_interface_filament",
];

#[derive(Debug, Default, Serialize)]
pub struct NativeMetadata {
    pub objects: BTreeMap<u32, NativeObject>,
    pub plates: Vec<NativePlate>,
    pub project_settings: BTreeMap<String, Value>,
    pub model_settings_sha256: Option<String>,
    pub project_settings_sha256: Option<String>,
    pub unreported_metadata_preserved_in_source: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct NativeObject {
    pub settings: BTreeMap<String, String>,
    pub parts: Vec<NativePart>,
}

#[derive(Debug, Default, Serialize)]
pub struct NativePart {
    pub id: Option<u32>,
    pub subtype: Option<String>,
    pub triangle_range: Option<[u32; 2]>,
    pub settings: BTreeMap<String, String>,
}

#[derive(Debug, Default, Serialize)]
pub struct NativePlate {
    /// One-based native plater_id; no tiled-document origin is inferred.
    pub plate: Option<u32>,
    pub locked: Option<bool>,
    pub instances: Vec<NativeInstance>,
}

#[derive(Debug, Default, Serialize)]
pub struct NativeInstance {
    pub object_id: Option<u32>,
    /// Zero-based copy index within the referenced object.
    pub instance_id: Option<u32>,
    pub identify_id: Option<u64>,
}

pub(super) fn read(
    archive: &mut ZipArchive<File>,
    names: &BTreeMap<String, String>,
    remaining: &mut u64,
    continue_work: &mut impl FnMut() -> bool,
) -> Result<NativeMetadata, ToolError> {
    let mut native = NativeMetadata {
        unreported_metadata_preserved_in_source: true,
        ..Default::default()
    };
    if let Some(name) = names.get("metadata/model_settings.config") {
        let bytes = model_entry(archive, name, remaining, continue_work)?;
        native = parse(&bytes, continue_work)?;
        native.model_settings_sha256 = Some(crate::provenance::digest(&bytes));
    }
    if let Some(name) = names.get("metadata/project_settings.config") {
        let bytes = model_entry(archive, name, remaining, continue_work)?;
        let settings: BTreeMap<String, Value> = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("unreadable native project settings"))?;
        native.project_settings = settings
            .into_iter()
            .filter(|(key, _)| PROJECT_SETTINGS.contains(&key.as_str()))
            .collect();
        native.project_settings_sha256 = Some(crate::provenance::digest(&bytes));
    }
    Ok(native)
}

fn parse(
    bytes: &[u8],
    continue_work: &mut impl FnMut() -> bool,
) -> Result<NativeMetadata, ToolError> {
    let mut result = NativeMetadata {
        unreported_metadata_preserved_in_source: true,
        ..Default::default()
    };
    let mut stack = Vec::<(Option<String>, String)>::new();
    let mut object: Option<(u32, NativeObject)> = None;
    let mut part: Option<NativePart> = None;
    let mut plate: Option<NativePlate> = None;
    let mut instance: Option<NativeInstance> = None;
    xml(bytes, "config", None, continue_work, |opening, element| {
        let path = |names: &[&str]| {
            stack.len() == names.len()
                && stack
                    .iter()
                    .zip(names)
                    .all(|((namespace, name), expected)| namespace.is_none() && name == expected)
        };
        if opening {
            if element.namespace.is_none() {
                match element.name.as_str() {
                    "object" if path(&["config"]) => {
                        object = Some((
                            integer(&element.attributes, "id", true)?,
                            NativeObject::default(),
                        ));
                    }
                    "part" if path(&["config", "object"]) => {
                        let id = element
                            .attributes
                            .get("id")
                            .map(|_| integer(&element.attributes, "id", true))
                            .transpose()?;
                        let range = element
                            .attributes
                            .get("lastid")
                            .map(|_| -> Result<_, ToolError> {
                                let first = integer(&element.attributes, "firstid", false)?;
                                let last = integer(&element.attributes, "lastid", false)?;
                                if last < first {
                                    return Err(invalid("reversed native part triangle range"));
                                }
                                Ok([first, last])
                            })
                            .transpose()?;
                        part = Some(NativePart {
                            id,
                            triangle_range: range,
                            subtype: element.attributes.get("subtype").cloned(),
                            ..Default::default()
                        });
                    }
                    "plate" if path(&["config"]) => plate = Some(NativePlate::default()),
                    "model_instance" if path(&["config", "plate"]) => {
                        instance = Some(NativeInstance::default())
                    }
                    "metadata" => {
                        if let (Some(key), Some(value)) = (
                            element.attributes.get("key"),
                            element.attributes.get("value"),
                        ) {
                            if path(&["config", "object"]) && LOCAL_SETTINGS.contains(&key.as_str())
                            {
                                insert_setting(
                                    &mut object
                                        .as_mut()
                                        .ok_or_else(|| invalid("metadata outside an object"))?
                                        .1
                                        .settings,
                                    key,
                                    value,
                                )?;
                            } else if path(&["config", "object", "part"])
                                && LOCAL_SETTINGS.contains(&key.as_str())
                            {
                                insert_setting(
                                    &mut part
                                        .as_mut()
                                        .ok_or_else(|| invalid("metadata outside a part"))?
                                        .settings,
                                    key,
                                    value,
                                )?;
                            } else if path(&["config", "plate"]) {
                                let plate = plate
                                    .as_mut()
                                    .ok_or_else(|| invalid("metadata outside a plate"))?;
                                match key.as_str() {
                                    "plater_id" => {
                                        let id: u32 = value.parse().map_err(|_| {
                                            invalid("invalid native plate identity")
                                        })?;
                                        if id == 0 || plate.plate.replace(id).is_some() {
                                            return Err(invalid("ambiguous native plate identity"));
                                        }
                                    }
                                    "locked" => {
                                        let locked = match value.as_str() {
                                            "true" => true,
                                            "false" => false,
                                            _ => return Err(invalid("invalid plate lock")),
                                        };
                                        if plate.locked.replace(locked).is_some() {
                                            return Err(invalid("ambiguous plate lock"));
                                        }
                                    }
                                    _ => {}
                                }
                            } else if path(&["config", "plate", "model_instance"]) {
                                let item = instance
                                    .as_mut()
                                    .ok_or_else(|| invalid("metadata outside an instance"))?;
                                match key.as_str() {
                                    "object_id" => set_number(&mut item.object_id, value)?,
                                    "instance_id" => set_number(&mut item.instance_id, value)?,
                                    "identify_id" => set_number(&mut item.identify_id, value)?,
                                    _ => {}
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            stack.push((element.namespace, element.name));
        } else {
            if element.namespace.is_none() {
                match element.name.as_str() {
                    "part" if path(&["config", "object", "part"]) => object
                        .as_mut()
                        .ok_or_else(|| invalid("part outside object"))?
                        .1
                        .parts
                        .push(part.take().ok_or_else(|| invalid("missing part"))?),
                    "object" if path(&["config", "object"]) => {
                        let (id, obj) = object.take().ok_or_else(|| invalid("missing object"))?;
                        if result.objects.insert(id, obj).is_some() {
                            return Err(invalid("duplicate native object identity"));
                        }
                    }
                    "model_instance" if path(&["config", "plate", "model_instance"]) => plate
                        .as_mut()
                        .ok_or_else(|| invalid("instance outside plate"))?
                        .instances
                        .push(instance.take().ok_or_else(|| invalid("missing instance"))?),
                    "plate" if path(&["config", "plate"]) => result
                        .plates
                        .push(plate.take().ok_or_else(|| invalid("missing plate"))?),
                    _ => {}
                }
            }
            if stack.pop() != Some((element.namespace, element.name)) {
                return Err(invalid("unmatched native metadata element"));
            }
        }
        Ok(())
    })?;
    let mut ids = BTreeSet::new();
    let mut instances = BTreeSet::new();
    for plate in &result.plates {
        if let Some(id) = plate.plate
            && !ids.insert(id)
        {
            return Err(invalid("duplicate native plate identity"));
        }
        for item in &plate.instances {
            if let (Some(object), Some(copy)) = (item.object_id, item.instance_id)
                && !instances.insert((object, copy))
            {
                return Err(invalid(
                    "native build instance belongs to multiple plate entries",
                ));
            }
        }
    }
    Ok(result)
}

fn insert_setting(
    settings: &mut BTreeMap<String, String>,
    key: &str,
    value: &str,
) -> Result<(), ToolError> {
    if value.len() > 2000 || settings.insert(key.to_owned(), value.to_owned()).is_some() {
        return Err(invalid("oversized or ambiguous native local setting"));
    }
    Ok(())
}

fn set_number<T: std::str::FromStr>(target: &mut Option<T>, value: &str) -> Result<(), ToolError> {
    let value = value
        .parse()
        .map_err(|_| invalid("invalid native instance identity"))?;
    if target.replace(value).is_some() {
        return Err(invalid("ambiguous native instance identity"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_identities_keep_plate_copy_part_and_material_domains_separate() {
        let bytes = br#"<config><object id="7"><metadata key="extruder" value="2"/><metadata key="unrelated_setting" value="not projected"/><part id="4" subtype="normal_part"><metadata key="extruder" value="1"/><metadata key="support_interface_filament" value="2"/></part></object><plate><metadata key="plater_id" value="2"/><metadata key="locked" value="true"/><model_instance><metadata key="object_id" value="7"/><metadata key="instance_id" value="0"/><metadata key="identify_id" value="100"/></model_instance></plate></config>"#;
        let native = parse(bytes, &mut || true).unwrap();
        assert_eq!(native.objects[&7].settings["extruder"], "2");
        assert!(
            !native.objects[&7]
                .settings
                .contains_key("unrelated_setting")
        );
        assert_eq!(native.objects[&7].parts[0].id, Some(4));
        assert_eq!(
            native.objects[&7].parts[0].settings["support_interface_filament"],
            "2"
        );
        assert_eq!(native.plates[0].plate, Some(2));
        assert_eq!(native.plates[0].locked, Some(true));
        assert_eq!(native.plates[0].instances[0].object_id, Some(7));
        assert_eq!(native.plates[0].instances[0].instance_id, Some(0));
        assert_eq!(native.plates[0].instances[0].identify_id, Some(100));
    }

    #[test]
    fn ambiguous_native_metadata_never_establishes_a_mapping() {
        for bytes in [
            "<config><object id='1'/><object id='1'/></config>",
            "<config><plate><metadata key='plater_id' value='1'/><metadata key='plater_id' value='2'/></plate></config>",
            "<config><object id='1'><metadata key='extruder' value='1'/><metadata key='extruder' value='2'/></object></config>",
            "<config><plate><metadata key='locked' value='maybe'/></plate></config>",
        ] {
            assert!(parse(bytes.as_bytes(), &mut || true).is_err(), "{bytes}");
        }
        assert!(parse(b"<config/>", &mut || false).is_err());
    }
}
