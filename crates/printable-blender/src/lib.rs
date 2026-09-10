//! Client for the Blender addon bridge.
//!
//! Speaks the addon wire protocol (see `PROTOCOL.md`): 4-byte big-endian
//! length-prefixed UTF-8 JSON frames, one fresh TCP connection per command,
//! exchanges serialized process-wide because Blender executes commands on its
//! single main thread. Carries the addon/server version handshake and a
//! per-phase deadline budget, and retries exactly once when the connection
//! cannot be established (never after a request has been written).

mod client;
mod deadline;
mod error;
mod protocol;
mod version;

/// Wire-compatibility version required by this client and reported by the add-on.
pub const BRIDGE_PROTOCOL_VERSION: &str = "0.5.0";

pub use client::{
    BlenderClient, ClientOptions, Connector, DEFAULT_BUDGET, DEFAULT_MAX_FRAME_LENGTH, Transaction,
};
pub use deadline::{Deadline, Phase};
pub use error::BlenderError;
pub use protocol::{Params, Request, Response, SceneSnapshot};
pub use version::{VersionState, format_version_mismatch};

#[cfg(any(test, feature = "fake-addon"))]
pub mod fake_addon;
