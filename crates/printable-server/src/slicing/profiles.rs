use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
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
    /// Explicit changes to Orca settings in this category, as listed by settings discovery.
    /// Retained with the slice.
    #[serde(default)]
    pub overrides: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilamentSelection {
    pub name: String,
    /// Explicit changes to Orca filament settings, as listed by settings discovery.
    /// Retained with the slice.
    #[serde(default)]
    pub overrides: BTreeMap<String, Value>,
    /// Slot colour as hex RGB or RGBA, such as #0A2CE3; an AMS tray value such as 0A2CE3FF
    /// is also accepted. Omitted colours keep the source 3MF's colour.
    pub colour: Option<String>,
}

impl FilamentSelection {
    pub fn profile(&self) -> ProfileSelection {
        ProfileSelection {
            name: self.name.clone(),
            overrides: self.overrides.clone(),
        }
    }

    /// The slot colour in Orca's `#RRGGBB` or `#RRGGBBAA` form.
    pub fn colour(&self) -> Result<Option<String>, ToolError> {
        self.colour
            .as_deref()
            .map(|value| {
                let hex = value.strip_prefix('#').unwrap_or(value);
                if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(invalid(
                        "filament colour must be hex RGB or RGBA, such as #0A2CE3",
                    ));
                }
                Ok(format!("#{}", hex.to_ascii_uppercase()))
            })
            .transpose()
    }
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
    #[schemars(range(min = 1))]
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
    /// Optional selected printer for engine-specific tower applicability.
    pub printer: Option<ProfileSelection>,
    /// Filter native setting names, such as temperature, support or fan.
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1))]
    pub limit: usize,
}

/// Settings that identify a profile or declare what it is compatible with.
const IDENTITY: &[&str] = &[
    "name",
    "inherits",
    "type",
    "from",
    "printer_model",
    "printer_variant",
    "printer_technology",
    "nozzle_diameter",
    "compatible_printers",
    "compatible_printers_condition",
    "compatible_prints",
    "compatible_prints_condition",
];

/// Settings that callers cannot use to configure host files, printer connections,
/// post-processing programs, or OrcaSlicer plugins.
const UNAVAILABLE: &[&str] = &[
    "bbl_use_printhost",
    "bed_custom_model",
    "bed_custom_texture",
    "filament_plugin_config_overrides",
    "host_type",
    "plugins",
    "post_process",
    "print_host",
    "print_host_webui",
    "print_plugin_config_overrides",
    "printer_agent",
    "printer_plugin_config_overrides",
    "printhost_apikey",
    "printhost_authorization_type",
    "printhost_cafile",
    "printhost_password",
    "printhost_port",
    "printhost_ssl_ignore_revoke",
    "printhost_user",
    "slicing_pipeline_plugin",
];

fn refusal(key: &str) -> Option<&'static str> {
    if IDENTITY.contains(&key) {
        Some("profile identity and compatibility require selecting the matching profile")
    } else if matches!(
        key,
        "bed_exclude_volume_mode" | "bed_exclude_volumes" | "extruder_bed_exclude_volumes"
    ) {
        Some("3D bed exclusion volumes require native printer configuration")
    } else if UNAVAILABLE.contains(&key) {
        Some(
            "printer connection, post-processing, host file and plugin settings are not available for slicing",
        )
    } else {
        None
    }
}

/// Orca's own setting names for each profile category, generated from the packaged
/// release by scripts/orca_settings.py.
const ORCA_SETTINGS: &str = include_str!("orca-settings.json");

#[derive(Deserialize)]
struct OrcaSettings {
    engine_version: String,
    source_revision: String,
    printer: BTreeSet<String>,
    process: BTreeSet<String>,
    filament: BTreeSet<String>,
    definitions: BTreeMap<String, super::definitions::Definition>,
}

impl OrcaSettings {
    fn bundled() -> Result<Self, ToolError> {
        let settings: Self = serde_json::from_str(ORCA_SETTINGS)?;
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<(), ToolError> {
        if self.engine_version != super::ENGINE_VERSION
            || self.source_revision != super::ENGINE_SOURCE_REVISION
        {
            return Err(invalid(
                "bundled Orca definitions do not match the packaged engine identity",
            ));
        }
        if self
            .printer
            .iter()
            .chain(&self.process)
            .chain(&self.filament)
            .any(|key| !self.definitions.contains_key(key))
        {
            return Err(invalid("bundled Orca setting definitions are incomplete"));
        }
        Ok(())
    }
}

pub struct Profiles {
    entries: BTreeMap<Category, BTreeMap<String, Map<String, Value>>>,
    /// Shared filament profiles used to resolve inheritance.
    library: BTreeMap<String, Map<String, Value>>,
    shared_selections: BTreeSet<String>,
    keys: BTreeMap<Category, BTreeSet<String>>,
    definitions: BTreeMap<String, super::definitions::Definition>,
}

fn json_files(directory: &Path) -> Result<Vec<PathBuf>, ToolError> {
    let mut pending = vec![directory.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if entry.path().extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if !file_type.is_file() {
                return Err(invalid("bundled profile must be a regular JSON file"));
            }
            files.push(entry.path());
        }
    }
    Ok(files)
}

fn load_category(
    directory: &Path,
    category: Category,
) -> Result<BTreeMap<String, Map<String, Value>>, ToolError> {
    let mut entries = BTreeMap::new();
    for path in json_files(directory)? {
        let profile: Map<String, Value> = serde_json::from_slice(&std::fs::read(&path)?)?;
        if profile.get("type").and_then(Value::as_str) != Some(category.directory()) {
            continue;
        }
        let Some(name) = profile.get("name").and_then(Value::as_str) else {
            // Orca places these non-preset resources beside its BBL filament presets.
            if category == Category::Filament
                && matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(
                        "filament_id_map.json"
                            | "filament_name_map.json"
                            | "support_recommended_params.json"
                    )
                )
            {
                continue;
            }
            return Err(invalid("bundled profile has no name"));
        };
        if entries.insert(name.to_owned(), profile).is_some() {
            return Err(invalid("bundled profile names are ambiguous"));
        }
    }
    Ok(entries)
}

impl Profiles {
    pub(super) fn native_numeric_default(&self, key: &str) -> Result<Value, ToolError> {
        let definition = self
            .definitions
            .get(key)
            .ok_or_else(|| invalid("packaged Orca setting definition is missing"))?;
        let number = definition
            .default
            .as_ref()
            .and_then(|default| default.value.as_number())
            .ok_or_else(|| invalid("packaged Orca numeric default is missing"))?;
        let value = json!(number.to_string());
        definition.validate(key, &value)?;
        Ok(value)
    }

    pub fn counts(&self) -> crate::worker_health::ProfileCounts {
        crate::worker_health::ProfileCounts {
            printer: self.entries[&Category::Printer].len(),
            process: self.entries[&Category::Process].len(),
            filament: self.entries[&Category::Filament].len(),
        }
    }

    pub fn settings(&self, query: SettingsQuery) -> Result<Value, ToolError> {
        if query.limit == 0 {
            return Err(invalid("settings discovery requires a positive limit"));
        }
        let (resolved, sources) = self.resolve_with_sources(query.category, &query.profile)?;
        let printer = query
            .printer
            .as_ref()
            .map(|p| self.resolve(Category::Printer, p))
            .transpose()?;
        let mut tower = super::tower::describe(
            if query.category == Category::Printer {
                Some(&resolved)
            } else {
                printer.as_ref()
            },
            (query.category == Category::Process).then_some(&resolved),
            &self.definitions,
        );
        tower["context_origin"] = json!("resolved_selected_profiles");
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
            .map(|key| json!({"key":key,"value":resolved.get(key),
                "value_source":if query.profile.overrides.contains_key(key) {json!({"kind":"override"})}
                    else if let Some(profile) = sources.get(key) {json!({"kind":"profile","profile":profile,"inherited":profile != &query.profile.name})}
                    else {json!({"kind":"unspecified"})},
                "definition":if UNAVAILABLE.contains(&key.as_str()) {None} else {self.definitions.get(key)},
                "applicability":tower["controls"].get(key).cloned().unwrap_or_else(||json!({"state":"unknown","reason":"job-specific applicability has not been verified"})),
                "overrideable":refusal(key).is_none()}))
            .collect();
        let next = query.offset.saturating_add(settings.len());
        Ok(
            json!({"profile":query.profile.name,"category":query.category,"requested_limit":query.limit,"returned":settings.len(),"settings":settings,"total":total,
                "next_offset":(next < total).then_some(next),
                "value_format":"Orca strings or nonempty string arrays; null means this profile does not specify a value",
                "catalog_identity":{"engine":"OrcaSlicer","version":super::ENGINE_VERSION,"source_revision":super::ENGINE_SOURCE_REVISION,"runtime_source_verified":false},
                "tower_context":tower,
                "definition_limits":"Only literal source facts are reported. Absent bounds and units are undeclared; unknown_fields identifies unresolved expressions. Native defaults do not replace profile values. Applicability requires the selected hardware, material assignments and effective native output; runtime probing establishes version compatibility, not source identity.",
                "build_plates":super::setup::BuildPlate::ALL.iter().map(|plate| json!({"name":plate,"temperature_setting":plate.temperature_key(),"initial_layer_temperature_setting":format!("{}_initial_layer",plate.temperature_key())})).collect::<Vec<_>>()
            }),
        )
    }

    /// Loads a vendor profile directory, including its subfolders. Filament parents the
    /// vendor lacks are read from Orca's shared library beside it, when present.
    /// Shared presets that explicitly list a vendor printer are selectable too.
    pub fn load(root: &Path) -> Result<Self, ToolError> {
        let mut entries = BTreeMap::new();
        for category in [Category::Printer, Category::Process, Category::Filament] {
            let category_entries = load_category(&root.join(category.directory()), category)?;
            if category_entries.is_empty() {
                return Err(invalid("bundled profile category is empty"));
            }
            entries.insert(category, category_entries);
        }
        let library = match root.parent().map(|parent| {
            parent
                .join("OrcaFilamentLibrary")
                .join(Category::Filament.directory())
        }) {
            Some(directory) if directory.is_dir() => load_category(&directory, Category::Filament)?,
            _ => BTreeMap::new(),
        };
        let printers = &entries[&Category::Printer];
        let shared_selections = library.iter().filter(|(_, profile)| {
            profile.get("instantiation").and_then(Value::as_str) == Some("true")
                && profile
                    .get("compatible_printers")
                    .and_then(Value::as_array)
                    .is_some_and(|names| {
                        names.iter().filter_map(Value::as_str).any(|name| {
                            printers.get(name).is_some_and(|printer| {
                                printer.get("instantiation").and_then(Value::as_str) == Some("true")
                            })
                        })
                    })
        });
        let shared_selections: Vec<_> = shared_selections.collect();
        let filaments = entries
            .get_mut(&Category::Filament)
            .expect("filament category");
        let mut imported = BTreeSet::new();
        for (name, profile) in shared_selections {
            if let std::collections::btree_map::Entry::Vacant(entry) = filaments.entry(name.clone())
            {
                entry.insert(profile.clone());
                imported.insert(name.clone());
            }
        }
        let settings = OrcaSettings::bundled()?;
        let keys = BTreeMap::from([
            (Category::Printer, settings.printer),
            (Category::Process, settings.process),
            (Category::Filament, settings.filament),
        ]);
        Ok(Self {
            entries,
            library,
            shared_selections: imported,
            keys,
            definitions: settings.definitions,
        })
    }

    pub fn resolve(
        &self,
        category: Category,
        selection: &ProfileSelection,
    ) -> Result<Value, ToolError> {
        self.resolve_with_sources(category, selection)
            .map(|(value, _)| value)
    }

    pub(super) fn native_tower_context(&self, settings: Option<&Value>) -> Value {
        let mut context = super::tower::describe(settings, settings, &self.definitions);
        context["context_origin"] = json!("final_native_package_project_settings");
        context
    }

    fn resolve_with_sources(
        &self,
        category: Category,
        selection: &ProfileSelection,
    ) -> Result<(Value, BTreeMap<String, String>), ToolError> {
        let entries = &self.entries[&category];
        let shared_selection =
            category == Category::Filament && self.shared_selections.contains(&selection.name);
        let mut chain = Vec::new();
        let mut name = selection.name.as_str();
        let mut seen = BTreeSet::new();
        loop {
            if !seen.insert(name.to_owned()) {
                return Err(invalid("profile inheritance is cyclic"));
            }
            let inherited = !chain.is_empty() && category == Category::Filament;
            let vendor = entries.get(name);
            let shared = inherited.then(|| self.library.get(name)).flatten();
            let profile = if shared_selection {
                shared.or(vendor)
            } else {
                vendor.or(shared)
            }
            .ok_or_else(|| {
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
        let mut sources = BTreeMap::new();
        for profile in chain.into_iter().rev() {
            let source = profile.get("name").and_then(Value::as_str).unwrap_or("");
            for key in profile.keys() {
                sources.insert(key.clone(), source.to_owned());
            }
            result.extend(profile.clone());
        }
        result.remove("inherits");
        for (key, value) in &selection.overrides {
            if let Some(reason) = refusal(key) {
                return Err(invalid(reason));
            }
            if key == "filament_colour" {
                return Err(invalid(
                    "filament_colour is not a profile setting; set colour on the filament selection",
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
            // Keep the legacy boolean spelling accepted by Orca's profile loader.
            let value = match (key.as_str(), value.as_str()) {
                ("enable_arc_fitting", Some(text))
                    if text == "1" || text.eq_ignore_ascii_case("true") =>
                {
                    json!("enabled")
                }
                ("enable_arc_fitting", Some(text))
                    if text == "0" || text.eq_ignore_ascii_case("false") =>
                {
                    json!("disabled")
                }
                _ => value.clone(),
            };
            if let Some(definition) = self.definitions.get(key) {
                definition.validate(key, &value)?;
            }
            result.insert(key.clone(), value);
        }
        result.insert("from".into(), json!("system"));
        Ok((Value::Object(result), sources))
    }

    pub fn discover(&self, query: ProfileQuery) -> Result<Value, ToolError> {
        if query.limit == 0 {
            return Err(invalid("profile discovery requires a positive limit"));
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
        Ok(
            json!({"profiles":items,"total":total,"requested_limit":query.limit,"returned":items.len(),"next_offset":(next < total).then_some(next)}),
        )
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
    fn large_native_profiles_and_long_queries_remain_discoverable() {
        let root = tempfile::tempdir().unwrap();
        for category in [Category::Printer, Category::Process, Category::Filament] {
            std::fs::create_dir(root.path().join(category.directory())).unwrap();
            std::fs::write(
                root.path().join(category.directory()).join("profile.json"),
                serde_json::to_vec(
                    &json!({"name":"fixture","type":category.directory(),"instantiation":"true"}),
                )
                .unwrap(),
            )
            .unwrap();
        }
        let name = "p".repeat(257);
        let profile = json!({"name":name,"type":"process","instantiation":"true","notes":"x".repeat(1024 * 1024 + 1)});
        std::fs::write(
            root.path().join("process/large.json"),
            serde_json::to_vec(&profile).unwrap(),
        )
        .unwrap();
        let profiles = Profiles::load(root.path()).unwrap();
        let query = serde_json::from_value(json!({"category":"process","query":name})).unwrap();
        assert_eq!(profiles.discover(query).unwrap()["returned"], 1);
    }

    #[test]
    fn profile_and_setting_pages_honor_positive_counts_and_defaults() {
        let entries = (0..301)
            .map(|i| {
                let name = format!("profile-{i:03}");
                let value =
                    serde_json::from_value(json!({"name":name,"instantiation":"true"})).unwrap();
                (name, value)
            })
            .collect();
        let profiles = Profiles {
            entries: BTreeMap::from([(Category::Process, entries)]),
            library: BTreeMap::new(),
            shared_selections: BTreeSet::new(),
            keys: BTreeMap::from([(
                Category::Process,
                (0..301).map(|i| format!("setting-{i:03}")).collect(),
            )]),
            definitions: BTreeMap::new(),
        };
        for (limit, offset, returned, next) in [
            (None, 0, 25, Some(25)),
            (Some(200), 0, 200, Some(200)),
            (Some(usize::MAX), 0, 301, None),
            (Some(200), 200, 101, None),
        ] {
            for settings in [false, true] {
                let mut input = json!({"category":"process","offset":offset});
                if let Some(limit) = limit {
                    input["limit"] = json!(limit);
                }
                let (result, schema) = if settings {
                    input["profile"] = json!({"name":"profile-000"});
                    (
                        profiles
                            .settings(serde_json::from_value(input.clone()).unwrap())
                            .unwrap(),
                        serde_json::to_value(schemars::schema_for!(SettingsQuery)).unwrap(),
                    )
                } else {
                    (
                        profiles
                            .discover(serde_json::from_value(input.clone()).unwrap())
                            .unwrap(),
                        serde_json::to_value(schemars::schema_for!(ProfileQuery)).unwrap(),
                    )
                };
                let validator = jsonschema::validator_for(&schema).unwrap();
                assert!(validator.is_valid(&input));
                assert_eq!(
                    result[if settings { "settings" } else { "profiles" }]
                        .as_array()
                        .unwrap()
                        .len(),
                    returned
                );
                assert_eq!(result["requested_limit"], limit.unwrap_or(25));
                assert_eq!(result["returned"], returned);
                assert_eq!(result["next_offset"], json!(next));
                input["limit"] = json!(0);
                assert!(!validator.is_valid(&input));
                let invalid = if settings {
                    profiles.settings(serde_json::from_value(input).unwrap())
                } else {
                    profiles.discover(serde_json::from_value(input).unwrap())
                };
                assert!(invalid.is_err());
            }
        }
    }

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
            library: BTreeMap::new(),
            shared_selections: BTreeSet::new(),
            keys: BTreeMap::from([(Category::Process, BTreeSet::from(["wall_loops".into()]))]),
            definitions: BTreeMap::new(),
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

    #[test]
    fn bundled_setting_names_match_the_packaged_engine_and_block_unsafe_settings() {
        let settings = OrcaSettings::bundled().unwrap();
        assert_eq!(settings.engine_version, super::super::ENGINE_VERSION);
        let all: BTreeSet<_> = settings
            .printer
            .iter()
            .chain(&settings.process)
            .chain(&settings.filament)
            .collect();
        assert!(!settings.printer.is_empty() && !settings.process.is_empty());
        assert!(!settings.filament.is_empty());
        for key in UNAVAILABLE {
            assert!(
                all.contains(&key.to_string()),
                "{key} is not an Orca setting"
            );
        }
        for key in all {
            if [
                "host",
                "password",
                "apikey",
                "cafile",
                "agent",
                "post_process",
                "bed_custom",
                "plugin",
            ]
            .iter()
            .any(|part| key.contains(part))
            {
                assert!(
                    UNAVAILABLE.contains(&key.as_str()),
                    "{key} needs a blocking decision"
                );
            }
        }
    }

    #[test]
    fn catalog_rejects_mismatched_identity_and_missing_definitions() {
        let mut settings = OrcaSettings::bundled().unwrap();
        let notices = include_str!("../../../../THIRD_PARTY_NOTICES.md");
        assert!(notices.contains(&format!("/tree/{}", settings.source_revision)));
        settings.source_revision = "0".repeat(40);
        assert!(settings.validate().is_err());
        settings.source_revision = super::super::ENGINE_SOURCE_REVISION.into();
        settings.engine_version = "future".into();
        assert!(settings.validate().is_err());
        settings.engine_version = super::super::ENGINE_VERSION.into();
        settings.definitions.remove("prime_tower_brim_width");
        assert!(settings.validate().is_err());
    }

    #[test]
    fn discovery_keeps_inherited_values_overrides_and_native_defaults_distinct() {
        let root = tempfile::tempdir().unwrap();
        for (directory, name) in [
            ("machine", "printer"),
            ("process", "base"),
            ("filament", "filament"),
        ] {
            write_profile(
                &root.path().join(directory).join("base.json"),
                json!({"name":name,"type":directory,"prime_tower_width":"55"}),
            );
        }
        write_profile(
            &root.path().join("process/child.json"),
            json!({"name":"child","type":"process","inherits":"base","layer_height":"0.15"}),
        );
        let profiles = Profiles::load(root.path()).unwrap();
        let query = |name, overrides| {
            serde_json::from_value(json!({
                "category":"process","profile":{"name":"child","overrides":overrides},"query":name
            }))
            .unwrap()
        };
        let width = profiles
            .settings(query("prime_tower_width", json!({})))
            .unwrap();
        assert_eq!(width["settings"][0]["value"], "55");
        assert_eq!(
            width["settings"][0]["value_source"],
            json!({"kind":"profile","profile":"base","inherited":true})
        );
        let width = profiles
            .settings(query(
                "prime_tower_width",
                json!({"prime_tower_width":"70"}),
            ))
            .unwrap();
        assert_eq!(width["settings"][0]["value"], "70");
        assert_eq!(width["settings"][0]["value_source"]["kind"], "override");
        assert_eq!(width["settings"][0]["definition"]["default"]["value"], 60);
        assert_eq!(width["catalog_identity"]["runtime_source_verified"], false);
        let brim = profiles
            .settings(query("prime_tower_brim_width", json!({})))
            .unwrap();
        assert!(brim["settings"][0]["value"].is_null());
        assert_eq!(brim["settings"][0]["value_source"]["kind"], "unspecified");
        assert_eq!(brim["settings"][0]["definition"]["default"]["value"], 3);
        assert_eq!(
            brim["settings"][0]["definition"]["applicability"],
            "unknown"
        );
        assert!(
            profiles
                .settings(query("prime_tower_width", json!({"prime_tower_width":"1"})))
                .is_err()
        );
        for key in ["max_travel_detour_distance", "outer_wall_line_width"] {
            let overrides = serde_json::to_value(BTreeMap::from([(key, "-1")])).unwrap();
            let error = profiles
                .settings(query(key, overrides))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("violates the packaged Orca setting definition"),
                "{error}"
            );
        }
        assert_eq!(
            profiles
                .settings(query("unknown_newer_feature", json!({})))
                .unwrap()["total"],
            0
        );
    }

    fn write_profile(path: &Path, profile: Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&profile).unwrap()).unwrap();
    }

    #[test]
    fn loads_subfolders_and_resolves_parents_from_the_shared_filament_library() {
        let root = tempfile::tempdir().unwrap();
        let vendor = root.path().join("BBL");
        let library = root.path().join("OrcaFilamentLibrary/filament");
        write_profile(
            &vendor.join("machine/printer.json"),
            json!({"name":"printer","type":"machine","instantiation":"true"}),
        );
        write_profile(
            &vendor.join("process/process.json"),
            json!({"name":"process","type":"process","instantiation":"true"}),
        );
        write_profile(
            &vendor.join("filament/common.json"),
            json!({"name":"fdm_filament_common","type":"filament","filament_type":["PLA"]}),
        );
        write_profile(
            &vendor.join("filament/filament_id_map.json"),
            json!({"type":"filament","GF01":"Bambu PLA Basic"}),
        );
        write_profile(
            &vendor.join("filament/Vendor/child.json"),
            json!({"name":"Vendor PLA @BBL X1C","type":"filament","inherits":"Vendor PLA @base",
                "instantiation":"true","compatible_printers":["printer"]}),
        );
        write_profile(
            &library.join("Vendor/base.json"),
            json!({"name":"Vendor PLA @base","type":"filament","inherits":"fdm_filament_common",
                "nozzle_temperature":["215"]}),
        );
        write_profile(
            &library.join("common.json"),
            json!({"name":"fdm_filament_common","type":"filament","filament_type":["PETG"]}),
        );
        write_profile(
            &library.join("generic.json"),
            json!({"name":"Generic PLA @System","type":"filament","instantiation":"true"}),
        );
        write_profile(
            &library.join("Vendor/selectable.json"),
            json!({"name":"Shared PETG @printer","type":"filament","inherits":"Vendor PLA @base",
                "compatible_printers":["printer"],"instantiation":"true"}),
        );
        let profiles = Profiles::load(&vendor).unwrap();
        let child = ProfileSelection {
            name: "Vendor PLA @BBL X1C".into(),
            overrides: BTreeMap::new(),
        };
        let resolved = profiles.resolve(Category::Filament, &child).unwrap();
        assert_eq!(resolved["nozzle_temperature"], json!(["215"]));
        assert_eq!(resolved["filament_type"], json!(["PLA"]));
        let found = profiles
            .discover(
                serde_json::from_value(
                    json!({"category":"filament","query":"@","printer":"printer"}),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(found["total"], 2);
        assert_eq!(found["profiles"][0]["name"], "Shared PETG @printer");
        assert_eq!(found["profiles"][0]["filament_type"], json!(["PETG"]));
        assert_eq!(found["profiles"][1]["name"], "Vendor PLA @BBL X1C");
        let shared = profiles
            .resolve(
                Category::Filament,
                &ProfileSelection {
                    name: "Shared PETG @printer".into(),
                    overrides: BTreeMap::new(),
                },
            )
            .unwrap();
        assert_eq!(shared["nozzle_temperature"], json!(["215"]));
        assert_eq!(shared["filament_type"], json!(["PETG"]));
        for name in ["Vendor PLA @base", "Generic PLA @System"] {
            let selection = ProfileSelection {
                name: name.into(),
                overrides: BTreeMap::new(),
            };
            assert!(profiles.resolve(Category::Filament, &selection).is_err());
        }
        write_profile(
            &vendor.join("filament/broken.json"),
            json!({"type":"filament"}),
        );
        assert!(Profiles::load(&vendor).is_err());
    }

    #[test]
    fn overrides_accept_orca_settings_and_refuse_unsafe_or_unknown_names() {
        let settings = OrcaSettings::bundled().unwrap();
        let profile = |name: &str| {
            BTreeMap::from([(
                name.to_owned(),
                serde_json::from_value(json!({"name":name})).unwrap(),
            )])
        };
        let profiles = Profiles {
            entries: BTreeMap::from([
                (Category::Printer, profile("printer")),
                (Category::Process, profile("process")),
                (Category::Filament, profile("filament")),
            ]),
            library: BTreeMap::new(),
            shared_selections: BTreeSet::new(),
            keys: BTreeMap::from([
                (Category::Printer, settings.printer),
                (Category::Process, settings.process),
                (Category::Filament, settings.filament),
            ]),
            definitions: settings.definitions,
        };
        let resolve = |category, key: &str, value: Value| {
            let name = match category {
                Category::Printer => "printer",
                Category::Process => "process",
                Category::Filament => "filament",
            };
            profiles.resolve(
                category,
                &ProfileSelection {
                    name: name.into(),
                    overrides: BTreeMap::from([(key.to_owned(), value)]),
                },
            )
        };
        let brim = resolve(Category::Process, "brim_type", json!("no_brim")).unwrap();
        assert_eq!(brim["brim_type"], "no_brim");
        for (value, expected) in [
            ("0", "disabled"),
            ("FALSE", "disabled"),
            ("1", "enabled"),
            ("true", "enabled"),
            ("sparse_infill", "sparse_infill"),
        ] {
            let profile = resolve(Category::Process, "enable_arc_fitting", json!(value)).unwrap();
            assert_eq!(profile["enable_arc_fitting"], expected);
        }
        let advance = resolve(Category::Filament, "pressure_advance", json!(["0.03"])).unwrap();
        assert_eq!(advance["pressure_advance"], json!(["0.03"]));
        for (category, key, message) in [
            (
                Category::Process,
                "wall_infill_order",
                "unknown process override",
            ),
            (Category::Process, "post_process", "not available"),
            (
                Category::Process,
                "slicing_pipeline_plugin",
                "not available",
            ),
            (Category::Process, "plugins", "not available"),
            (
                Category::Printer,
                "printer_plugin_config_overrides",
                "not available",
            ),
            (
                Category::Filament,
                "filament_plugin_config_overrides",
                "not available",
            ),
            (Category::Printer, "printhost_apikey", "not available"),
            (
                Category::Printer,
                "bed_exclude_volumes",
                "native printer configuration",
            ),
            (Category::Printer, "printer_technology", "identity"),
            (
                Category::Filament,
                "compatible_prints_condition",
                "identity",
            ),
            (Category::Filament, "filament_colour", "set colour"),
        ] {
            let error = resolve(category, key, json!(["x"]))
                .unwrap_err()
                .to_string();
            assert!(error.contains(message), "{key}: {error}");
        }
        for (key, overrideable) in [("brim_type", true), ("post_process", false)] {
            let query = serde_json::from_value(
                json!({"category":"process","profile":{"name":"process"},"query":key}),
            )
            .unwrap();
            let discovered = profiles.settings(query).unwrap();
            let row = discovered["settings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["key"] == key)
                .unwrap();
            assert_eq!(row["overrideable"], overrideable);
            assert!(row["value"].is_null());
        }
    }

    #[test]
    fn filament_colours_normalize_hex_and_reject_other_text() {
        let selection = |colour: Option<&str>| FilamentSelection {
            name: "filament".into(),
            overrides: BTreeMap::new(),
            colour: colour.map(str::to_owned),
        };
        for (input, expected) in [
            (None, None),
            (Some("#0a2ce3"), Some("#0A2CE3")),
            (Some("0A2CE3FF"), Some("#0A2CE3FF")),
        ] {
            assert_eq!(
                selection(input).colour().unwrap().as_deref(),
                expected,
                "{input:?}"
            );
        }
        for input in ["", "blue", "#12345", "#GGGGGG", "#0A2CE3 ", "##0A2CE3"] {
            assert!(selection(Some(input)).colour().is_err(), "{input}");
        }
    }
}
