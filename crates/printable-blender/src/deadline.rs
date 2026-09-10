use std::fmt;
use std::time::{Duration, Instant};

use crate::error::BlenderError;

/// A phase of one command exchange. Used for deadline accounting and to name
/// the phase a timeout occurred in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Acquiring the process-wide serialization lock.
    Lock,
    /// Establishing the TCP connection.
    Connect,
    /// Writing the request frame.
    Send,
    /// Reading the response frame.
    Recv,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Phase::Lock => "acquiring the Blender lock",
            Phase::Connect => "connecting to Blender",
            Phase::Send => "sending to Blender",
            Phase::Recv => "waiting for Blender",
        };
        f.write_str(s)
    }
}

/// A total time budget for one command, from which the remaining time is
/// derived before each phase so a stall in any phase fails within budget rather
/// than hanging. A single deadline is shared across all phases (and, for a
/// transaction, across all its exchanges).
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    start: Instant,
    budget: Duration,
}

impl Deadline {
    /// A deadline `budget` from now. A non-positive budget yields a deadline
    /// that is already expired.
    pub fn new(budget: Duration) -> Self {
        Deadline {
            start: Instant::now(),
            budget,
        }
    }

    /// Remaining time before `phase`, or a `Timeout{phase}` error if the budget
    /// is already spent.
    pub fn remaining(&self, phase: Phase) -> Result<Duration, BlenderError> {
        let elapsed = self.start.elapsed();
        if elapsed >= self.budget {
            return Err(BlenderError::Timeout { phase });
        }
        Ok(self.budget - elapsed)
    }
}
