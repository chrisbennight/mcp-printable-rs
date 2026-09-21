use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::ToolError;

#[derive(
    Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Printer,
    Process,
    Filament,
}

impl Category {
    fn directory(self) -> &'static str {
        match self {
            Self::Printer => "machine",
            Self::Process => "process",
            Self::Filament => "filament",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileSelection {
    pub name: String,
    /// Explicit changes to known Orca settings in this category. Retained with the slice.
    #[serde(default)]
    pub overrides: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileQuery {
    pub category: Category,
    #[serde(default)]
    pub query: String,
    /// Exact printer profile name to filter declared compatibility.
    pub printer: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    25
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SettingsQuery {
    pub category: Category,
    pub profile: ProfileSelection,
    /// Filter native setting names, such as temperature, support or fan.
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn editable(key: &str) -> bool {
    ![
        "name",
        "inherits",
        "type",
        "from",
        "printer_model",
        "printer_variant",
        "nozzle_diameter",
        "compatible_printers",
    ]
    .contains(&key)
}

pub struct Profiles {
    entries: BTreeMap<Category, BTreeMap<String, Map<String, Value>>>,
    keys: BTreeMap<Category, BTreeSet<String>>,
}

impl Profiles {
    pub fn settings(&self, query: SettingsQuery) -> Result<Value, ToolError> {
        if !(1..=100).contains(&query.limit) || query.query.len() > 256 {
            return Err(invalid(
                "settings discovery requires a limit of 1–100 and a query of at most 256 bytes",
            ));
        }
        let resolved = self.resolve(query.category, &query.profile)?;
        let needle = query.query.to_lowercase();
        let keys: Vec<_> = self.keys[&query.category]
            .iter()
            .filter(|key| key.to_lowercase().contains(&needle))
            .collect();
        let total = keys.len();
        let settings: Vec<_> = keys
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .map(|key| json!({"key":key,"value":resolved.get(key),"overrideable":editable(key)}))
            .collect();
        let next = query.offset.saturating_add(settings.len());
        Ok(
            json!({"profile":query.profile.name,"category":query.category,"settings":settings,"total":total,
                "next_offset":(next < total).then_some(next),
                "value_format":"Orca strings or nonempty string arrays; null means this profile does not specify a value",
                "build_plates":super::setup::BuildPlate::ALL.iter().map(|plate| json!({"name":plate,"temperature_setting":plate.temperature_key(),"initial_layer_temperature_setting":format!("{}_initial_layer",plate.temperature_key())})).collect::<Vec<_>>()
            }),
        )
    }

    pub fn load(root: &Path) -> Result<Self, ToolError> {
        let mut entries = BTreeMap::new();
        let mut keys = BTreeMap::new();
        for category in [Category::Printer, Category::Process, Category::Filament] {
            let mut category_entries = BTreeMap::new();
            let mut category_keys = BTreeSet::new();
            for entry in std::fs::read_dir(root.join(category.directory()))? {
                let entry = entry?;
                if entry.path().extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let metadata = entry.metadata()?;
                if !metadata.is_file() || metadata.len() > 1024 * 1024 {
                    return Err(invalid(
                        "bundled profile must be a regular JSON file below 1 MiB",
                    ));
                }
                let profile: Map<String, Value> =
                    serde_json::from_slice(&std::fs::read(entry.path())?)?;
                if profile.get("type").and_then(Value::as_str) != Some(category.directory()) {
                    continue;
                }
                let name = profile
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("bundled profile has no name"))?
                    .to_owned();
                category_keys.extend(profile.keys().cloned());
                if category_entries.insert(name, profile).is_some() {
                    return Err(invalid("bundled profile names are ambiguous"));
                }
            }
            if category_entries.is_empty() {
                return Err(invalid("bundled profile category is empty"));
            }
            entries.insert(category, category_entries);
            keys.insert(category, category_keys);
        }
        Ok(Self { entries, keys })
    }

    pub fn resolve(
        &self,
        category: Category,
        selection: &ProfileSelection,
    ) -> Result<Value, ToolError> {
        let entries = &self.entries[&category];
        let mut chain = Vec::new();
        let mut name = selection.name.as_str();
        let mut seen = BTreeSet::new();
        loop {
            if !seen.insert(name.to_owned()) || chain.len() >= 32 {
                return Err(invalid("profile inheritance is cyclic or too deep"));
            }
            let profile = entries.get(name).ok_or_else(|| {
                invalid(&format!(
                    "unresolved {} profile: {name}",
                    category.directory()
                ))
            })?;
            chain.push(profile);
            match profile
                .get("inherits")
                .and_then(Value::as_str)
                .filter(|x| !x.is_empty())
            {
                Some(parent) => name = parent,
                None => break,
            }
        }
        let mut result = Map::new();
        for profile in chain.into_iter().rev() {
            result.extend(profile.clone());
        }
        result.remove("inherits");
        for (key, value) in &selection.overrides {
            if !editable(key) {
                return Err(invalid(
                    "profile identity and compatibility require selecting the matching profile",
                ));
            }
            if !self.keys[&category].contains(key) {
                return Err(invalid(&format!(
                    "unknown {} override: {key}",
                    category.directory()
                )));
            }
            if !(value.is_string()
                || value
                    .as_array()
                    .is_some_and(|a| !a.is_empty() && a.iter().all(Value::is_string)))
            {
                return Err(invalid(
                    "Orca profile values must be strings or nonempty string arrays",
                ));
            }
            result.insert(key.clone(), value.clone());
        }
        result.insert("from".into(), json!("system"));
        Ok(Value::Object(result))
    }

    pub fn discover(&self, query: ProfileQuery) -> Result<Value, ToolError> {
        if !(1..=100).contains(&query.limit) || query.query.len() > 256 {
            return Err(invalid(
                "profile discovery limit must be 1–100 and query at most 256 bytes",
            ));
        }
        let needle = query.query.to_lowercase();
        let mut candidates = Vec::new();
        for (name, profile) in &self.entries[&query.category] {
            if profile.get("instantiation").and_then(Value::as_str) != Some("true")
                || !name.to_lowercase().contains(&needle)
            {
                continue;
            }
            let selection = ProfileSelection {
                name: name.clone(),
                overrides: BTreeMap::new(),
            };
            match self.resolve(query.category, &selection) {
                Ok(resolved) => {
                    if let Some(printer) = &query.printer
                        && query.category != Category::Printer
                        && !compatible(&resolved, printer)
                    {
                        continue;
                    }
                    candidates.push(json!({"name":name,"resolvable":true,
                        "printer_model":resolved.get("printer_model"), "nozzle_diameter":resolved.get("nozzle_diameter"),
                        "compatible_printers":resolved.get("compatible_printers"), "filament_type":resolved.get("filament_type")}));
                }
                Err(_) if query.printer.is_none() => {
                    candidates.push(json!({"name":name,"resolvable":false}))
                }
                Err(_) => {}
            }
        }
        let total = candidates.len();
        let items: Vec<_> = candidates
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .collect();
        let next = query.offset.saturating_add(items.len());
        Ok(json!({"profiles":items,"total":total,"next_offset":(next < total).then_some(next)}))
    }
}

pub fn compatible(profile: &Value, printer: &str) -> bool {
    profile
        .get("compatible_printers")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|name| name.as_str() == Some(printer)))
}

fn invalid(message: &str) -> ToolError {
    ToolError::Validation(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_actual_parent_values_and_rejects_missing_parents_and_identity_overrides() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "base".into(),
            serde_json::from_value(json!({"name":"base","layer_height":"0.2","wall_loops":"2"}))
                .unwrap(),
        );
        entries.insert(
            "fine".into(),
            serde_json::from_value(json!({"name":"fine","inherits":"base","layer_height":"0.1"}))
                .unwrap(),
        );
        let profiles = Profiles {
            entries: BTreeMap::from([(Category::Process, entries)]),
            keys: BTreeMap::from([(Category::Process, BTreeSet::from(["wall_loops".into()]))]),
        };
        let mut selection = ProfileSelection {
            name: "fine".into(),
            overrides: BTreeMap::from([("wall_loops".into(), json!("3"))]),
        };
        let resolved = profiles.resolve(Category::Process, &selection).unwrap();
        assert_eq!(resolved["layer_height"], "0.1");
        assert_eq!(resolved["wall_loops"], "3");
        assert!(resolved.get("inherits").is_none());
        selection
            .overrides
            .insert("nozzle_diameter".into(), json!(["0.6"]));
        assert!(profiles.resolve(Category::Process, &selection).is_err());
        selection.name = "missing".into();
        assert!(profiles.resolve(Category::Process, &selection).is_err());
    }
}
