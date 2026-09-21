//! Printable MCP server: streamable HTTP at `/mcp`, `/healthz` liveness,
//! `/readyz` dependency readiness, and a
//! hand-rolled [`ServerHandler`](mcp::PrintableServer) in the house style.
//!
//! This crate hosts the MCP server: settings from the environment, the axum
//! transport wiring, the resource catalog, and the typed workspace and Blender
//! modeling/file tool catalog.

pub mod cad;
pub mod config;
pub mod error;
pub mod file_ingest;
pub mod file_transfer;
pub mod health;
pub mod jobs;
pub mod mcp;
pub mod projects;
pub mod resources;
pub mod server;
pub mod slicing;
pub mod tools;
pub mod upload;
