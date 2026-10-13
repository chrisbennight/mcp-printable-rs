//! Printable MCP server: streamable HTTP at `/mcp`, `/healthz` liveness,
//! `/readyz` dependency readiness, and a
//! hand-rolled [`ServerHandler`](mcp::PrintableServer) in the house style.
//!
//! This crate hosts the MCP server: settings from the environment, the axum
//! transport wiring, the resource catalog, and the typed workspace and Blender
//! modeling/file tool catalog.

mod admission;
pub mod cad;
pub mod config;
pub mod error;
pub mod file_ingest;
pub mod file_transfer;
pub mod flame;
pub mod health;
pub mod jobs;
pub mod mcp;
pub mod printers;
pub mod projects;
pub mod provenance;
pub mod recon;
pub mod resources;
pub mod server;
pub mod shutdown;
pub mod slicing;
pub mod tools;
pub mod upload;
pub mod worker_health;
