//! Cooperative bounds for short-lived slice observations and their file copies.
use crate::error::ToolError;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct ProcessingLimit {
    pub deadline: Instant,
    pub cancellation: CancellationToken,
    pub operation: &'static str,
}

impl ProcessingLimit {
    pub fn check(&self) -> Result<(), ToolError> {
        if self.cancellation.is_cancelled() {
            Err(super::slice_error(&format!("{} cancelled", self.operation)))
        } else if Instant::now() >= self.deadline {
            Err(super::slice_error(&format!(
                "{} exceeded its processing deadline",
                self.operation
            )))
        } else {
            Ok(())
        }
    }

    pub fn check_io(&self) -> std::io::Result<()> {
        self.check().map_err(std::io::Error::other)
    }
}

pub(super) fn copy_and_hash(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    limit: &ProcessingLimit,
) -> std::io::Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        limit.check_io()?;
        let count = reader.read(&mut buffer)?;
        limit.check_io()?;
        if count == 0 {
            break;
        }
        writer.write_all(&buffer[..count])?;
        limit.check_io()?;
        digest.update(&buffer[..count]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
