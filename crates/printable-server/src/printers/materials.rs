use super::{PrinterService, validate_id};
use crate::error::ToolError;
use bambuddy_api::{
    ApiError, BambuddyApi, PrinterStatus, material_types::*, materials::*, printer_types::*,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Unavailable {
    PermissionDenied,
    NotFound,
    Unreachable,
    InvalidResponse,
    ChangedDuringRead,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Observation<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<Unavailable>,
}
impl<T> Observation<T> {
    pub(super) fn from_result(result: Result<T, ApiError>) -> Self {
        match result {
            Ok(data) => Self {
                data: Some(data),
                unavailable: None,
            },
            Err(error) => Self {
                data: None,
                unavailable: Some(match error {
                    ApiError::Forbidden | ApiError::Authentication => Unavailable::PermissionDenied,
                    ApiError::NotFound => Unavailable::NotFound,
                    ApiError::Unavailable => Unavailable::Unreachable,
                    _ => Unavailable::InvalidResponse,
                }),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Material {
    pub ams_id: Option<u32>,
    pub slot_id: u32,
    pub slot_label: String,
    pub mapping_id: Option<u32>,
    #[serde(rename = "type")]
    pub material_type: Option<String>,
    pub color: Option<String>,
    pub product_name: Option<String>,
    pub product_source: Option<String>,
    pub filament_id: Option<String>,
    pub remaining_percent: Option<i32>,
    pub state: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<MaterialDetail>,
}
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MaterialDetail {
    pub reported: MaterialTray,
    pub preset: Option<SlotPreset>,
    pub inventory: Option<SpoolAssignment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicting_inventory: Option<SpoolAssignment>,
    pub catalog_name: Option<String>,
    pub remaining_grams: Option<f64>,
    pub remaining_grams_source: Option<String>,
    pub active: Option<bool>,
    pub extruder: Option<i64>,
}

pub(super) struct LoadedMaterials {
    pub status: PrinterStatus,
    pub materials: Vec<Material>,
    pub sources: BTreeMap<String, Option<Unavailable>>,
}

fn nonempty(value: &Option<String>) -> Option<String> {
    value.as_ref().filter(|s| !s.trim().is_empty()).cloned()
}
fn same_text(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}
fn same_tray(a: &MaterialTray, b: &MaterialTray) -> bool {
    a.id == b.id
        && a.tray_info_idx == b.tray_info_idx
        && a.tray_sub_brands == b.tray_sub_brands
        && a.tray_id_name == b.tray_id_name
        && a.tray_type == b.tray_type
        && a.tray_color == b.tray_color
        && a.tag_uid == b.tag_uid
        && a.tray_uuid == b.tray_uuid
}
fn trays(status: &PrinterStatus) -> Vec<(Option<u32>, &MaterialTray)> {
    status
        .ams
        .iter()
        .flat_map(|u| u.tray.iter().map(move |t| (Some(u.id), t)))
        .chain(status.vt_tray.iter().map(|t| (None, t)))
        .collect()
}

impl PrinterService {
    pub(super) async fn loaded_materials(
        &self,
        id: u64,
        detail: bool,
    ) -> Result<LoadedMaterials, ToolError> {
        validate_id(id)?;
        let before = self.read.printer_status(id).await?;
        let (presets, labels, assignments, names, remaining) = tokio::join!(
            self.read.slot_presets(id),
            self.read.ams_labels(id),
            self.read.assignments(id),
            self.read.filament_names(),
            self.read.inventory_remaining(id)
        );
        let presets = Observation::from_result(presets);
        let labels = Observation::from_result(labels);
        let assignments = Observation::from_result(assignments);
        let names = Observation::from_result(names);
        let remaining = Observation::from_result(remaining);
        let status = self.read.printer_status(id).await?;
        let mut sources = BTreeMap::from([
            ("slot_presets".into(), presets.unavailable.clone()),
            ("labels".into(), labels.unavailable.clone()),
            ("inventory".into(), assignments.unavailable.clone()),
            ("catalog".into(), names.unavailable.clone()),
            ("remaining_grams".into(), remaining.unavailable.clone()),
        ]);
        let previous = trays(&before);
        let materials = trays(&status)
            .into_iter()
            .map(|(ams, tray)| {
                let unchanged = previous
                    .iter()
                    .any(|(unit, t)| *unit == ams && same_tray(t, tray));
                let mapping = match ams {
                    Some(id) if (128..=135).contains(&id) => Some(id),
                    Some(id) => id.checked_mul(4).and_then(|n| n.checked_add(tray.id)),
                    None => Some(tray.id),
                };
                let saved_ams = ams.unwrap_or(255);
                // External preset records use slot 0/1 while status uses tray 254/255.
                let saved_tray = if ams.is_none() {
                    tray.id.saturating_sub(254)
                } else {
                    tray.id
                };
                let preset = unchanged
                    .then(|| {
                        presets
                            .data
                            .as_ref()?
                            .values()
                            .find(|p| p.ams_id == saved_ams && p.tray_id == saved_tray)
                            .cloned()
                    })
                    .flatten();
                let assignment = unchanged
                    .then(|| {
                        assignments
                            .data
                            .as_ref()?
                            .iter()
                            .find(|a| {
                                a.ams_id == Some(i64::from(saved_ams))
                                    && a.tray_id == Some(i64::from(saved_tray))
                            })
                            .cloned()
                    })
                    .flatten();
                let catalog = names
                    .data
                    .as_ref()
                    .and_then(|ns| {
                        ns.iter()
                            .find(|n| Some(&n.filament_id) == tray.tray_info_idx.as_ref())
                    })
                    .map(|n| n.name.clone());
                let mut conflicts = Vec::new();
                let inventory = assignment.clone().filter(|a| {
                    let agrees = a
                        .fingerprint_type
                        .as_ref()
                        .zip(tray.tray_type.as_ref())
                        .is_none_or(|(a, b)| same_text(a, b))
                        && a.fingerprint_color
                            .as_ref()
                            .zip(tray.tray_color.as_ref())
                            .is_none_or(|(a, b)| same_text(a, b))
                        && a.spool.as_ref().is_none_or(|s| {
                            s.material
                                .as_ref()
                                .zip(tray.tray_type.as_ref())
                                .is_none_or(|(a, b)| same_text(a, b))
                                && s.tag_uid
                                    .as_ref()
                                    .zip(tray.tag_uid.as_ref())
                                    .is_none_or(|(a, b)| a == b)
                                && s.tray_uuid
                                    .as_ref()
                                    .zip(tray.tray_uuid.as_ref())
                                    .is_none_or(|(a, b)| a == b)
                        });
                    if !agrees {
                        conflicts
                            .push("inventory assignment does not match the reported spool".into());
                    }
                    agrees
                });
                let conflicting_inventory = if inventory.is_none() {
                    assignment
                } else {
                    None
                };
                if !unchanged {
                    sources.insert("enrichment".into(), Some(Unavailable::ChangedDuringRead));
                }
                let reported_name = nonempty(&tray.tray_sub_brands);
                let preset_name = preset.as_ref().and_then(|p| nonempty(&p.preset_name));
                let spool_name = inventory
                    .as_ref()
                    .and_then(|a| a.spool.as_ref())
                    .and_then(|s| nonempty(&s.slicer_filament_name));
                let names = [
                    ("device", &reported_name),
                    ("catalog", &catalog),
                    ("inventory", &spool_name),
                    ("saved preset", &preset_name),
                ];
                for (i, (source, name)) in names.iter().enumerate() {
                    for (other_source, other) in &names[i + 1..] {
                        if let (Some(name), Some(other)) = (name, other) {
                            let name = product_name_key(name);
                            let other = product_name_key(other);
                            if name != other {
                                conflicts.push(format!(
                                    "{source} and {other_source} product names disagree"
                                ));
                            }
                        }
                    }
                }
                let (product_name, product_source) = [
                    (reported_name, "device"),
                    (catalog.clone(), "bambuddy_builtin_catalog"),
                    (spool_name, "inventory"),
                    (preset_name, "saved_preset"),
                ]
                .into_iter()
                .find_map(|(name, source)| name.map(|n| (Some(n), Some(source.to_owned()))))
                .unwrap_or((None, None));
                let grams = if unchanged && inventory.is_some() && conflicts.is_empty() {
                    remaining
                        .data
                        .as_ref()
                        .and_then(|r| {
                            mapping.and_then(|m| r.inventory_remain_g.get(&m.to_string()).copied())
                        })
                        .filter(|g| g.is_finite() && *g >= 0.0)
                } else {
                    None
                };
                let unit_label =
                    ams.and_then(|a| labels.data.as_ref()?.get(&a.to_string()).cloned());
                let slot_label = match (ams, unit_label) {
                    (Some(_), Some(label)) => format!("{label}, slot {}", tray.id + 1),
                    (Some(a), None) if (128..=135).contains(&a) => {
                        format!("AMS HT {}, slot 1", a - 127)
                    }
                    (Some(a), None) => format!("AMS {}, slot {}", a + 1, tray.id + 1),
                    (None, _) => format!("External spool {}", saved_tray + 1),
                };
                Material {
                    ams_id: ams,
                    slot_id: tray.id,
                    slot_label,
                    mapping_id: mapping,
                    material_type: nonempty(&tray.tray_type),
                    color: nonempty(&tray.tray_color),
                    product_name,
                    product_source,
                    filament_id: nonempty(&tray.tray_info_idx),
                    remaining_percent: tray.remain.filter(|p| (0..=100).contains(p)),
                    state: tray.state,
                    conflicts,
                    detail: detail.then(|| MaterialDetail {
                        reported: tray.clone(),
                        preset,
                        inventory,
                        conflicting_inventory,
                        catalog_name: catalog,
                        remaining_grams: grams,
                        remaining_grams_source: grams.map(|_| "bambuddy_inventory_remain".into()),
                        active: status
                            .tray_now
                            .zip(mapping)
                            .map(|(now, m)| now == i64::from(m)),
                        extruder: ams
                            .and_then(|a| status.ams_extruder_map.get(&a.to_string()).copied()),
                    }),
                }
            })
            .collect();
        Ok(LoadedMaterials {
            status,
            materials,
            sources,
        })
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MaterialScope {
    Loaded,
    Inventory,
    Presets,
    Catalog,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaterialsParams {
    pub scope: MaterialScope,
    pub printer_id: Option<u64>,
    pub spool_id: Option<i64>,
    /// Case-insensitive name, material, brand, or preset search.
    pub query: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
pub(super) fn default_limit() -> usize {
    25
}
pub(super) fn page<T: Serialize>(
    items: Vec<T>,
    offset: usize,
    limit: usize,
) -> Result<Value, ToolError> {
    if !(1..=100).contains(&limit) {
        return Err(ToolError::Validation("limit must be 1–100".into()));
    }
    let total = items.len();
    let items = items
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let next = offset.saturating_add(items.len());
    Ok(json!({"items":items,"total":total,"next_offset":(next<total).then_some(next)}))
}
fn matches_query<T: Serialize>(item: &T, query: &Option<String>) -> bool {
    query.as_ref().is_none_or(|q| {
        serde_json::to_string(item).is_ok_and(|s| s.to_lowercase().contains(&q.to_lowercase()))
    })
}
impl PrinterService {
    pub(super) async fn materials(&self, p: MaterialsParams) -> Result<Value, ToolError> {
        if !(1..=100).contains(&p.limit) {
            return Err(ToolError::Validation("limit must be 1–100".into()));
        }
        let mut result = match p.scope {
            MaterialScope::Loaded => {
                let loaded = self
                    .loaded_materials(
                        p.printer_id.ok_or_else(|| {
                            ToolError::Validation("loaded materials require printer_id".into())
                        })?,
                        true,
                    )
                    .await?;
                let mut result = page(
                    loaded
                        .materials
                        .into_iter()
                        .filter(|x| {
                            matches_query(x, &p.query)
                                && p.spool_id.is_none_or(|id| {
                                    x.detail
                                        .as_ref()
                                        .and_then(|d| d.inventory.as_ref())
                                        .and_then(|i| i.spool_id)
                                        == Some(id)
                                })
                        })
                        .collect(),
                    p.offset,
                    p.limit,
                )?;
                result["sources"] = serde_json::to_value(loaded.sources)?;
                result["printer_id"] = json!(loaded.status.id);
                result["connected"] = json!(loaded.status.connected);
                result["last_ams_update"] = json!(loaded.status.last_ams_update);
                result
            }
            MaterialScope::Inventory => page(
                self.read
                    .spools(p.include_archived)
                    .await?
                    .into_iter()
                    .filter(|s| {
                        p.spool_id.is_none_or(|id| id == s.id) && matches_query(s, &p.query)
                    })
                    .collect(),
                p.offset,
                p.limit,
            )?,
            MaterialScope::Catalog => page(
                self.read
                    .filament_names()
                    .await?
                    .into_iter()
                    .filter(|x| matches_query(x, &p.query))
                    .collect(),
                p.offset,
                p.limit,
            )?,
            MaterialScope::Presets => {
                let presets = self.read.filament_presets().await?;
                let mut result = page(
                    presets
                        .local
                        .filament
                        .into_iter()
                        .chain(presets.orca_cloud.filament)
                        .chain(presets.cloud.filament)
                        .chain(presets.standard.filament)
                        .filter(|x| matches_query(x, &p.query))
                        .collect(),
                    p.offset,
                    p.limit,
                )?;
                result["cloud_status"] = json!(presets.cloud_status);
                result["orca_cloud_status"] = json!(presets.orca_cloud_status);
                result
            }
        };
        result["scope"] = serde_json::to_value(p.scope)?;
        result["fetched_at_unix_ms"] = json!(super::observations::now_ms());
        Ok(result)
    }
}

fn product_name_key(name: &str) -> String {
    let name = name.trim().to_lowercase();
    name.strip_prefix("bambu lab ")
        .or_else(|| name.strip_prefix("bambu "))
        .unwrap_or(&name)
        .to_owned()
}
