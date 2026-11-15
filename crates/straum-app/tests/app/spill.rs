//! A spill in memory: buckets in a map behind a lock, as the writer and
//! asks put and get them. A case can break it on purpose, and read how
//! often it was asked for a bucket or emptied.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use straum_app::{Spill, SpillError};
use straum_domain::{Sealed, SealedId};

/// A spill that keeps its buckets in memory and counts its calls.
#[derive(Default)]
pub struct MemorySpill {
    pub buckets: Mutex<BTreeMap<SealedId, Sealed>>,
    /// Every call fails while set.
    broken: AtomicBool,
    gets: AtomicUsize,
    clears: AtomicUsize,
}

impl MemorySpill {
    /// The buckets it holds now.
    pub fn ids(&self) -> Vec<SealedId> {
        self.buckets.lock().unwrap().keys().cloned().collect()
    }

    /// Makes every call fail while `broken`.
    pub fn break_it(&self, broken: bool) {
        self.broken.store(broken, Ordering::Relaxed);
    }

    /// How often a bucket was read back.
    pub fn gets(&self) -> usize {
        self.gets.load(Ordering::Relaxed)
    }

    /// How often it was emptied.
    pub fn clears(&self) -> usize {
        self.clears.load(Ordering::Relaxed)
    }

    fn working(&self) -> Result<(), SpillError> {
        if self.broken.load(Ordering::Relaxed) {
            return Err("broken".into());
        }
        Ok(())
    }
}

impl Spill for MemorySpill {
    fn put(&self, sealed: &Sealed) -> Result<(), SpillError> {
        self.working()?;
        let mut buckets = self.buckets.lock().unwrap();
        buckets.insert(sealed.id.clone(), sealed.clone());
        Ok(())
    }

    fn get(&self, id: &SealedId) -> Result<Sealed, SpillError> {
        self.working()?;
        self.gets.fetch_add(1, Ordering::Relaxed);
        let buckets = self.buckets.lock().unwrap();
        match buckets.get(id) {
            Some(sealed) => Ok(sealed.clone()),
            None => Err(format!("no {id}").into()),
        }
    }

    fn remove(&self, id: &SealedId) -> Result<(), SpillError> {
        self.working()?;
        self.buckets.lock().unwrap().remove(id);
        Ok(())
    }

    fn clear(&self) -> Result<(), SpillError> {
        self.working()?;
        self.clears.fetch_add(1, Ordering::Relaxed);
        self.buckets.lock().unwrap().clear();
        Ok(())
    }
}
