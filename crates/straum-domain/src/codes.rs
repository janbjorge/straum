//! Codes: the small integer a column's value goes by inside the store.
//! A rollup-key part or a dim attribute codes each value the first time
//! it meets one and keeps the value, so a cell key or a projection row is
//! a few integers, and an answer looks the value up again by its code.

use crate::hash::FastMap;
use crate::value::Datum;

/// A shorter list would drop the codes past its end without a word.
const FLAG_PER_CODE: &str = "keep is given one live flag per code";

/// A column's values, each under the code of its first add, so a cell key
/// or a projection entry is a few integers. Codes outlive their rows until
/// [`Codes::keep`] drops the dead ones or `clear` drops them all.
#[derive(Debug, Default)]
pub(crate) struct Codes {
    codes: FastMap<Datum, u32>,
    values: Vec<Datum>,
}

impl Codes {
    /// The code of `value`, given now if it has none yet.
    pub(crate) fn code(&mut self, value: &Datum) -> u32 {
        if let Some(&code) = self.codes.get(value) {
            return code;
        }
        let code = u32::try_from(self.values.len()).expect("fewer than 2^32 values fit in memory");
        self.values.push(value.clone());
        self.codes.insert(value.clone(), code);
        code
    }

    /// The code of a value seen before.
    pub(crate) fn get(&self, value: &Datum) -> Option<u32> {
        self.codes.get(value).copied()
    }

    /// The value a code stands for.
    pub(crate) fn value(&self, code: u32) -> &Datum {
        &self.values[code as usize]
    }

    /// How many codes have been given.
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    /// Keeps the values whose code is `live`, numbered again from 0 in
    /// their old order, and returns each old code's new one; a dead code
    /// maps to [`DEAD`]. Code 0, if live, stays 0.
    pub(crate) fn keep(&mut self, live: &[bool]) -> Vec<u32> {
        assert_eq!(live.len(), self.values.len(), "{FLAG_PER_CODE}");
        let mut new_codes = Vec::with_capacity(self.values.len());
        let mut values = Vec::new();
        let mut codes = FastMap::default();
        for (value, &kept) in self.values.drain(..).zip(live) {
            if !kept {
                new_codes.push(DEAD);
                continue;
            }
            let code = u32::try_from(values.len()).expect("fewer codes than before");
            codes.insert(value.clone(), code);
            values.push(value);
            new_codes.push(code);
        }
        self.values = values;
        self.codes = codes;
        new_codes
    }
}

/// What [`Codes::keep`] maps a dead code to: no code a value has.
pub(crate) const DEAD: u32 = u32::MAX;

/// Codes a column may hold before it is worth looking for dead ones: a
/// recode reads every cell, so it waits until it can free many.
const RECODE_FLOOR: usize = 4_096;

/// Whether a column with `codes` codes, at most `live` of them still used,
/// should drop its dead ones: when they may be most of it. Waiting for
/// twice the live ones makes each recode pay for the codes given since
/// the last.
pub(crate) fn worth_recoding(codes: usize, live: usize) -> bool {
    codes > RECODE_FLOOR && codes > live.saturating_mul(2)
}
