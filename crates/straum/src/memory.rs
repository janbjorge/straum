//! The `--max-memory` watch: Straum's resident memory, read from Linux's
//! `/proc/self/status`, against the limit. `resident_bytes` reads the
//! `VmRSS` line of the file's text, `read_resident` reads the file, and
//! `watch` reads it every second, records each reading for `/status`,
//! until memory passes the limit or the file can't be read, and says why. `main.rs` reads once at the start,
//! so a limit that can't be read fails there, then runs `watch` on a
//! thread of its own and ends the process with what it returns.

use std::fmt;
use std::fs;
use std::io;
use std::thread;
use std::time::Duration;

use straum_app::{Memory, MemoryReadings, format_mebibytes};

/// Where Linux says how much memory the process holds. std has no call
/// for it, and this needs no `unsafe` and no page size.
pub(crate) const STATUS: &str = "/proc/self/status";

/// How often the watch reads resident memory.
const READ_EVERY: Duration = Duration::from_secs(1);

/// The line of the status that holds resident memory, in kB.
const RESIDENT: &str = "VmRSS:";

/// Linux's "kB" in the status is a kibibyte.
const KIB: &str = "kB";
const BYTES_PER_KIB: u64 = 1 << 10;

/// Why resident memory could not be read from a status.
#[derive(Debug)]
pub(crate) enum ReadError {
    File(io::Error),
    NoResident,
    /// The `VmRSS` line, as written.
    Malformed(String),
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::File(error) => error.fmt(formatter),
            ReadError::NoResident => formatter.write_str("it has no VmRSS line"),
            ReadError::Malformed(line) => write!(formatter, "{line:?} is not a size in {KIB}"),
        }
    }
}

/// Why the memory watch ends Straum, or fails the start.
#[derive(Debug)]
pub(crate) enum MemoryError {
    Unreadable { path: String, error: ReadError },
    PastLimit { held: u64, limit: u64 },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryError::Unreadable { path, error } => write!(
                formatter,
                "--max-memory needs {path} to read Straum's resident memory: {error}"
            ),
            MemoryError::PastLimit { held, limit } => write!(
                formatter,
                "Straum holds {} of resident memory, past the --max-memory limit of {}",
                format_mebibytes(*held),
                format_mebibytes(*limit)
            ),
        }
    }
}

/// The resident memory a `/proc/<pid>/status` text gives, in bytes.
pub(crate) fn resident_bytes(status: &str) -> Result<u64, ReadError> {
    for line in status.lines() {
        let Some(value) = line.strip_prefix(RESIDENT) else {
            continue;
        };
        let Some(kib) = value.trim().strip_suffix(KIB) else {
            return Err(ReadError::Malformed(line.to_owned()));
        };
        let Ok(kib) = kib.trim().parse::<u64>() else {
            return Err(ReadError::Malformed(line.to_owned()));
        };
        let Some(bytes) = kib.checked_mul(BYTES_PER_KIB) else {
            return Err(ReadError::Malformed(line.to_owned()));
        };
        return Ok(bytes);
    }
    Err(ReadError::NoResident)
}

/// The resident memory the status file at `path` gives, in bytes.
pub(crate) fn read_resident(path: &str) -> Result<u64, MemoryError> {
    let read = match fs::read_to_string(path) {
        Ok(text) => resident_bytes(&text),
        Err(error) => Err(ReadError::File(error)),
    };
    match read {
        Ok(bytes) => Ok(bytes),
        Err(error) => {
            let path = path.to_owned();
            Err(MemoryError::Unreadable { path, error })
        }
    }
}

/// Whether `held` bytes are past the `limit`.
pub(crate) fn check(held: u64, limit: u64) -> Result<(), MemoryError> {
    if held > limit {
        return Err(MemoryError::PastLimit { held, limit });
    }
    Ok(())
}

/// Reads resident memory at `path` every second, recording each reading
/// in `readings`, until it passes `limit` or can't be read, and returns
/// why it stopped. An unreadable file ends the watch too: a limit nothing
/// checks would pass for one that holds.
pub(crate) fn watch(path: &str, limit: u64, readings: &MemoryReadings) -> MemoryError {
    loop {
        let held = match read_resident(path) {
            Ok(held) => held,
            Err(error) => return error,
        };
        readings.record(Memory {
            resident_bytes: held,
            limit_bytes: limit,
        });
        if let Err(stop) = check(held, limit) {
            return stop;
        }
        thread::sleep(READ_EVERY);
    }
}

#[cfg(test)]
mod tests;
