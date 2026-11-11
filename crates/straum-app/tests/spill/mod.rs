//! A spill in memory, which can be broken on purpose and counts its calls.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use straum_app::{Spill, SpillError};
use straum_domain::{Sealed, SealedId};

#[derive(Default)]
pub struct MemorySpill {
    pub buckets: Mutex<BTreeMap<SealedId, Sealed>>,
    /// Every call fails while set.
    pub broken: AtomicBool,
    pub gets: AtomicUsize,
    pub clears: AtomicUsize,
}

impl MemorySpill {
    pub fn ids(&self) -> Vec<SealedId> {
        self.buckets.lock().unwrap().keys().cloned().collect()
    }

    pub fn break_it(&self, broken: bool) {
        self.broken.store(broken, Ordering::Relaxed);
    }

    pub fn gets(&self) -> usize {
        self.gets.load(Ordering::Relaxed)
    }

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
        buckets
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no {id}").into())
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
