//! On-demand contracts derived from the same schemas used for tool discovery.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::tools::{output, workflows};

pub const ROOT: &str = "printable://contracts";

fn action_name(variant: &Value) -> Option<&str> {
    let action = &variant["properties"]["action"];
    action["const"].as_str().or_else(|| {
        let values = action["enum"].as_array()?;
        (values.len() == 1).then(|| values[0].as_str()).flatten()
    })
}

fn variants(schema: &Value) -> &[Value] {
    schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn references(value: &Value, names: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(name) = object
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|reference| reference.strip_prefix("#/$defs/"))
            {
                names.insert(name.to_string());
            }
            for child in object.values() {
                references(child, names);
            }
        }
        Value::Array(values) => {
            for child in values {
                references(child, names);
            }
        }
        _ => {}
    }
}

fn selected_schema(schema: &Value, action: &str) -> Option<Value> {
    let mut selected = variants(schema)
        .iter()
        .find(|variant| action_name(variant) == Some(action))?
        .clone();
    let mut pending = BTreeSet::new();
    let mut definitions = Map::new();
    references(&selected, &mut pending);
    while let Some(name) = pending.pop_first() {
        if definitions.contains_key(&name) {
            continue;
        }
        let definition = schema.get("$defs")?.get(&name)?.clone();
        references(&definition, &mut pending);
        definitions.insert(name, definition);
    }
    if !definitions.is_empty() {
        selected["$defs"] = Value::Object(definitions);
    }
    if let Some(dialect) = schema.get("$schema") {
        selected["$schema"] = dialect.clone();
    }
    Some(selected)
}

/// Return only server-owned schemas; URI segments never access the filesystem.
pub fn read(uri: &str) -> Option<Value> {
    if uri == ROOT {
        let tools: Vec<_> = workflows::TOOLS
            .iter()
            .map(|tool| {
                let schema = Value::Object((tool.schema)().as_ref().clone());
                let actions: Vec<_> = variants(&schema).iter().filter_map(action_name).collect();
                json!({"tool": tool.name, "actions": actions,
                    "uri": format!("{ROOT}/{}", tool.name)})
            })
            .collect();
        return Some(json!({"tools": tools,
            "action_uri": "printable://contracts/{tool}/{action}",
            "note": "Read one action contract when available. These are discovery resources, not execution or authorization endpoints."}));
    }
    let mut segments = uri.strip_prefix(&format!("{ROOT}/"))?.split('/');
    let name = segments.next()?;
    let action = segments.next();
    if segments.next().is_some() || action == Some("") {
        return None;
    }
    let tool = workflows::lookup(name)?;
    let full_schema = Value::Object((tool.schema)().as_ref().clone());
    let input = match action {
        Some(action) => selected_schema(&full_schema, action)?,
        None => full_schema,
    };
    let selected_output =
        action.and_then(|action| crate::printers::action_output_schema(name, action));
    let output_scope = if selected_output.is_some() {
        "action"
    } else {
        "tool"
    };
    let output_schema =
        selected_output.unwrap_or_else(|| Value::Object(output::schema(name).as_ref().clone()));
    Some(json!({
        "tool": name,
        "action": action,
        "description": tool.description,
        "inputSchema": input,
        "outputSchema": output_schema,
        "output_schema_scope": output_scope,
        "annotations": (tool.annotations)(),
        "note": "Invoke the advertised tool with this input. Tool-wide annotations are not an action authorization boundary. output_schema_scope identifies the output selection; failures may set isError."
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_action_has_a_self_contained_contract() {
        let index = read(ROOT).unwrap();
        for entry in index["tools"].as_array().unwrap() {
            let uri = entry["uri"].as_str().unwrap();
            assert!(read(uri).is_some());
            for action in entry["actions"].as_array().unwrap() {
                let contract = read(&format!("{uri}/{}", action.as_str().unwrap())).unwrap();
                jsonschema::validator_for(&contract["inputSchema"]).unwrap();
                jsonschema::validator_for(&contract["outputSchema"]).unwrap();
                if matches!(entry["tool"].as_str(), Some("printer" | "print")) {
                    assert_eq!(contract["output_schema_scope"], "action");
                }
            }
        }
        let render = read("printable://contracts/render/product").unwrap();
        let validator = jsonschema::validator_for(&render["inputSchema"]).unwrap();
        assert!(validator.is_valid(&json!({"action":"product","params":{
            "path":"render.png","objects":["OrganicForm"],
            "presentation":{"profile":"studio_neutral"}
        }})));
        assert!(!validator.is_valid(&json!({"action":"scene","params":{"path":"render.png"}})));
        assert!(
            !render["inputSchema"]["$defs"]
                .as_object()
                .unwrap()
                .contains_key("RenderGalleryParams")
        );
    }

    #[test]
    fn contract_lookup_rejects_unknown_or_ambiguous_paths() {
        for uri in [
            "printable://contracts/",
            "printable://contracts/nope",
            "printable://contracts/render/",
            "printable://contracts/render/nope",
            "printable://contracts/render/product/extra",
            "printable://contracts/../render",
            "printable://contracts/status/product",
        ] {
            assert!(read(uri).is_none(), "accepted {uri}");
        }
    }
}
