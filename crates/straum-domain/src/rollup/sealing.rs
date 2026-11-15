//! Sealing: a hot `date_bin` bucket that has ended goes out to a spill as
//! plain data, and the rollup keeps only its place and its cell count.
//! An ask or a change that needs it back thaws it, which checks every
//! code and count against the rollup, then unseals it in place.

use crate::column::Counts;
use crate::index::{Index, Keys};
use crate::sealed::{Sealed, SealedColumn, SealedError, SealedId, Thawed};
use crate::value::Datum;

use super::cells::Cells;
use super::level::{Bucket, Level};
use super::state::State;
use super::{BucketId, DATE_BIN, Rollup};

impl Rollup {
    /// Each `date_bin` bucket not sealed that ends by `before`. Wider
    /// buckets stay in memory: an ask over a long range reads them, and
    /// the narrow ones only at its ends (ADR-0035).
    pub(crate) fn sealable(&self, before: i64) -> Vec<BucketId> {
        let level = &self.levels[DATE_BIN.0];
        let mut sealable = Vec::new();
        for (start, bucket) in level.buckets.range(..Some(before)) {
            let Some(start) = *start else {
                continue;
            };
            if let Some(id) = level.hot_ended_by(start, bucket, before) {
                sealable.push(id);
            }
        }
        sealable
    }

    /// A copy of a bucket not sealed.
    pub(crate) fn sealed_copy(&self, id: &SealedId) -> Option<Sealed> {
        let bucket = self.level(id.width)?.buckets.get(&Some(id.bucket_start))?;
        let cells = bucket.cells()?;
        let mut columns = Vec::with_capacity(cells.states.len());
        for state in &cells.states {
            columns.push(state.sealed(&cells.rows));
        }
        Some(Sealed {
            id: id.clone(),
            keys: cells.keys.clone(),
            counts: cells.rows.iter().collect(),
            columns,
        })
    }

    /// Drops a bucket's cells and keeps its place.
    pub(crate) fn seal(&mut self, width: i64, start: i64) -> Result<(), &'static str> {
        let level = self.level_mut(width)?;
        let Some(bucket) = level.buckets.get_mut(&Some(start)) else {
            return Err("no such bucket");
        };
        if let Bucket::Sealed { .. } = bucket {
            return Err("sealed already");
        }
        *bucket = Bucket::Sealed {
            cells: bucket.len(),
        };
        self.sealed += 1;
        Ok(())
    }

    /// Every sealed bucket.
    pub(crate) fn sealed(&self) -> Vec<BucketId> {
        let mut sealed = Vec::new();
        // Most rollups have none sealed, so none is walked.
        if self.sealed == 0 {
            return sealed;
        }
        for level in &self.levels {
            for (start, bucket) in &level.buckets {
                if let (Some(start), Bucket::Sealed { .. }) = (*start, bucket) {
                    sealed.push(BucketId {
                        width: level.width,
                        start,
                    });
                }
            }
        }
        sealed
    }

    /// Each sealed bucket a row of time `time` falls in.
    pub(crate) fn touched(&self, time: &Datum) -> Vec<BucketId> {
        let mut touched = Vec::new();
        if self.sealed == 0 {
            return touched;
        }
        for level in &self.levels {
            if let Some(id) = level.sealed_bucket_of(time) {
                touched.push(id);
            }
        }
        touched
    }

    /// Whether a row of time `time` falls in a sealed bucket. Every row
    /// added asks, so this allocates nothing; most rollups have none
    /// sealed.
    pub(super) fn touches_sealed(&self, time: &Datum) -> bool {
        if self.sealed == 0 {
            return false;
        }
        for level in &self.levels {
            if level.sealed_bucket_of(time).is_some() {
                return true;
            }
        }
        false
    }

    /// A bucket from a spill, checked so that no ask or change on it can
    /// panic: a width this rollup has, codes it holds, rows in every cell,
    /// a column per input of its kind and length.
    pub(crate) fn thaw(&self, sealed: Sealed) -> Result<Thawed, SealedError> {
        let Sealed {
            id,
            keys,
            counts,
            columns,
        } = sealed;
        match self.thawed_cells(id.width, keys, &counts, columns) {
            Ok(cells) => Ok(Thawed { id, cells }),
            Err(reason) => Err(SealedError { id, reason }),
        }
    }

    fn thawed_cells(
        &self,
        bucket_width: i64,
        keys: Vec<u32>,
        counts: &[u64],
        columns: Vec<SealedColumn>,
    ) -> Result<Cells, &'static str> {
        if self.level(bucket_width).is_none() {
            return Err("no such width");
        }
        let key_parts = self.key_columns.len();
        if counts.is_empty() {
            return Err("no cells");
        }
        if counts.contains(&0) {
            return Err("a cell without rows");
        }
        if Some(keys.len()) != counts.len().checked_mul(key_parts) {
            return Err("keys don't fit the rollup key");
        }
        if !self.knows_codes(&keys, key_parts) {
            return Err("a code the store never gave");
        }
        if columns.len() != self.inputs.len() {
            return Err("columns don't fit the measures");
        }
        let mut states = Vec::with_capacity(columns.len());
        for (column, input) in columns.into_iter().zip(&self.inputs) {
            let Some(state) = State::thaw(column, input.kind, counts) else {
                return Err("columns don't fit the measures");
            };
            states.push(state);
        }
        Ok(Cells {
            key_parts,
            index: Index::default(),
            keys,
            rows: Counts::of(counts),
            states,
        })
    }

    /// Whether every code in `keys`, `key_parts` a cell, is one this
    /// rollup gave its part.
    fn knows_codes(&self, keys: &[u32], key_parts: usize) -> bool {
        for (position, &code) in keys.iter().enumerate() {
            if code as usize >= self.codes[position % key_parts].len() {
                return false;
            }
        }
        true
    }

    /// Puts back the cells of a sealed bucket.
    pub(crate) fn unseal(
        &mut self,
        width: i64,
        start: i64,
        mut cells: Cells,
    ) -> Result<(), &'static str> {
        let bucket = self.level_mut(width)?.buckets.get_mut(&Some(start));
        let Some(bucket @ Bucket::Sealed { .. }) = bucket else {
            return Err("not sealed");
        };
        let keys = Keys {
            codes: &cells.keys,
            codes_per_key: cells.key_parts,
        };
        if !cells.index.fill(cells.rows.len(), keys) {
            return Err("a key held twice");
        }
        *bucket = Bucket::Hot(cells);
        self.sealed -= 1;
        Ok(())
    }

    fn level(&self, width: i64) -> Option<&Level> {
        for level in &self.levels {
            if level.width == width {
                return Some(level);
            }
        }
        None
    }

    fn level_mut(&mut self, width: i64) -> Result<&mut Level, &'static str> {
        for level in &mut self.levels {
            if level.width == width {
                return Ok(level);
            }
        }
        Err("no such width")
    }
}
