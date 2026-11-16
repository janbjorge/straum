//! Sealing: a hot `date_bin` bucket's cells go to a spill and leave
//! memory, keeping the bucket's place (ADR-0010). The store names the
//! buckets ready to seal, copies one out, drops it, checks a copy that
//! comes back (thawing) and puts it back (unsealing). A sealed bucket
//! leaves the bound cells while it is out.

use crate::ask::{Ask, AskError};
use crate::rollup::{BucketId, Direction, RowError};
use crate::sealed::{Sealed, SealedError, SealedId, Thawed};
use crate::spec::Fact;
use crate::value::Datum;

use super::ask::buckets;
use super::{FactIndex, IN_SPEC, POURED, Store};

impl Store {
    /// Each `date_bin` bucket not sealed that ends by `before`, Unix
    /// seconds, every fact.
    pub fn sealable(&self, before: i64) -> Vec<SealedId> {
        let mut sealable = Vec::new();
        for (fact, rollup) in self.spec.facts().iter().zip(&self.rollups) {
            sealable.extend(sealed_ids(fact, &rollup.sealable(before)));
        }
        sealable
    }

    /// A copy of a bucket's cells, for a spill to keep before
    /// [`Store::seal`] drops them. `None` if it is sealed or has no cells.
    pub fn sealed_copy(&self, id: &SealedId) -> Option<Sealed> {
        let fact = self.fact_index(id.fact.as_str())?;
        self.keyed(fact).sealed_copy(id)
    }

    /// Drops a bucket's cells, keeping its place, so asks and changes ask
    /// for it back. What the bucket put into the bound cells leaves them,
    /// at every width, until [`Store::unseal`] binds it again: the bound
    /// cells hold the hot buckets and no other, so a dim change moves all
    /// of them, and an ask over a sealed bucket reads the keyed cells.
    pub fn seal(&mut self, id: &SealedId) -> Result<(), SealedError> {
        let Some(fact) = self.fact_index(id.fact.as_str()) else {
            return Err(sealed_error(id, "unknown fact"));
        };
        let bucket = BucketId {
            width: id.width,
            start: id.bucket_start,
        };
        if self.pour_bucket(fact, bucket, Direction::Remove).is_err() {
            return Err(sealed_error(id, "bound cells don't hold the bucket"));
        }
        match self.rollups[fact.0].seal(id.width, id.bucket_start) {
            Ok(()) => Ok(()),
            Err(reason) => Err(sealed_error(id, reason)),
        }
    }

    /// Every sealed bucket of a fact, which a truncate drops with the rest.
    pub fn sealed(&self, fact: &str) -> Vec<SealedId> {
        let index = self.fact_index(fact).expect(IN_SPEC);
        sealed_ids(self.fact(index), &self.keyed(index).sealed())
    }

    /// The sealed buckets [`Store::ask_with`] needs for `ask`; none when
    /// the bound cells answer it, which are never sealed.
    pub fn sealed_read(&self, ask: &Ask) -> Result<Vec<SealedId>, Vec<AskError>> {
        let reading = self.reading(ask)?;
        let mut sealed = Vec::new();
        for covered in buckets(reading.read.rollup(), ask) {
            if covered.bucket.cells().is_none() {
                sealed.push(covered.id);
            }
        }
        Ok(sealed_ids(reading.fact, &sealed))
    }

    /// The sealed buckets a fact row falls in, which must be unsealed
    /// before the row is added or removed.
    pub fn sealed_touched(&self, fact: &str, row: &[Datum]) -> Vec<SealedId> {
        let index = self.fact_index(fact).expect(IN_SPEC);
        let Some(time) = row.first() else {
            return Vec::new();
        };
        sealed_ids(self.fact(index), &self.keyed(index).touched(time))
    }

    /// Checks a sealed bucket against the store, so neither an ask nor
    /// [`Store::unseal`] can panic on it.
    pub fn thaw(&self, sealed: Sealed) -> Result<Thawed, SealedError> {
        let Some(fact) = self.fact_index(sealed.id.fact.as_str()) else {
            return Err(SealedError {
                id: sealed.id,
                reason: "unknown fact",
            });
        };
        self.keyed(fact).thaw(sealed)
    }

    /// Puts a sealed bucket's cells back, so changes reach them again,
    /// and binds them as the dims are now.
    pub fn unseal(&mut self, thawed: Thawed) -> Result<(), SealedError> {
        let Thawed { id, cells } = thawed;
        let fact = self.fact_index(id.fact.as_str()).expect(IN_SPEC);
        if let Err(reason) = self.rollups[fact.0].unseal(id.width, id.bucket_start, cells) {
            return Err(SealedError { id, reason });
        }
        let bucket = BucketId {
            width: id.width,
            start: id.bucket_start,
        };
        self.pour_bucket(fact, bucket, Direction::Add)
            .expect(POURED);
        Ok(())
    }

    /// Binds a hot `date_bin` bucket's cells into the bound cells, or
    /// takes them back out, as the dims are now. A wider bucket has no
    /// rows of its own to bind.
    fn pour_bucket(
        &mut self,
        fact: FactIndex,
        bucket: BucketId,
        direction: Direction,
    ) -> Result<(), RowError> {
        let Some(bound) = self.bound[fact.0].as_mut() else {
            return Ok(());
        };
        let keyed = &self.rollups[fact.0];
        if bucket.width != keyed.date_bin_width() {
            return Ok(());
        }
        let start = Some(bucket.start);
        let Some(cells) = keyed.hot_date_bin(start) else {
            return Ok(());
        };
        bound.pour(keyed, &self.projections, start, cells, direction)
    }
}

fn sealed_error(id: &SealedId, reason: &'static str) -> SealedError {
    SealedError {
        id: id.clone(),
        reason,
    }
}

/// The ids of a fact's buckets.
fn sealed_ids(fact: &Fact, buckets: &[BucketId]) -> Vec<SealedId> {
    let mut ids = Vec::with_capacity(buckets.len());
    for bucket in buckets {
        ids.push(SealedId {
            fact: fact.name.clone(),
            width: bucket.width,
            bucket_start: bucket.start,
        });
    }
    ids
}
