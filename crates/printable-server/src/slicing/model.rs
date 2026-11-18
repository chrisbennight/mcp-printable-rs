//! Namespace-aware 3MF model decoding. Archive references remain data; this
//! module never extracts a member to a caller-selected filesystem path.

mod native;
pub use native::{NativeMetadata, NativeObject, NativePart};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    path::Path,
};

use printable_geom::{
    TriangleMesh,
    plate::{IDENTITY_3MF, Transform3mf, compose_3mf, transform_mesh, validate_transform},
};
use quick_xml::{XmlVersion, events::Event, name::ResolveResult, reader::NsReader};
use serde::{Deserialize, Serialize};
use zip::ZipArchive;

use crate::error::ToolError;

const CORE: &str = "http://schemas.microsoft.com/3dmanufacturing/core/2015/02";
const PRODUCTION: &str = "http://schemas.microsoft.com/3dmanufacturing/production/2015/06";
const RELATIONSHIPS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

fn invalid(message: &str) -> ToolError {
    ToolError::Validation(format!("3MF layout: {message}"))
}

#[derive(Debug, Clone)]
pub struct Component {
    pub object_id: u32,
    pub path: Option<String>,
    pub transform: Transform3mf,
}

#[derive(Debug)]
pub struct Object {
    pub object_type: String,
    pub mesh: TriangleMesh,
    pub components: Vec<Component>,
    pub default_property_group: Option<u32>,
    pub default_property_index: Option<u32>,
    /// Native property groups and indices are not assumed to be filament slots.
    pub property_groups: BTreeSet<u32>,
    pub property_indices: BTreeSet<u32>,
}

#[derive(Debug)]
pub struct BuildItem {
    pub component: Component,
    /// Zero-based copy number for this resource in build order.
    pub copy: u32,
    pub printable: bool,
}

#[derive(Debug)]
pub struct Document {
    pub unit: String,
    pub objects: BTreeMap<u32, Object>,
    pub build: Vec<BuildItem>,
    pub unresolved: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectKey {
    pub model: String,
    pub object_id: u32,
}

#[derive(Debug)]
pub struct SourceArchive {
    pub primary_model: String,
    pub documents: BTreeMap<String, Document>,
    pub unresolved: BTreeSet<String>,
    pub xml_bytes_read: u64,
    pub native: NativeMetadata,
}

/// A leaf resource in one authored build instance. Component resources in the
/// same build item may intentionally meet or overlap; they are not independent
/// objects merely because the archive stores them separately.
#[derive(Debug)]
pub struct InstanceMesh {
    pub build_index: usize,
    pub copy: u32,
    pub resource: ObjectKey,
    pub component_path: Vec<usize>,
    pub transform: Transform3mf,
    pub object_type: String,
    pub mesh: TriangleMesh,
}

impl SourceArchive {
    fn key(&self, base: &str, component: &Component) -> Result<ObjectKey, ToolError> {
        let model = if let Some(path) = &component.path {
            let normalized = archive_name(path)?;
            self.documents
                .keys()
                .find(|name| name.eq_ignore_ascii_case(&normalized))
                .cloned()
                .ok_or_else(|| invalid("referenced model was not inspected"))?
        } else {
            base.to_owned()
        };
        Ok(ObjectKey {
            model,
            object_id: component.object_id,
        })
    }

    /// Expand reachable components without changing resource or material indices.
    /// The caller runs this inside the memory-limited geometry process.
    pub fn instances(&self, max_leaves: usize) -> Result<Vec<InstanceMesh>, ToolError> {
        struct Pending {
            key: ObjectKey,
            transform: Transform3mf,
            path: Vec<usize>,
            ancestors: BTreeSet<ObjectKey>,
        }
        let mut result = Vec::new();
        for (build_index, item) in self.documents[&self.primary_model].build.iter().enumerate() {
            if !item.printable {
                continue;
            }
            let mut pending = vec![Pending {
                key: self.key(&self.primary_model, &item.component)?,
                transform: item.component.transform,
                path: Vec::new(),
                ancestors: BTreeSet::new(),
            }];
            while let Some(mut next) = pending.pop() {
                printable_geom::limit::check()?;
                if next.path.len() > 256 {
                    return Err(invalid("component nesting exceeds the supported depth"));
                }
                if !next.ancestors.insert(next.key.clone()) {
                    return Err(invalid("cyclic component resources"));
                }
                let document = &self.documents[&next.key.model];
                if document.unit != "millimeter" {
                    return Err(invalid(
                        "instance geometry requires verified millimeter units",
                    ));
                }
                let object = document
                    .objects
                    .get(&next.key.object_id)
                    .ok_or_else(|| invalid("referenced object resource is missing"))?;
                if !object.mesh.triangles.is_empty() {
                    if result.len() >= max_leaves {
                        return Err(invalid(
                            "component expansion exceeds its requested leaf budget",
                        ));
                    }
                    result.push(InstanceMesh {
                        build_index,
                        copy: item.copy,
                        resource: next.key.clone(),
                        component_path: next.path.clone(),
                        transform: next.transform,
                        object_type: object.object_type.clone(),
                        mesh: transform_mesh(&object.mesh, &next.transform)?,
                    });
                }
                for (index, component) in object.components.iter().enumerate().rev() {
                    let mut path = next.path.clone();
                    path.push(index);
                    pending.push(Pending {
                        key: self.key(&next.key.model, component)?,
                        transform: compose_3mf(&next.transform, &component.transform)?,
                        path,
                        ancestors: next.ancestors.clone(),
                    });
                }
                if object.mesh.triangles.is_empty() && object.components.is_empty() {
                    return Err(invalid("printable resource has no supported geometry"));
                }
            }
        }
        if result.is_empty() {
            return Err(invalid("source has no printable mesh instances"));
        }
        Ok(result)
    }
}

fn archive_name(value: &str) -> Result<String, ToolError> {
    let value = value.strip_prefix('/').unwrap_or(value);
    if value.is_empty()
        || value.contains(['\\', '\0', '%', '?', '#', ':'])
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(invalid(
            "archive references must be internal paths without traversal or URI escapes",
        ));
    }
    Ok(value.to_owned())
}

fn model_entry(
    archive: &mut ZipArchive<File>,
    name: &str,
    remaining: &mut u64,
    continue_work: &mut impl FnMut() -> bool,
) -> Result<Vec<u8>, ToolError> {
    let mut entry = archive
        .by_name(name)
        .map_err(|_| invalid("referenced archive member is missing or unreadable"))?;
    if entry.size() > *remaining {
        return Err(invalid("inspection exceeds its requested XML byte budget"));
    }
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if !continue_work() {
            return Err(invalid(
                "inspection stopped at its time or cancellation budget",
            ));
        }
        let read = entry.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if bytes.len() as u64 + read as u64 > *remaining {
            return Err(invalid("inspection exceeds its requested XML byte budget"));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    *remaining -= bytes.len() as u64;
    Ok(bytes)
}

/// Read only the primary model and resources reachable from its build. Call this
/// in the disposable geometry worker so expansion and exact CSG share its memory
/// and runtime limits. Other archive members remain untouched.
pub fn read_archive(
    path: &Path,
    max_xml_bytes: u64,
    mut continue_work: impl FnMut() -> bool,
) -> Result<SourceArchive, ToolError> {
    if max_xml_bytes == 0 {
        return Err(invalid("inspection requires a positive XML byte budget"));
    }
    if !continue_work() {
        return Err(invalid(
            "inspection stopped at its time or cancellation budget",
        ));
    }
    let mut archive = super::package::open(path)?;
    if archive.has_duplicate_names() {
        return Err(invalid("duplicate archive member names are ambiguous"));
    }
    let mut names = BTreeMap::new();
    for index in 0..archive.len() {
        if !continue_work() {
            return Err(invalid(
                "inspection stopped at its time or cancellation budget",
            ));
        }
        let entry = archive
            .by_index(index)
            .map_err(|_| invalid("archive member cannot be read"))?;
        let name = std::str::from_utf8(entry.name_raw())
            .map_err(|_| invalid("archive member name is not UTF-8"))?;
        if name.ends_with('/') {
            continue;
        }
        let normalized = archive_name(name)?;
        if names
            .insert(normalized.to_ascii_lowercase(), name.to_owned())
            .is_some()
        {
            return Err(invalid(
                "case-equivalent archive member names are ambiguous",
            ));
        }
    }
    let canonical = |name: &str| -> Result<String, ToolError> {
        names
            .get(&archive_name(name)?.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| invalid("referenced model member is missing"))
    };
    let mut remaining = max_xml_bytes;
    let relationships = model_entry(
        &mut archive,
        &canonical("_rels/.rels")?,
        &mut remaining,
        &mut continue_work,
    )?;
    let primary = canonical(&primary_model(&relationships, &mut continue_work)?)?;
    let bytes = model_entry(&mut archive, &primary, &mut remaining, &mut continue_work)?;
    let document = parse_model(&bytes, &mut continue_work)?;
    let reference = |base: &str, component: &Component| -> Result<ObjectKey, ToolError> {
        let model = if let Some(path) = &component.path {
            if !path.starts_with('/') {
                return Err(invalid(
                    "relative production resource paths are not verified for the packaged engine",
                ));
            }
            canonical(path)?
        } else {
            base.to_owned()
        };
        Ok(ObjectKey {
            model,
            object_id: component.object_id,
        })
    };
    let mut pending = document
        .build
        .iter()
        .map(|item| reference(&primary, &item.component))
        .collect::<Result<Vec<_>, _>>()?;
    let mut documents = BTreeMap::from([(primary.clone(), document)]);
    let mut visited = BTreeSet::new();
    let mut unresolved = documents[&primary].unresolved.clone();
    while let Some(key) = pending.pop() {
        if !continue_work() {
            return Err(invalid(
                "inspection stopped at its time or cancellation budget",
            ));
        }
        if !visited.insert(key.clone()) {
            continue;
        }
        if !documents.contains_key(&key.model) {
            let bytes = model_entry(&mut archive, &key.model, &mut remaining, &mut continue_work)?;
            documents.insert(key.model.clone(), parse_model(&bytes, &mut continue_work)?);
        }
        let document = &documents[&key.model];
        if document.unit != "millimeter" {
            unresolved.insert("non_millimeter_native_interpretation_not_verified".into());
        }
        unresolved.extend(document.unresolved.iter().cloned());
        let object = document
            .objects
            .get(&key.object_id)
            .ok_or_else(|| invalid("referenced object resource is missing"))?;
        if !object.mesh.triangles.is_empty() && !object.components.is_empty() {
            unresolved.insert("mixed_mesh_and_component_object_not_verified".into());
        }
        for component in &object.components {
            pending.push(reference(&key.model, component)?);
        }
    }
    let native = native::read(&mut archive, &names, &mut remaining, &mut continue_work)?;
    Ok(SourceArchive {
        primary_model: primary,
        documents,
        unresolved,
        xml_bytes_read: max_xml_bytes - remaining,
        native,
    })
}

struct Element {
    namespace: Option<String>,
    name: String,
    attributes: BTreeMap<String, String>,
    production_path: Option<String>,
    unsupported_required_extensions: bool,
}

fn xml(
    bytes: &[u8],
    root_name: &str,
    root_namespace: Option<&str>,
    continue_work: &mut impl FnMut() -> bool,
    mut visit: impl FnMut(bool, Element) -> Result<(), ToolError>,
) -> Result<(), ToolError> {
    let mut reader = NsReader::from_reader(bytes);
    reader.config_mut().expand_empty_elements = true;
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    let mut root_seen = false;
    loop {
        if !continue_work() {
            return Err(invalid(
                "inspection stopped at its time or cancellation budget",
            ));
        }
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| invalid("malformed XML"))?
        {
            Event::Start(start) => {
                let (namespace, local) = reader.resolver().resolve_element(start.name());
                let namespace = match namespace {
                    ResolveResult::Bound(namespace) => Some(
                        std::str::from_utf8(namespace.as_ref())
                            .map_err(|_| invalid("invalid namespace"))?
                            .to_owned(),
                    ),
                    ResolveResult::Unbound => None,
                    ResolveResult::Unknown(_) => {
                        return Err(invalid("undeclared element namespace"));
                    }
                };
                let name = std::str::from_utf8(local.as_ref())
                    .map_err(|_| invalid("invalid element name"))?
                    .to_owned();
                if depth == 0 {
                    if root_seen || name != root_name || namespace.as_deref() != root_namespace {
                        return Err(invalid("unexpected document root or namespace"));
                    }
                    root_seen = true;
                }
                let mut attributes = BTreeMap::new();
                let mut production_path = None;
                let mut unsupported_required_extensions = false;
                for attribute in start.attributes().with_checks(true) {
                    let attribute =
                        attribute.map_err(|_| invalid("invalid or duplicate attribute"))?;
                    if attribute.key.as_ref() == b"xmlns"
                        || attribute.key.as_ref().starts_with(b"xmlns:")
                    {
                        continue;
                    }
                    let (namespace, local) = reader.resolver().resolve_attribute(attribute.key);
                    let key = match namespace {
                        ResolveResult::Unbound => Some(
                            std::str::from_utf8(local.as_ref())
                                .map_err(|_| invalid("invalid attribute name"))?
                                .to_owned(),
                        ),
                        ResolveResult::Bound(namespace)
                            if namespace.as_ref() == PRODUCTION.as_bytes()
                                && local.as_ref() == b"path" =>
                        {
                            None
                        }
                        ResolveResult::Bound(_) => continue,
                        ResolveResult::Unknown(_) => {
                            return Err(invalid("undeclared attribute namespace"));
                        }
                    };
                    let value = attribute
                        .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                        .map_err(|_| invalid("invalid XML attribute value"))?
                        .into_owned();
                    if let Some(key) = key {
                        if key == "requiredextensions" {
                            unsupported_required_extensions = value.split_ascii_whitespace().any(|prefix| {
                                let name = format!("{prefix}:extension");
                                !matches!(reader.resolver().resolve_element(quick_xml::name::QName(name.as_bytes())).0,
                                    ResolveResult::Bound(namespace) if namespace.as_ref() == PRODUCTION.as_bytes())
                            });
                        }
                        if attributes.insert(key, value).is_some() {
                            return Err(invalid("ambiguous attributes"));
                        }
                    } else if production_path.replace(value).is_some() {
                        return Err(invalid("duplicate production resource path"));
                    }
                }
                visit(
                    true,
                    Element {
                        namespace,
                        name,
                        attributes,
                        production_path,
                        unsupported_required_extensions,
                    },
                )?;
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| invalid("XML nesting exceeds the numeric range"))?;
            }
            Event::End(end) => {
                let (namespace, local) = reader.resolver().resolve_element(end.name());
                let namespace = match namespace {
                    ResolveResult::Bound(namespace) => Some(
                        std::str::from_utf8(namespace.as_ref())
                            .map_err(|_| invalid("invalid namespace"))?
                            .to_owned(),
                    ),
                    ResolveResult::Unbound => None,
                    ResolveResult::Unknown(_) => {
                        return Err(invalid("undeclared element namespace"));
                    }
                };
                let name = std::str::from_utf8(local.as_ref())
                    .map_err(|_| invalid("invalid element name"))?
                    .to_owned();
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| invalid("unmatched XML closing element"))?;
                visit(
                    false,
                    Element {
                        namespace,
                        name,
                        attributes: BTreeMap::new(),
                        production_path: None,
                        unsupported_required_extensions: false,
                    },
                )?;
            }
            Event::DocType(_) => return Err(invalid("document type declarations are unsupported")),
            Event::Text(text) if depth == 0 => {
                if !text
                    .decode()
                    .map_err(|_| invalid("invalid XML text"))?
                    .trim()
                    .is_empty()
                {
                    return Err(invalid("text outside the document root"));
                }
            }
            Event::Eof => {
                if !root_seen || depth != 0 {
                    return Err(invalid("incomplete XML document"));
                }
                return Ok(());
            }
            _ => {}
        }
        buffer.clear();
    }
}

fn integer(
    attributes: &BTreeMap<String, String>,
    key: &str,
    positive: bool,
) -> Result<u32, ToolError> {
    attributes
        .get(key)
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| !positive || *value > 0)
        .ok_or_else(|| invalid("missing or invalid integer attribute"))
}

fn transform(attributes: &BTreeMap<String, String>) -> Result<Transform3mf, ToolError> {
    let Some(value) = attributes.get("transform") else {
        return Ok(IDENTITY_3MF);
    };
    let values = value
        .split_ascii_whitespace()
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("invalid transform coefficient"))?;
    let values: Transform3mf = values
        .try_into()
        .map_err(|_| invalid("transforms require twelve coefficients"))?;
    validate_transform(&values)?;
    Ok(values)
}

fn component(element: &Element) -> Result<Component, ToolError> {
    Ok(Component {
        object_id: integer(&element.attributes, "objectid", true)?,
        path: element.production_path.clone(),
        transform: transform(&element.attributes)?,
    })
}

pub fn parse_model(
    bytes: &[u8],
    mut continue_work: impl FnMut() -> bool,
) -> Result<Document, ToolError> {
    let mut document = Document {
        unit: "millimeter".into(),
        objects: BTreeMap::new(),
        build: Vec::new(),
        unresolved: BTreeSet::new(),
    };
    let mut stack = Vec::<(bool, String)>::new();
    let mut current: Option<(u32, Object)> = None;
    let mut copies = BTreeMap::<(Option<String>, u32), u32>::new();
    xml(
        bytes,
        "model",
        Some(CORE),
        &mut continue_work,
        |opening, element| {
            if opening {
                let core = element.namespace.as_deref() == Some(CORE);
                let parent = stack
                    .last()
                    .filter(|(core, _)| *core)
                    .map(|(_, name)| name.as_str());
                let path = |names: &[&str]| {
                    stack.len() == names.len()
                        && stack
                            .iter()
                            .zip(names)
                            .all(|((core, name), expected)| *core && name == expected)
                };
                if core {
                    match (element.name.as_str(), parent) {
                        ("model", None) => {
                            document.unit = element
                                .attributes
                                .get("unit")
                                .cloned()
                                .unwrap_or_else(|| "millimeter".into());
                            if element.unsupported_required_extensions {
                                document
                                    .unresolved
                                    .insert("required_model_extensions_not_verified".into());
                            }
                        }
                        ("object", Some("resources")) if path(&["model", "resources"]) => {
                            if current.is_some() {
                                return Err(invalid("nested object definitions"));
                            }
                            let default_property_group = element
                                .attributes
                                .contains_key("pid")
                                .then(|| integer(&element.attributes, "pid", true))
                                .transpose()?;
                            let default_property_index = element
                                .attributes
                                .contains_key("pindex")
                                .then(|| integer(&element.attributes, "pindex", false))
                                .transpose()?;
                            current = Some((
                                integer(&element.attributes, "id", true)?,
                                Object {
                                    object_type: element
                                        .attributes
                                        .get("type")
                                        .cloned()
                                        .unwrap_or_else(|| "model".into()),
                                    mesh: TriangleMesh {
                                        vertices: Vec::new(),
                                        triangles: Vec::new(),
                                    },
                                    components: Vec::new(),
                                    default_property_group,
                                    default_property_index,
                                    property_groups: default_property_group.into_iter().collect(),
                                    property_indices: default_property_index.into_iter().collect(),
                                },
                            ));
                        }
                        ("vertex", Some("vertices"))
                            if path(&["model", "resources", "object", "mesh", "vertices"]) =>
                        {
                            let object = &mut current
                                .as_mut()
                                .ok_or_else(|| invalid("vertex outside an object"))?
                                .1;
                            let point = ["x", "y", "z"].map(|key| {
                                element
                                    .attributes
                                    .get(key)
                                    .and_then(|value| value.parse::<f64>().ok())
                                    .filter(|value| value.is_finite())
                                    .ok_or_else(|| invalid("invalid vertex coordinate"))
                            });
                            let [x, y, z] = point;
                            object.mesh.vertices.push([x?, y?, z?]);
                        }
                        ("triangle", Some("triangles"))
                            if path(&["model", "resources", "object", "mesh", "triangles"]) =>
                        {
                            let object = &mut current
                                .as_mut()
                                .ok_or_else(|| invalid("triangle outside an object"))?
                                .1;
                            object.mesh.triangles.push([
                                integer(&element.attributes, "v1", false)?,
                                integer(&element.attributes, "v2", false)?,
                                integer(&element.attributes, "v3", false)?,
                            ]);
                            for key in ["p1", "p2", "p3"] {
                                if element.attributes.contains_key(key) {
                                    object.property_indices.insert(integer(
                                        &element.attributes,
                                        key,
                                        false,
                                    )?);
                                }
                            }
                            if element.attributes.contains_key("pid") {
                                object.property_groups.insert(integer(
                                    &element.attributes,
                                    "pid",
                                    true,
                                )?);
                            }
                        }
                        ("component", Some("components"))
                            if path(&["model", "resources", "object", "components"]) =>
                        {
                            current
                                .as_mut()
                                .ok_or_else(|| invalid("component outside an object"))?
                                .1
                                .components
                                .push(component(&element)?)
                        }
                        ("item", Some("build")) if path(&["model", "build"]) => {
                            let component = component(&element)?;
                            let count = copies
                                .entry((component.path.clone(), component.object_id))
                                .or_default();
                            let printable =
                                match element.attributes.get("printable").map(String::as_str) {
                                    None | Some("1" | "true") => true,
                                    Some("0" | "false") => false,
                                    _ => return Err(invalid("invalid printable attribute")),
                                };
                            document.build.push(BuildItem {
                                component,
                                copy: *count,
                                printable,
                            });
                            *count = count.checked_add(1).ok_or_else(|| {
                                invalid("instance count exceeds the numeric range")
                            })?;
                        }
                        ("mesh" | "components", Some("object"))
                            if path(&["model", "resources", "object"]) => {}
                        ("vertices" | "triangles", Some("mesh"))
                            if path(&["model", "resources", "object", "mesh"]) => {}
                        (
                            "object" | "vertex" | "triangle" | "component" | "item" | "mesh"
                            | "vertices" | "triangles" | "components",
                            _,
                        ) => return Err(invalid("unexpected geometry element location")),
                        _ => {}
                    }
                } else if current.is_some() && element.name != "metadata" {
                    document
                        .unresolved
                        .insert("geometry_extension_not_verified".into());
                }
                stack.push((core, element.name));
            } else {
                if element.namespace.as_deref() == Some(CORE)
                    && element.name == "object"
                    && stack.iter().map(|(core, name)| (*core, name.as_str())).eq([
                        (true, "model"),
                        (true, "resources"),
                        (true, "object"),
                    ])
                {
                    let (id, object) = current
                        .take()
                        .ok_or_else(|| invalid("object closing without a definition"))?;
                    if document.objects.insert(id, object).is_some() {
                        return Err(invalid("duplicate object resource"));
                    }
                }
                if stack.pop() != Some((element.namespace.as_deref() == Some(CORE), element.name)) {
                    return Err(invalid("unmatched model element"));
                }
            }
            Ok(())
        },
    )?;
    Ok(document)
}

/// Select the sole internal OPC relationship identifying the primary model.
pub fn primary_model(
    bytes: &[u8],
    mut continue_work: impl FnMut() -> bool,
) -> Result<String, ToolError> {
    let mut targets = Vec::new();
    xml(
        bytes,
        "Relationships",
        Some(RELATIONSHIPS),
        &mut continue_work,
        |opening, element| {
            if opening
                && element.namespace.as_deref() == Some(RELATIONSHIPS)
                && element.name == "Relationship"
                && element.attributes.get("Type").is_some_and(|value| {
                    value == "http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"
                })
            {
                if element
                    .attributes
                    .get("TargetMode")
                    .is_some_and(|value| value != "Internal")
                {
                    return Err(invalid("external model references are unsupported"));
                }
                targets.push(
                    element
                        .attributes
                        .get("Target")
                        .cloned()
                        .ok_or_else(|| invalid("primary model relationship lacks a target"))?,
                );
            }
            Ok(())
        },
    )?;
    if targets.len() != 1 {
        return Err(invalid("one primary model relationship is required"));
    }
    Ok(targets.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(body: &str) -> String {
        format!(r#"<model xmlns="{CORE}" xmlns:p="{PRODUCTION}" unit="millimeter">{body}</model>"#)
    }

    #[test]
    fn reads_production_components_build_copies_and_property_indices() {
        let text = model(
            r#"<resources><object id="1"><mesh><vertices><vertex x="0" y="0" z="0"/><vertex x="1" y="0" z="0"/><vertex x="0" y="1" z="0"/></vertices><triangles><triangle v1="0" v2="1" v3="2" pid="4" p1="2"/></triangles></mesh></object><object id="2"><components><component objectid="1" p:path="/3D/Objects/a.model"/></components></object></resources><build><item objectid="2" transform="1 0 0 0 1 0 0 0 1 10 20 0"/><item objectid="2" printable="0"/></build>"#,
        );
        let document = parse_model(text.as_bytes(), || true).unwrap();
        assert_eq!(document.objects[&1].mesh.triangles, [[0, 1, 2]]);
        assert_eq!(document.objects[&1].property_indices, BTreeSet::from([2]));
        assert_eq!(
            document.objects[&2].components[0].path.as_deref(),
            Some("/3D/Objects/a.model")
        );
        assert_eq!(document.build[0].component.transform[9..], [10., 20., 0.]);
        assert_eq!(document.build[1].copy, 1);
        assert!(!document.build[1].printable);
    }

    #[test]
    fn retains_object_property_defaults_and_explicit_triangle_references() {
        let text = model(
            r#"<resources><object id="1" pid="4" pindex="0"><mesh><vertices><vertex x="0" y="0" z="0"/><vertex x="1" y="0" z="0"/><vertex x="0" y="1" z="0"/></vertices><triangles><triangle v1="0" v2="1" v3="2"/><triangle v1="0" v2="1" v3="2" pid="7" p1="2" p2="3"/></triangles></mesh></object></resources><build><item objectid="1"/></build>"#,
        );
        let document = parse_model(text.as_bytes(), || true).unwrap();
        let object = &document.objects[&1];
        assert_eq!(object.default_property_group, Some(4));
        assert_eq!(object.default_property_index, Some(0));
        assert_eq!(object.property_groups, BTreeSet::from([4, 7]));
        assert_eq!(object.property_indices, BTreeSet::from([0, 2, 3]));
        for attribute in ["pid='0'", "pid='bad'", "pindex='-1'"] {
            let invalid = model(&format!(
                "<resources><object id='1' {attribute}/></resources><build/>"
            ));
            assert!(parse_model(invalid.as_bytes(), || true).is_err());
        }
    }

    #[test]
    fn rejects_dtds_duplicates_wrong_namespaces_and_interrupted_reads() {
        let valid = model(r#"<resources><object id="1"/><object id="1"/></resources><build/>"#);
        assert!(parse_model(valid.as_bytes(), || true).is_err());
        assert!(parse_model(model("<resources/><build/>").as_bytes(), || false).is_err());
        assert!(parse_model(br#"<!DOCTYPE model><model xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02"/>"#, || true).is_err());
        assert!(parse_model(b"<model xmlns='wrong'/>", || true).is_err());
    }

    fn archive_fixture(extra: &[(&str, &[u8])]) -> (tempfile::NamedTempFile, u64) {
        use std::io::Write;
        let relationships = format!(
            r#"<Relationships xmlns="{RELATIONSHIPS}"><Relationship Id="model" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel" Target="/3D/3dmodel.model"/></Relationships>"#
        );
        let main = model(
            r#"<resources><object id="2"><components><component objectid="1" p:path="/3D/Objects/a.model"/></components></object></resources><build><item objectid="2"/></build>"#,
        );
        let part = model(
            r#"<resources><object id="1"><mesh><vertices><vertex x="0" y="0" z="0"/><vertex x="1" y="0" z="0"/><vertex x="0" y="1" z="0"/></vertices><triangles><triangle v1="0" v2="1" v3="2"/></triangles></mesh></object></resources><build/>"#,
        );
        let mut entries = vec![
            ("_rels/.rels", relationships.as_bytes()),
            ("3D/3dmodel.model", main.as_bytes()),
            ("3D/Objects/a.model", part.as_bytes()),
        ];
        entries.extend_from_slice(extra);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&super::super::package::fixture::package(&entries))
            .unwrap();
        (file, (relationships.len() + main.len() + part.len()) as u64)
    }

    #[test]
    fn reachable_resources_share_the_requested_xml_budget() {
        let (file, bytes) = archive_fixture(&[("Metadata/unknown.bin", b"preserved data")]);
        let archive = read_archive(file.path(), bytes, || true).unwrap();
        assert_eq!(archive.primary_model, "3D/3dmodel.model");
        assert_eq!(archive.xml_bytes_read, bytes);
        assert_eq!(archive.documents.len(), 2);
        assert_eq!(
            archive.documents["3D/Objects/a.model"].objects[&1]
                .mesh
                .vertices
                .len(),
            3
        );
        assert!(read_archive(file.path(), bytes - 1, || true).is_err());
        assert!(read_archive(file.path(), bytes, || false).is_err());
    }

    #[test]
    fn native_project_retains_sparse_layer_combination_setting() {
        let settings = br#"{"wipe_tower_sparse_layers_combination":"1"}"#;
        let (file, bytes) = archive_fixture(&[("Metadata/project_settings.config", settings)]);
        let archive = read_archive(file.path(), bytes + settings.len() as u64, || true).unwrap();
        assert_eq!(
            archive.native.project_settings["wipe_tower_sparse_layers_combination"],
            "1"
        );
    }

    #[test]
    fn ambiguous_or_external_archive_names_are_rejected() {
        for path in [
            "../outside",
            "/../outside",
            "//host/model",
            "https://example.invalid/model",
            "3D/%2e%2e/model",
            "3D\\model",
            "3D/./model",
        ] {
            assert!(archive_name(path).is_err(), "{path}");
        }
        let (file, bytes) = archive_fixture(&[("Metadata/A.txt", b"a"), ("Metadata/a.txt", b"b")]);
        assert!(read_archive(file.path(), bytes, || true).is_err());
        let xml = model(
            r#"<resources><object id="1"><x:mesh xmlns:x="wrong"><vertices><vertex x="0" y="0" z="0"/></vertices></x:mesh></object></resources><build/>"#,
        );
        assert!(parse_model(xml.as_bytes(), || true).is_err());
    }

    #[test]
    fn unqualified_attributes_cannot_impersonate_production_resource_paths() {
        let xml = model(
            r#"<resources><object id="2"><components><component objectid="1" production_path="/3D/evil.model"/></components></object></resources><build/>"#,
        );
        let document = parse_model(xml.as_bytes(), || true).unwrap();
        assert!(document.objects[&2].components[0].path.is_none());
    }

    #[test]
    fn required_production_prefixes_are_resolved_instead_of_assumed() {
        let xml = format!(
            "<model xmlns='{CORE}' xmlns:parts='{PRODUCTION}' requiredextensions='parts'><resources/><build/></model>"
        );
        assert!(
            parse_model(xml.as_bytes(), || true)
                .unwrap()
                .unresolved
                .is_empty()
        );
        let xml = xml.replace(PRODUCTION, "urn:unverified-extension");
        assert!(
            parse_model(xml.as_bytes(), || true)
                .unwrap()
                .unresolved
                .contains("required_model_extensions_not_verified")
        );
    }

    #[test]
    fn cyclic_components_do_not_expand_forever() {
        let xml = model(
            "<resources><object id='1'><components><component objectid='2'/></components></object><object id='2'><components><component objectid='1'/></components></object></resources><build><item objectid='1'/></build>",
        );
        let archive = SourceArchive {
            primary_model: "3D/3dmodel.model".into(),
            documents: BTreeMap::from([(
                "3D/3dmodel.model".into(),
                parse_model(xml.as_bytes(), || true).unwrap(),
            )]),
            unresolved: BTreeSet::new(),
            xml_bytes_read: xml.len() as u64,
            native: NativeMetadata::default(),
        };
        assert!(
            archive
                .instances(10)
                .unwrap_err()
                .to_string()
                .contains("cyclic")
        );
    }
}
