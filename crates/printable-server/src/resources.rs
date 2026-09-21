//! Published `printable://` MCP guidance.
//!
//! A resource enters this catalog only when its body and the capability it
//! explains ship together.

pub mod contracts;

/// One resource's static metadata.
pub struct ResourceDef {
    pub uri: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub mime_type: &'static str,
    pub body: &'static str,
}

/// The resource catalog, in a stable declared order.
pub static RESOURCES: &[ResourceDef] = &[
    ResourceDef {
        uri: "printable://modeling/blender-v1",
        name: "Blender modeling and selective inspection",
        description: "Scripted modeling, concise result examples, filtered inspection, node topology, checkpoints, and visual review.",
        mime_type: "text/markdown",
        body: include_str!("../resources/blender-modeling-v1.md"),
    },
    ResourceDef {
        uri: "printable://design/product-v1",
        name: "Generic FDM product design kit",
        description: "OpenSCAD product_v1 modules, explicit manufacturing/form profiles, aesthetic guidance, and honest manufacturing-evidence boundaries.",
        mime_type: "text/markdown",
        body: include_str!("../resources/product-v1.md"),
    },
    ResourceDef {
        uri: "printable://render/product-v1",
        name: "Product presentation profiles",
        description: "Deterministic engineering and studio rendering profiles, material behavior, bounds framing, and source-scene preservation.",
        mime_type: "text/markdown",
        body: include_str!("../resources/product-render-v1.md"),
    },
    ResourceDef {
        uri: "printable://printing/workflow-v1",
        name: "Printer observation and physical printing",
        description: "Physical setup, typed observations, prepared-file review, staging, control and honest failure recovery.",
        mime_type: "text/markdown",
        body: include_str!("../resources/printing-workflow-v1.md"),
    },
];
