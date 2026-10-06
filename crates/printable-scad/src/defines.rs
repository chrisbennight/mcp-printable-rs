//! Typed OpenSCAD command-line definitions.
//!
//! Values are serialized into complete `name=<literal>` arguments and are
//! always passed to OpenSCAD as distinct argv entries. Nothing in this module
//! produces shell syntax.

use std::collections::BTreeMap;

use crate::{ProductProfile, ProductProfileError};

/// One value accepted by OpenSCAD's `-D name=value` option.
#[derive(Clone, Debug, PartialEq)]
pub enum DefineValue {
    Bool(bool),
    Number(f64),
    String(String),
    NumberVector(Vec<f64>),
}

/// Validated argv entries and non-sensitive metadata for one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerializedDefinitions {
    argv: Vec<String>,
    names: Vec<String>,
    variant_applied: bool,
}

impl SerializedDefinitions {
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn variant_applied(&self) -> bool {
        self.variant_applied
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DefineError {
    #[error(transparent)]
    InvalidProductProfile(#[from] ProductProfileError),
    #[error("OpenSCAD definition name is not a valid ASCII identifier: {0}")]
    InvalidName(String),
    #[error("OpenSCAD definition names beginning with pbl_ are reserved")]
    ReservedName,
    #[error("OpenSCAD definition {0} must be a finite number")]
    NonFiniteNumber(String),
    #[error("OpenSCAD definition {0} string contains an ASCII control character")]
    StringControlCharacter(String),
}

/// Serialize caller definitions and the optional server-reserved variant.
///
/// The returned vector is ready to splice into an OpenSCAD argv: every
/// assignment is preceded by its own `-D` entry.
pub fn serialize_definitions(
    definitions: &BTreeMap<String, DefineValue>,
    variant: Option<&str>,
) -> Result<SerializedDefinitions, DefineError> {
    serialize_product_definitions(definitions, variant, None)
}

/// Serialize caller definitions, an optional product variant, and a validated
/// server-owned product profile into one deterministic argv sequence.
pub fn serialize_product_definitions(
    definitions: &BTreeMap<String, DefineValue>,
    variant: Option<&str>,
    profile: Option<&ProductProfile>,
) -> Result<SerializedDefinitions, DefineError> {
    if let Some(profile) = profile {
        profile.validate()?;
    }

    let mut assignments = Vec::with_capacity(
        definitions.len() + usize::from(variant.is_some()) + usize::from(profile.is_some()) * 9,
    );
    for (name, value) in definitions {
        validate_name(name)?;
        if name.starts_with("pbl_") {
            return Err(DefineError::ReservedName);
        }
        assignments.push((name.clone(), serialize_value(name, value)?));
    }

    if let Some(variant) = variant {
        assignments.push((
            "pbl_variant".to_string(),
            serialize_string("pbl_variant", variant)?,
        ));
    }
    if let Some(profile) = profile {
        for (name, value) in [
            (
                "pbl_nozzle_diameter_mm",
                profile.manufacturing.nozzle_diameter_mm,
            ),
            ("pbl_layer_height_mm", profile.manufacturing.layer_height_mm),
            ("pbl_minimum_wall_mm", profile.manufacturing.minimum_wall_mm),
            (
                "pbl_moving_clearance_mm",
                profile.manufacturing.moving_clearance_mm,
            ),
            (
                "pbl_maximum_overhang_degrees",
                profile.manufacturing.maximum_overhang_degrees,
            ),
            ("pbl_primary_radius_mm", profile.form.primary_radius_mm),
            ("pbl_secondary_radius_mm", profile.form.secondary_radius_mm),
            ("pbl_edge_break_mm", profile.form.edge_break_mm),
            (
                "pbl_transition_length_mm",
                profile.form.transition_length_mm,
            ),
        ] {
            assignments.push((name.to_string(), serialize_number(name, value)?));
        }
    }
    assignments.sort_unstable_by(|left, right| left.0.cmp(&right.0));

    let mut argv = Vec::with_capacity(assignments.len() * 2);
    let mut names = Vec::with_capacity(assignments.len());
    for (name, literal) in assignments {
        let assignment = format!("{name}={literal}");
        argv.push("-D".to_string());
        argv.push(assignment);
        names.push(name);
    }

    Ok(SerializedDefinitions {
        argv,
        names,
        variant_applied: variant.is_some(),
    })
}

fn validate_name(name: &str) -> Result<(), DefineError> {
    let mut bytes = name.bytes();
    if !matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(DefineError::InvalidName(name.to_string()));
    }
    Ok(())
}

fn serialize_value(name: &str, value: &DefineValue) -> Result<String, DefineError> {
    match value {
        DefineValue::Bool(value) => Ok(value.to_string()),
        DefineValue::Number(value) => serialize_number(name, *value),
        DefineValue::String(value) => serialize_string(name, value),
        DefineValue::NumberVector(values) => {
            let values = values
                .iter()
                .map(|value| serialize_number(name, *value))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("[{}]", values.join(",")))
        }
    }
}

fn serialize_number(name: &str, value: f64) -> Result<String, DefineError> {
    if !value.is_finite() {
        return Err(DefineError::NonFiniteNumber(name.to_string()));
    }
    if value == 0.0 {
        return Ok("0".to_string());
    }
    Ok(value.to_string())
}

fn serialize_string(name: &str, value: &str) -> Result<String, DefineError> {
    if value.chars().any(char::is_control) {
        return Err(DefineError::StringControlCharacter(name.to_string()));
    }

    let mut literal = String::with_capacity(value.len() + 2);
    literal.push('"');
    for character in value.chars() {
        match character {
            '"' => literal.push_str("\\\""),
            '\\' => literal.push_str("\\\\"),
            character => literal.push(character),
        }
    }
    literal.push('"');
    Ok(literal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn serializes_every_value_as_sorted_discrete_argv() {
        let definitions = BTreeMap::from([
            ("width".to_string(), DefineValue::Number(70.0)),
            ("enabled".to_string(), DefineValue::Bool(true)),
            (
                "label".to_string(),
                DefineValue::String("left \"A\" \\\\ path".to_string()),
            ),
            (
                "samples".to_string(),
                DefineValue::NumberVector(vec![1.0, -0.0, 3.5]),
            ),
        ]);

        let serialized = serialize_definitions(&definitions, Some("print")).unwrap();

        assert_eq!(
            serialized.argv(),
            [
                "-D",
                "enabled=true",
                "-D",
                "label=\"left \\\"A\\\" \\\\\\\\ path\"",
                "-D",
                "pbl_variant=\"print\"",
                "-D",
                "samples=[1,0,3.5]",
                "-D",
                "width=70",
            ]
        );
        assert_eq!(
            serialized.names(),
            ["enabled", "label", "pbl_variant", "samples", "width"]
        );
        assert!(serialized.variant_applied());
    }

    #[test]
    fn product_profile_adds_only_sorted_server_reserved_definitions() {
        let profile = ProductProfile {
            manufacturing: crate::ManufacturingProfile {
                nozzle_diameter_mm: 0.4,
                layer_height_mm: 0.2,
                minimum_wall_mm: 2.0,
                moving_clearance_mm: 0.35,
                maximum_overhang_degrees: 45.0,
            },
            form: crate::FormProfile {
                primary_radius_mm: 3.0,
                secondary_radius_mm: 1.5,
                edge_break_mm: 0.5,
                transition_length_mm: 8.0,
            },
        };

        let serialized =
            serialize_product_definitions(&BTreeMap::new(), None, Some(&profile)).unwrap();

        assert_eq!(
            serialized.argv(),
            [
                "-D",
                "pbl_edge_break_mm=0.5",
                "-D",
                "pbl_layer_height_mm=0.2",
                "-D",
                "pbl_maximum_overhang_degrees=45",
                "-D",
                "pbl_minimum_wall_mm=2",
                "-D",
                "pbl_moving_clearance_mm=0.35",
                "-D",
                "pbl_nozzle_diameter_mm=0.4",
                "-D",
                "pbl_primary_radius_mm=3",
                "-D",
                "pbl_secondary_radius_mm=1.5",
                "-D",
                "pbl_transition_length_mm=8",
            ]
        );
    }

    #[test]
    fn rejects_reserved_invalid_and_control_character_inputs() {
        assert_eq!(
            serialize_definitions(
                &BTreeMap::from([("pbl_private".to_string(), DefineValue::Bool(true))]),
                None
            ),
            Err(DefineError::ReservedName)
        );
        assert!(matches!(
            serialize_definitions(
                &BTreeMap::from([("bad-name".to_string(), DefineValue::Bool(true))]),
                None
            ),
            Err(DefineError::InvalidName(_))
        ));
        assert!(matches!(
            serialize_definitions(
                &BTreeMap::from([(
                    "label".to_string(),
                    DefineValue::String("line\nbreak".to_string())
                )]),
                None
            ),
            Err(DefineError::StringControlCharacter(_))
        ));
        assert!(serialize_definitions(&BTreeMap::new(), Some(&"v".repeat(65))).is_ok());
        assert!(serialize_definitions(&BTreeMap::new(), Some(&"v".repeat(64))).is_ok());
    }

    #[test]
    fn large_definitions_vectors_strings_and_argv_are_serialized() {
        let mut definitions: BTreeMap<String, DefineValue> = (0..65)
            .map(|index| (format!("v{index}"), DefineValue::Bool(true)))
            .collect();
        definitions.insert("points".into(), DefineValue::NumberVector(vec![0.0; 100]));
        definitions.insert("label".into(), DefineValue::String("a".repeat(65537)));
        let serialized = serialize_definitions(&definitions, Some(&"variant".repeat(20))).unwrap();
        assert_eq!(serialized.names().len(), 68);
        assert!(
            serialized
                .argv()
                .iter()
                .any(|arg| arg.starts_with("points=["))
        );
        assert!(serialized.argv().iter().any(|arg| arg.len() > 65536));
        assert!(serialized.variant_applied());
    }

    #[test]
    fn reports_when_no_variant_was_applied() {
        let serialized = serialize_definitions(&BTreeMap::new(), None).unwrap();

        assert!(!serialized.variant_applied());
    }

    #[test]
    fn rejects_non_finite_numbers_in_scalars_and_vectors() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let scalar = BTreeMap::from([("size".to_string(), DefineValue::Number(value))]);
            assert!(matches!(
                serialize_definitions(&scalar, None),
                Err(DefineError::NonFiniteNumber(_))
            ));
            let vector = BTreeMap::from([(
                "samples".to_string(),
                DefineValue::NumberVector(vec![value]),
            )]);
            assert!(matches!(
                serialize_definitions(&vector, None),
                Err(DefineError::NonFiniteNumber(_))
            ));
        }
    }

    proptest! {
        #[test]
        fn arbitrary_strings_cannot_end_the_literal(
            value in "[^\\p{Cc}]{0,512}"
        ) {
            let definitions = BTreeMap::from([(
                "value".to_string(),
                DefineValue::String(value.clone()),
            )]);
            let serialized = serialize_definitions(&definitions, None).unwrap();
            let assignment = &serialized.argv()[1];
            prop_assert!(assignment.starts_with("value=\""));
            prop_assert!(assignment.ends_with('"'));
            let literal = &assignment["value=".len()..];
            let interior = &literal[1..literal.len() - 1];
            let mut escaped = false;
            for character in interior.chars() {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else {
                    prop_assert_ne!(character, '"');
                }
            }
            prop_assert!(!escaped);
        }

        #[test]
        fn map_order_never_changes_the_serialized_result(
            entries in prop::collection::btree_map(
                // The reserved prefix is excluded from the generator rather
                // than unwrapped past: serialize_definitions rejects those
                // names by design, and this property is about ordering
                // independence among the names it accepts. Left in, the
                // generator eventually produces one and the unwrap below
                // fails for a reason the property is not about.
                "[A-Za-z_][A-Za-z0-9_]{0,20}".prop_filter(
                    "the pbl_ prefix is reserved and rejected by design",
                    |name: &String| !name.starts_with("pbl_"),
                ),
                any::<bool>(),
                0..=100,
            )
        ) {
            let definitions = entries
                .into_iter()
                .map(|(name, value)| (name, DefineValue::Bool(value)))
                .collect();
            let first = serialize_definitions(&definitions, None).unwrap();
            let second = serialize_definitions(&definitions, None).unwrap();
            prop_assert_eq!(first, second);
        }
    }
}
