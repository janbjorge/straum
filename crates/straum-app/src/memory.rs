//! Straum's resident memory as the binary's watch last read it. The app
//! reads no file: the watch reads Linux's figure each second and records
//! it through the `MemoryReadings` the app hands out, and the status
//! takes the last reading from there.

use std::sync::{Arc, Mutex};

const READINGS_POISONED: &str = "a thread panicked while it held the memory reading";

/// A reading of resident memory, and the `--max-memory` limit if one is
/// set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    pub resident_bytes: u64,
    pub limit_bytes: Option<u64>,
}

/// Where the watch puts its last reading and the status takes it.
/// Clones share it.
#[derive(Debug, Clone, Default)]
pub struct MemoryReadings {
    last: Arc<Mutex<Option<Memory>>>,
}

impl MemoryReadings {
    /// Keeps `memory` as the last reading.
    pub fn record(&self, memory: Memory) {
        *self.last.lock().expect(READINGS_POISONED) = Some(memory);
    }

    /// The last reading, or `None` where the system gives no figure.
    pub fn last(&self) -> Option<Memory> {
        *self.last.lock().expect(READINGS_POISONED)
    }
}
