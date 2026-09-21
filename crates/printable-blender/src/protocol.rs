use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

/// The params object of a request. The wire contract defines `params` as a JSON
/// object (`{}` when none), so it is a `Map` rather than an arbitrary `Value` —
/// a non-object params is unrepresentable, not merely discouraged.
pub type Params = Map<String, Value>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub generation: String,
    pub revision: u64,
}

/// A request envelope sent to the addon: `{id, command, params}`.
#[derive(Debug, Clone, Serialize)]
pub struct Request<'a> {
    pub id: String,
    pub command: &'a str,
    pub params: Params,
}

impl<'a> Request<'a> {
    /// Build a request with a fresh correlation id.
    pub fn new(command: &'a str, params: Params) -> Self {
        Request {
            id: Uuid::new_v4().to_string(),
            command,
            params,
        }
    }
}

/// A response envelope from the addon, discriminated by its `status` tag.
///
/// Modelling this as an enum makes the presence rules part of the type: a
/// `Success` **requires** `result`, so a success envelope missing the `result`
/// key fails to deserialize (surfaced as a protocol error), while an explicit
/// `"result": null` is a perfectly valid result value (`Value::Null`) and is
/// accepted — any JSON is a legal result per the wire contract. Symmetrically,
/// `Error` requires `error`, so an error envelope missing the message is a
/// protocol error rather than a nameless addon failure. `id` and
/// `addon_version` and `bridge_instance_id` are common to both variants and are
/// stamped on every response, including errors and timeouts.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Response {
    Success {
        #[serde(default)]
        id: String,
        result: Value,
        #[serde(default)]
        addon_version: Option<String>,
        bridge_instance_id: String,
    },
    Error {
        #[serde(default)]
        id: String,
        error: String,
        #[serde(default)]
        error_code: Option<String>,
        #[serde(default)]
        scene_state: Option<SceneSnapshot>,
        #[serde(default)]
        traceback: Option<String>,
        #[serde(default)]
        addon_version: Option<String>,
        bridge_instance_id: String,
    },
}

impl Response {
    /// The correlation id echoed from the request (present on both variants).
    /// The client verifies it matches the request it sent.
    pub fn id(&self) -> &str {
        match self {
            Response::Success { id, .. } | Response::Error { id, .. } => id,
        }
    }

    /// The addon's `bl_info` version stamped on the response, if present.
    pub fn addon_version(&self) -> Option<&str> {
        match self {
            Response::Success { addon_version, .. } | Response::Error { addon_version, .. } => {
                addon_version.as_deref()
            }
        }
    }

    /// The bridge process identity stamped on the response.
    pub fn bridge_instance_id(&self) -> &str {
        match self {
            Response::Success {
                bridge_instance_id, ..
            }
            | Response::Error {
                bridge_instance_id, ..
            } => bridge_instance_id,
        }
    }
}
