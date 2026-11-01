//! Report discovery payload sizes without invoking a model or a native worker.

use std::borrow::Cow;

use printable_server::{
    resources::contracts,
    tools::{output, workflows},
};
use rmcp::model::Tool;
use serde_json::json;

fn main() {
    let available = workflows::Availability::default();
    let tools: Vec<_> = workflows::advertised(available)
        .map(|meta| {
            Tool::new(
                Cow::Borrowed(meta.name),
                Cow::Borrowed(meta.description),
                (meta.schema)(),
            )
            .annotate((meta.annotations)())
            .with_raw_output_schema(output::schema(meta.name))
        })
        .collect();
    let index = contracts::read(contracts::ROOT, available).expect("contract index");
    let cad = tools
        .iter()
        .find(|tool| tool.name == "cad_build")
        .expect("CAD tool");
    let inspect = contracts::read("printable://contracts/cad_build/inspect", available);
    let export = contracts::read("printable://contracts/cad_build/export", available);
    println!(
        "{}",
        json!({
            "boundary":"compact UTF-8 JSON; default optional-worker availability; excludes transport, instructions and model token usage",
            "full_catalog_json_bytes":json!({"tools":tools}).to_string().len(),
            "cad_tool_json_bytes":serde_json::to_vec(cad).expect("CAD JSON").len(),
            "contract_index_json_bytes":index.to_string().len(),
            "inspect_contract_json_bytes":inspect.map(|value| value.to_string().len()),
            "export_contract_json_bytes":export.map(|value| value.to_string().len()),
            "model_tokens":null
        })
    );
}
