//! Lenient decoding for values that sites send in more than one shape.

use serde::{Deserialize, Deserializer, de};

use crate::FileKind;

/// Longest identifier accepted from a site.
const MAXIMUM_ID_CHARACTERS: usize = 64;

/// An identifier sent as a JSON string or integer, limited to ASCII letters,
/// digits, `-` and `_` so it is safe in paths, file names and messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Id(pub(crate) String);

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(value)
                if !value.is_empty()
                    && value.len() <= MAXIMUM_ID_CHARACTERS
                    && value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
                    }) =>
            {
                Ok(Self(value))
            }
            serde_json::Value::Number(value) if value.is_u64() || value.is_i64() => {
                Ok(Self(value.to_string()))
            }
            _ => Err(de::Error::custom("expected a string or integer identifier")),
        }
    }
}

/// A non-negative count or size sent as an integer, a numeric string or null.
pub(crate) fn count<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    Ok(
        match Option::<serde_json::Value>::deserialize(deserializer)? {
            Some(serde_json::Value::Number(value)) => value.as_u64(),
            Some(serde_json::Value::String(value)) => value.trim().parse().ok(),
            _ => None,
        },
    )
}

/// A list sent as a JSON array, null or nothing at all; the last two mean
/// empty. Use with `#[serde(default)]` so an absent field also decodes.
pub(crate) fn list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Classify a file by its extension when the site does not say what it is.
pub(crate) fn kind_for_name(name: &str) -> FileKind {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".gcode") || lower.ends_with(".bgcode") || lower.ends_with(".gcode.3mf") {
        return FileKind::Sliced;
    }
    let extension = lower
        .rsplit_once('.')
        .map_or("", |(_, extension)| extension);
    match extension {
        "stl" | "3mf" | "obj" | "step" | "stp" | "scad" | "ply" | "off" | "glb" | "gltf"
        | "blend" | "dxf" | "svg" => FileKind::Model,
        _ => FileKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Sample {
        id: Id,
        #[serde(default, deserialize_with = "count")]
        size: Option<u64>,
    }

    #[test]
    fn identifiers_and_counts_accept_strings_and_numbers() {
        let sample: Sample = serde_json::from_str(r#"{"id": 42, "size": "1024"}"#).unwrap();
        assert_eq!((sample.id.0.as_str(), sample.size), ("42", Some(1024)));
        let sample: Sample = serde_json::from_str(r#"{"id": "US2b", "size": null}"#).unwrap();
        assert_eq!((sample.id.0.as_str(), sample.size), ("US2b", None));
        let sample: Sample = serde_json::from_str(r#"{"id": "7"}"#).unwrap();
        assert_eq!(sample.size, None);
        assert!(serde_json::from_str::<Sample>(r#"{"id": 1.5}"#).is_err());
        assert!(serde_json::from_str::<Sample>(r#"{"id": ""}"#).is_err());
        assert!(serde_json::from_str::<Sample>(r#"{"id": "../x"}"#).is_err());
        let long = format!(r#"{{"id": "{}"}}"#, "a".repeat(65));
        assert!(serde_json::from_str::<Sample>(&long).is_err());
    }

    #[test]
    fn lists_accept_null_and_absence_as_empty() {
        #[derive(Deserialize)]
        struct Listing {
            #[serde(default, deserialize_with = "list")]
            items: Vec<u8>,
        }
        let decode = |text| serde_json::from_str::<Listing>(text).unwrap().items;
        assert_eq!(decode(r#"{"items": [1, 2]}"#), [1, 2]);
        assert!(decode(r#"{"items": null}"#).is_empty());
        assert!(decode("{}").is_empty());
        assert!(serde_json::from_str::<Listing>(r#"{"items": "1"}"#).is_err());
    }

    #[test]
    fn files_are_classified_by_extension() {
        assert_eq!(kind_for_name("Bowl.STL"), FileKind::Model);
        assert_eq!(kind_for_name("plate.gcode.3mf"), FileKind::Sliced);
        assert_eq!(kind_for_name("part.bgcode"), FileKind::Sliced);
        assert_eq!(kind_for_name("instructions.pdf"), FileKind::Other);
        assert_eq!(kind_for_name("README"), FileKind::Other);
    }
}
