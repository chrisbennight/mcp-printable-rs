use crate::worker_health::CapabilityState;
use serde::Serialize;

/// The MCP server name. Shared by the `/healthz` `server` field and the MCP
/// `serverInfo.name`, so a client and an operator see the same identity.
pub const SERVER_NAME: &str = "printable_blender";

/// `/healthz` body: unauthenticated process liveness only — backend readiness
/// is the `printable_status` tool's job, not this route's. Exactly two fields,
/// `status` then `server`, serialized compactly; the exact wire bytes are
/// asserted in the endpoint tests.
#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub server: &'static str,
}

impl HealthResponse {
    pub fn ok() -> Self {
        Self {
            status: "ok",
            server: SERVER_NAME,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct ReadinessChecks {
    pub blender: bool,
    pub render_worker: bool,
    pub workspace: bool,
    pub openscad: bool,
    pub ffmpeg: bool,
    pub durable_recovery: bool,
    pub cad: CapabilityState,
    pub slicer: CapabilityState,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct ReadinessWork {
    pub queued: bool,
    pub running: bool,
}

/// Sanitized dependency readiness for an unauthenticated operator probe.
///
/// The response deliberately contains only stable capability states and booleans:
/// no backend address, binary path, workspace path, or diagnostic string may
/// cross this boundary.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReadinessResponse {
    pub status: &'static str,
    pub checks: ReadinessChecks,
    pub work: ReadinessWork,
    pub codes: Vec<&'static str>,
}

impl ReadinessResponse {
    pub(crate) fn new(checks: ReadinessChecks, work: ReadinessWork) -> Self {
        let mut codes = Vec::new();
        if !checks.blender {
            codes.push("blender_unavailable");
        }
        if !checks.render_worker {
            codes.push("render_worker_unavailable");
        }
        if !checks.workspace {
            codes.push("workspace_unavailable");
        }
        if !checks.openscad {
            codes.push("openscad_unavailable");
        }
        if !checks.ffmpeg {
            codes.push("ffmpeg_unavailable");
        }
        if !checks.durable_recovery {
            codes.push("durable_recovery_fenced");
        }
        match checks.cad {
            CapabilityState::Unavailable => codes.push("cad_unavailable"),
            CapabilityState::Incompatible => codes.push("cad_incompatible"),
            _ => {}
        }
        match checks.slicer {
            CapabilityState::Unavailable => codes.push("slicer_unavailable"),
            CapabilityState::Incompatible => codes.push("slicer_incompatible"),
            _ => {}
        }

        let status = if !codes.is_empty() {
            "blocked"
        } else if work.queued
            || work.running
            || [checks.cad, checks.slicer].contains(&CapabilityState::Busy)
        {
            codes.push("work_in_progress");
            "busy"
        } else {
            "ready"
        };

        Self {
            status,
            checks,
            work,
            codes,
        }
    }

    pub(crate) fn available(&self) -> bool {
        self.status != "blocked"
    }
}

#[cfg(test)]
mod tests {
    use super::{ReadinessChecks, ReadinessResponse, ReadinessWork};
    use crate::worker_health::CapabilityState;

    #[test]
    fn readiness_distinguishes_ready_busy_and_blocked_without_diagnostics() {
        let healthy = ReadinessChecks {
            blender: true,
            render_worker: true,
            workspace: true,
            openscad: true,
            ffmpeg: true,
            durable_recovery: true,
            cad: CapabilityState::NotConfigured,
            slicer: CapabilityState::NotConfigured,
        };

        let ready = ReadinessResponse::new(
            healthy,
            ReadinessWork {
                queued: false,
                running: false,
            },
        );
        assert_eq!(ready.status, "ready");
        assert!(ready.codes.is_empty());
        assert!(ready.available());

        let native_busy = ReadinessResponse::new(
            ReadinessChecks {
                cad: CapabilityState::Ready,
                slicer: CapabilityState::Busy,
                ..healthy
            },
            ReadinessWork {
                queued: false,
                running: false,
            },
        );
        assert_eq!(native_busy.status, "busy");
        assert!(native_busy.available());
        let native_blocked = ReadinessResponse::new(
            ReadinessChecks {
                cad: CapabilityState::Unavailable,
                slicer: CapabilityState::Ready,
                ..healthy
            },
            ReadinessWork {
                queued: false,
                running: false,
            },
        );
        assert_eq!(native_blocked.status, "blocked");
        assert_eq!(native_blocked.codes, ["cad_unavailable"]);
        assert_eq!(native_blocked.checks.slicer, CapabilityState::Ready);

        let busy = ReadinessResponse::new(
            healthy,
            ReadinessWork {
                queued: true,
                running: false,
            },
        );
        assert_eq!(busy.status, "busy");
        assert_eq!(busy.codes, ["work_in_progress"]);
        assert!(busy.available());

        let blocked = ReadinessResponse::new(
            ReadinessChecks {
                blender: false,
                ffmpeg: false,
                ..healthy
            },
            ReadinessWork {
                queued: true,
                running: true,
            },
        );
        assert_eq!(blocked.status, "blocked");
        assert_eq!(blocked.codes, ["blender_unavailable", "ffmpeg_unavailable"]);
        assert!(!blocked.available());

        let serialized = serde_json::to_value(blocked).expect("serialize readiness");
        assert_eq!(
            serialized,
            serde_json::json!({
                "status": "blocked",
                "checks": {
                    "blender": false,
                    "render_worker": true,
                    "workspace": true,
                    "openscad": true,
                    "ffmpeg": false,
                    "durable_recovery": true,
                    "cad": "not_configured",
                    "slicer": "not_configured"
                },
                "work": {
                    "queued": true,
                    "running": true
                },
                "codes": ["blender_unavailable", "ffmpeg_unavailable"]
            })
        );
    }
}
