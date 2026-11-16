//! Engine-specific generator evidence. This describes source-selected algorithms,
//! not physical compatibility or proof that a job generated a tower.

use super::definitions::Definition;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) fn describe(
    printer: Option<&Value>,
    process: Option<&Value>,
    definitions: &BTreeMap<String, Definition>,
) -> Value {
    let value = |profile: Option<&Value>, key: &str| {
        if let Some(value) = profile.and_then(|p| p.get(key)) {
            json!({"value":value,"source":"selected_context"})
        } else if let Some(default) = definitions.get(key).and_then(|d| d.default.as_ref()) {
            json!({"value":default.value,"source":"native_definition"})
        } else {
            json!({"value":null,"source":"unknown"})
        }
    };
    let selected_type = value(printer, "wipe_tower_type");
    let wall = value(process, "wipe_tower_wall_type");
    let printer_identity = printer.and_then(|p| {
        p["printer_model"]
            .as_str()
            .filter(|v| !v.is_empty())
            .or_else(|| p["name"].as_str())
    });
    let algorithm = match printer_identity {
        Some(name) if name.starts_with("Bambu Lab") => Some("type1"),
        Some(_) => selected_type["value"]
            .as_str()
            .filter(|v| matches!(*v, "type1" | "type2")),
        None => None,
    };
    let mut controls = BTreeMap::new();
    for key in [
        "wipe_tower_type",
        "wipe_tower_wall_type",
        "wipe_tower_cone_angle",
        "wipe_tower_rotation_angle",
        "prime_tower_width",
        "prime_tower_brim_width",
        "prime_tower_enable_framework",
        "wipe_tower_no_sparse_layers",
    ] {
        let (state, reason) = match (algorithm, key) {
            (None, _) => (
                "unknown",
                "selected printer identity and tower implementation are required",
            ),
            (Some("type1"), "wipe_tower_type")
                if printer_identity.is_some_and(|v| v.starts_with("Bambu Lab")) =>
            {
                (
                    "inapplicable",
                    "the packaged BBL path forces Type 1 regardless of this configured value",
                )
            }
            (Some("type1"), "wipe_tower_cone_angle") => (
                "inapplicable",
                "Type 1 does not generate the stabilization cone; planning bounds are separate",
            ),
            (Some("type2"), "wipe_tower_cone_angle") if process.is_none() => {
                ("unknown", "selected process wall type is required")
            }
            (Some("type2"), "wipe_tower_cone_angle") if wall["value"] != "cone" => {
                if wall["value"].is_null() {
                    ("unknown", "wall type is unknown")
                } else {
                    (
                        "inapplicable",
                        "Type 2 cone generation requires the cone wall type",
                    )
                }
            }
            (Some("type2"), "prime_tower_enable_framework") => (
                "inapplicable",
                "this framework control is consumed by Type 1, not the Type 2 generator",
            ),
            (Some("type1"), "wipe_tower_wall_type") if wall["value"] == "cone" => (
                "inapplicable",
                "Type 1 distinguishes rib from non-rib walls; selecting cone does not enable a cone",
            ),
            (Some(_), "prime_tower_width") if wall["value"] == "rib" => (
                "supported",
                "rib planning derives the final width; the configured width is not a measured output width",
            ),
            (Some(_), "wipe_tower_rotation_angle") => (
                "supported",
                "shared G-code integration rotates both Type 1 and Type 2 tower paths",
            ),
            _ => (
                "supported",
                "the selected packaged generator consumes this control; final effect requires output evidence",
            ),
        };
        let resolved = value(
            if key == "wipe_tower_type" {
                printer
            } else {
                process
            },
            key,
        );
        controls.insert(
            key,
            json!({"state":state,"reason":reason,"configured":resolved,
            "output_effect":"unverified_until_generated_paths_are_compared"}),
        );
    }
    let base = format!(
        "https://github.com/chrisbennight/OrcaSlicer/blob/{}",
        super::ENGINE_SOURCE_REVISION
    );
    json!({"scope":"packaged_tower_generator","algorithm":algorithm,"wall":wall,
        "engine":{"version":super::ENGINE_VERSION,"source_revision":super::ENGINE_SOURCE_REVISION,"runtime_source_verified":false},
        "controls":controls,"physical_compatibility":"unknown",
        "not_exposed":["native 3D collision volumes","combined sparse tower layers"],
        "basis":[format!("{base}/src/OrcaSlicer.cpp"),format!("{base}/src/libslic3r/Print.hpp"),
            format!("{base}/src/libslic3r/GCode.hpp"),format!("{base}/src/libslic3r/GCode.cpp"),
            format!("{base}/src/libslic3r/GCode/WipeTower.cpp"),format!("{base}/src/libslic3r/GCode/WipeTower2.cpp")],
        "limits":["Source consumption is distinct from a generated tower or a measured parameter effect.",
            "Imported project/object settings and material changes can alter effective output; inspect retained final geometry and toolpaths.",
            "No physical adhesion, bonding or hardware clearance is established."]})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bbl_rotation_cone_and_rib_width_follow_the_pinned_implementation() {
        let printer = json!({"name":"software fixture","printer_model":"Bambu Lab X1 Carbon","wipe_tower_type":"type2"});
        let process = json!({"wipe_tower_wall_type":"rib"});
        let result = describe(Some(&printer), Some(&process), &BTreeMap::new());
        assert_eq!(result["algorithm"], "type1");
        for key in ["wipe_tower_type", "wipe_tower_cone_angle"] {
            assert_eq!(result["controls"][key]["state"], "inapplicable");
        }
        assert_eq!(
            result["controls"]["wipe_tower_rotation_angle"]["state"],
            "supported"
        );
        assert_eq!(
            result["controls"]["prime_tower_width"]["configured"]["value"],
            Value::Null
        );
        let unknown = describe(None, Some(&process), &BTreeMap::new());
        assert_eq!(
            unknown["controls"]["wipe_tower_rotation_angle"]["state"],
            "unknown"
        );
        let printer = json!({"name":"selected non-BBL printer","wipe_tower_type":"type2"});
        let rectangle = json!({"wipe_tower_wall_type":"rectangle"});
        assert_eq!(
            describe(Some(&printer), Some(&rectangle), &BTreeMap::new())["controls"]["wipe_tower_cone_angle"]
                ["state"],
            "inapplicable"
        );
        let cone = json!({"wipe_tower_wall_type":"cone"});
        assert_eq!(
            describe(Some(&printer), Some(&cone), &BTreeMap::new())["controls"]["wipe_tower_cone_angle"]
                ["state"],
            "supported"
        );
    }
}
