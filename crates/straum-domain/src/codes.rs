use crate::hash::FastMap;
use crate::value::Datum;

/// A column's values, each under the code of its first add, so a cell key
/// or a projection entry is a few integers. Codes outlive their rows until
/// `clear`.
#[derive(Debug, Default)]
pub(crate) struct Codes {
    codes: FastMap<Datum, u32>,
    values: Vec<Datum>,
}

impl Codes {
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

    pub(crate) fn value(&self, code: u32) -> &Datum {
        &self.values[code as usize]
    }

    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }
}
