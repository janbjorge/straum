//! The memory watch: Straum's resident memory, read from Linux's
//! `/proc/self/status`, or from `ps` where there is no such file, as on
//! macOS, against the `--max-memory` limit when one is set. A `Resident`
//! says where to read it; `resident_bytes` reads the `VmRSS` line of the
//! file's text, `ps_resident_bytes` the output of `ps`, and `watch` reads
//! it every second and
//! records each reading for `/status`, until memory passes the limit or
//! the file can't be read, and says why. `main.rs` reads once at the
//! start, so a limit that can't be read fails there, then runs `watch` on
//! a thread of its own; past the limit it ends the process.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::process::{self, Command};
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

/// Why resident memory could not be read from a status or from `ps`.
#[derive(Debug)]
pub(crate) enum ReadError {
    File(io::Error),
    NoResident,
    /// The `VmRSS` line, or the output of `ps`, as written.
    Malformed(String),
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::File(error) => error.fmt(formatter),
            ReadError::NoResident => formatter.write_str("it gives no resident size"),
            ReadError::Malformed(line) => write!(formatter, "{line:?} is not a size in {KIB}"),
        }
    }
}

/// Where a process's resident memory is read.
#[derive(Debug)]
pub(crate) enum Resident {
    /// A `/proc/<pid>/status` file, which Linux writes, at `path`. It
    /// stays open and is read again from its start: an open at each read
    /// failed once the connections held every file descriptor, and ended
    /// Straum.
    Status { path: String, file: File },
    /// `ps -o rss= -p PID`, for a system without that file.
    Ps { pid: u32 },
}

impl Resident {
    /// Where this process's memory is read: its status file where Linux
    /// writes one, else `ps`. The image holds no `ps`, but it runs on
    /// Linux.
    pub(crate) fn of_this_process() -> Resident {
        match File::open(STATUS) {
            Ok(file) => Resident::Status {
                path: STATUS.to_owned(),
                file,
            },
            Err(_) => Resident::Ps { pid: process::id() },
        }
    }

    /// The resident memory, in bytes.
    pub(crate) fn read(&self) -> Result<u64, MemoryError> {
        let read = match self {
            Resident::Status { file, .. } => match read_again(file) {
                Ok(text) => resident_bytes(&text),
                Err(error) => Err(ReadError::File(error)),
            },
            Resident::Ps { pid } => read_ps(*pid),
        };
        match read {
            Ok(bytes) => Ok(bytes),
            Err(error) => Err(MemoryError::Unreadable {
                source: self.to_string(),
                error,
            }),
        }
    }
}

impl fmt::Display for Resident {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Resident::Status { path, .. } => formatter.write_str(path),
            Resident::Ps { pid } => write!(formatter, "ps -o rss= -p {pid}"),
        }
    }
}

/// The whole text of `file`, read from its start. Linux writes a status
/// file again for each read from its start.
fn read_again(mut file: &File) -> io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

/// Why the memory watch ends Straum, or fails the start.
#[derive(Debug)]
pub(crate) enum MemoryError {
    /// `source` is where it was read, as a [`Resident`] names it.
    Unreadable {
        source: String,
        error: ReadError,
    },
    PastLimit {
        held: u64,
        limit: u64,
    },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryError::Unreadable { source, error } => write!(
                formatter,
                "--max-memory needs {source} to read Straum's resident memory: {error}"
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

/// The resident memory `ps -o rss=` writes, a size in kB on a line of
/// its own, in bytes. No line means no such process.
pub(crate) fn ps_resident_bytes(output: &str) -> Result<u64, ReadError> {
    let text = output.trim();
    if text.is_empty() {
        return Err(ReadError::NoResident);
    }
    let Ok(kib) = text.parse::<u64>() else {
        return Err(ReadError::Malformed(text.to_owned()));
    };
    let Some(bytes) = kib.checked_mul(BYTES_PER_KIB) else {
        return Err(ReadError::Malformed(text.to_owned()));
    };
    Ok(bytes)
}

fn read_ps(pid: u32) -> Result<u64, ReadError> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output();
    match output {
        Ok(output) => ps_resident_bytes(&String::from_utf8_lossy(&output.stdout)),
        Err(error) => Err(ReadError::File(error)),
    }
}

/// Whether `held` bytes are past the `limit`.
pub(crate) fn check(held: u64, limit: u64) -> Result<(), MemoryError> {
    if held > limit {
        return Err(MemoryError::PastLimit { held, limit });
    }
    Ok(())
}

/// Reads resident memory from `resident` every second, recording each
/// reading in `readings`, until it passes `limit`, if one is set, or
/// can't be read, and returns why it stopped. An unreadable figure ends
/// the watch too: a limit nothing checks would pass for one that holds.
pub(crate) fn watch(
    resident: &Resident,
    limit: Option<u64>,
    readings: &MemoryReadings,
) -> MemoryError {
    loop {
        let held = match resident.read() {
            Ok(held) => held,
            Err(error) => return error,
        };
        readings.record(Memory {
            resident_bytes: held,
            limit_bytes: limit,
        });
        if let Some(limit) = limit
            && let Err(stop) = check(held, limit)
        {
            return stop;
        }
        thread::sleep(READ_EVERY);
    }
}

#[cfg(test)]
mod tests;
