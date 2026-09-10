use crate::deadline::Phase;

/// Errors from the Blender bridge client.
///
/// `Addon` carries the user-facing failure reported by the add-on. Backend
/// tracebacks are discarded at the protocol boundary because they may contain
/// internal paths or scene data.
#[derive(Debug, thiserror::Error)]
pub enum BlenderError {
    #[error(
        "Cannot connect to Blender on {host}:{port}. Make sure Blender is running with the Printable Bridge addon enabled."
    )]
    Connect {
        host: String,
        port: u16,
        #[source]
        source: std::io::Error,
    },
    #[error("Timed out {phase}")]
    Timeout { phase: Phase },
    #[error("Blender response frame exceeds the {max} byte limit")]
    FrameTooLarge { max: usize },
    #[error("Blender protocol error: {0}")]
    Protocol(String),
    #[error("Blender mutations are fenced until durable session recovery completes")]
    RecoveryFenced,
    #[error("A Blender readiness probe is already in progress")]
    ReadinessProbeInProgress,
    #[error("Blender error: {message}")]
    Addon {
        message: String,
        scene_state: Option<crate::SceneSnapshot>,
    },
    #[error("The expected scene is stale; inspect the current scene before retrying")]
    StaleSceneState { observed: crate::SceneSnapshot },
    #[error("expected_scene requires a canonical generation UUID and non-negative revision")]
    InvalidSceneState { observed: crate::SceneSnapshot },
    #[error("Blender I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

impl BlenderError {
    pub fn code(&self) -> &'static str {
        match self {
            BlenderError::Connect { .. } => "connect",
            BlenderError::Timeout { .. } => "timeout",
            BlenderError::FrameTooLarge { .. } => "frame_too_large",
            BlenderError::Protocol(_) => "protocol",
            BlenderError::RecoveryFenced => "recovery_fenced",
            BlenderError::ReadinessProbeInProgress => "readiness_probe_in_progress",
            BlenderError::Addon { .. } => "addon",
            BlenderError::StaleSceneState { .. } => "stale_scene_state",
            BlenderError::InvalidSceneState { .. } => "invalid_scene_state",
            BlenderError::Io(_) => "io",
        }
    }

    pub fn scene_state(&self) -> Option<&crate::SceneSnapshot> {
        match self {
            Self::Addon { scene_state, .. } => scene_state.as_ref(),
            Self::StaleSceneState { observed } | Self::InvalidSceneState { observed } => {
                Some(observed)
            }
            _ => None,
        }
    }
}
