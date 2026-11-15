//! The decompressors for full-page images `wal_compression` writes. pglz
//! as `pglz_decompress` in `src/common/pg_lzcompress.c` reads it; lz4 as a
//! bare LZ4 block, as `LZ4_decompress_safe` reads it. Both refuse input
//! that does not come out at exactly `size` bytes.

/// A pglz copy tag's first byte: the high nibble is the top of a 12-bit
/// offset, the low one the length less 3. A nibble of 15 (a length of 18)
/// says a third byte adds to the length (`pglz_decompress`).
const PGLZ_OFFSET_HIGH: usize = 0xF0;
const PGLZ_LENGTH: usize = 0x0F;
const PGLZ_SHORTEST_COPY: usize = 3;
const PGLZ_LONG_COPY: usize = 18;
/// An LZ4 token: the literals' length nibble high, the match's low. A
/// nibble of 15 goes on in the bytes after, while they are 255; a match is
/// 4 bytes at least.
const LZ4_LITERALS_SHIFT: u8 = 4;
const LZ4_MATCH_LENGTH: u8 = 0x0F;
const LZ4_LONG_LENGTH: usize = 15;
const LZ4_MORE_LENGTH: u8 = 255;
const LZ4_SHORTEST_MATCH: usize = 4;

/// A pglz stream: control bytes, each saying which of the next eight
/// items are literals and which copy from what came out before.
pub(crate) fn pglz(source: &[u8], size: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(size);
    let mut at = 0;
    while at < source.len() && out.len() < size {
        let mut control = source[at];
        at += 1;
        for _ in 0..8 {
            if at >= source.len() || out.len() >= size {
                break;
            }
            if control & 1 == 0 {
                out.push(source[at]);
                at += 1;
            } else {
                let first = usize::from(*source.get(at)?);
                let second = usize::from(*source.get(at + 1)?);
                at += 2;
                let mut length = (first & PGLZ_LENGTH) + PGLZ_SHORTEST_COPY;
                let offset = ((first & PGLZ_OFFSET_HIGH) << 4) | second;
                if length == PGLZ_LONG_COPY {
                    length += usize::from(*source.get(at)?);
                    at += 1;
                }
                if offset == 0 || offset > out.len() {
                    return None;
                }
                let length = length.min(size - out.len());
                copy_back(&mut out, offset, length);
            }
            control >>= 1;
        }
    }
    (out.len() == size && at == source.len()).then_some(out)
}

/// An LZ4 block: sequences of literals then a match, the last with
/// literals only.
pub(crate) fn lz4(source: &[u8], size: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(size);
    let mut at = 0;
    loop {
        let token = *source.get(at)?;
        at += 1;
        let literals = lz4_length(source, &mut at, usize::from(token >> LZ4_LITERALS_SHIFT))?;
        out.extend_from_slice(source.get(at..at.checked_add(literals)?)?);
        at += literals;
        if at == source.len() {
            break;
        }
        let offset = usize::from(u16::from_le_bytes([*source.get(at)?, *source.get(at + 1)?]));
        at += 2;
        let nibble = usize::from(token & LZ4_MATCH_LENGTH);
        let length = lz4_length(source, &mut at, nibble)? + LZ4_SHORTEST_MATCH;
        if offset == 0 || offset > out.len() || out.len() + length > size {
            return None;
        }
        copy_back(&mut out, offset, length);
    }
    (out.len() == size).then_some(out)
}

/// A length nibble of 15 goes on in the bytes after, while they are 255.
fn lz4_length(source: &[u8], at: &mut usize, nibble: usize) -> Option<usize> {
    let mut length = nibble;
    if nibble == LZ4_LONG_LENGTH {
        loop {
            let byte = *source.get(*at)?;
            *at += 1;
            length = length.checked_add(usize::from(byte))?;
            if byte != LZ4_MORE_LENGTH {
                break;
            }
        }
    }
    Some(length)
}

/// Copies `length` bytes from `offset` back, a byte at a time, since the
/// copy may overlap what it writes.
fn copy_back(out: &mut Vec<u8>, offset: usize, length: usize) {
    let from = out.len() - offset;
    for index in from..from + length {
        out.push(out[index]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A splitmix generator, so failures replay.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut mixed = self.0;
            mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            mixed ^ (mixed >> 31)
        }

        fn below(&mut self, limit: usize) -> usize {
            usize::try_from(self.next() % limit as u64).unwrap()
        }
    }

    /// Data with repeats near and far, like a page of tuples.
    fn page_like(rng: &mut Rng, size: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            if data.len() > 4 && rng.below(2) == 0 {
                let back = 1 + rng.below(data.len().min(4000));
                let length = 1 + rng.below(300);
                for _ in 0..length {
                    data.push(data[data.len() - back]);
                }
            } else {
                for _ in 0..rng.below(20) {
                    data.push(u8::try_from(rng.below(8)).unwrap());
                }
            }
        }
        data.truncate(size);
        data
    }

    struct Match {
        offset: usize,
        length: usize,
    }

    /// The longest earlier match at `at`, as a compressor finds it.
    fn longest(data: &[u8], at: usize, window: usize, most: usize) -> Match {
        let mut best = Match {
            offset: 0,
            length: 0,
        };
        for from in at.saturating_sub(window)..at {
            let mut length = 0;
            while length < most
                && at + length < data.len()
                && data[from + length] == data[at + length]
            {
                length += 1;
            }
            if length > best.length {
                best = Match {
                    offset: at - from,
                    length,
                };
            }
        }
        best
    }

    /// pglz as `pglz_compress` lays it out, with a greedy search.
    fn pglz_compress(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < data.len() {
            let control = out.len();
            out.push(0);
            for bit in 0..8 {
                if at >= data.len() {
                    break;
                }
                let Match { offset, length } = longest(data, at, 4095, 273);
                if length >= PGLZ_SHORTEST_COPY {
                    out[control] |= 1 << bit;
                    let offset_high = (offset >> 4) & PGLZ_OFFSET_HIGH;
                    let offset_low = offset.to_le_bytes()[0];
                    if length >= PGLZ_LONG_COPY {
                        out.push(byte(offset_high | PGLZ_LENGTH));
                        out.push(offset_low);
                        out.push(byte(length - PGLZ_LONG_COPY));
                    } else {
                        out.push(byte(offset_high | (length - PGLZ_SHORTEST_COPY)));
                        out.push(offset_low);
                    }
                    at += length;
                } else {
                    out.push(data[at]);
                    at += 1;
                }
            }
        }
        out
    }

    fn byte(value: usize) -> u8 {
        u8::try_from(value).unwrap()
    }

    fn lz4_length_bytes(out: &mut Vec<u8>, mut rest: usize) {
        while rest >= 255 {
            out.push(255);
            rest -= 255;
        }
        out.push(byte(rest));
    }

    /// An LZ4 block, with a greedy search; the last 5 bytes stay literal,
    /// as the format asks.
    fn lz4_compress(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut literal_start = 0;
        let mut at = 0;
        while at + 5 < data.len() {
            let Match { offset, length } = longest(data, at, 65_535, data.len() - 5 - at);
            if length < 4 {
                at += 1;
                continue;
            }
            let literals = at - literal_start;
            let token = byte(literals.min(15) << 4) | byte((length - 4).min(15));
            out.push(token);
            if literals >= 15 {
                lz4_length_bytes(&mut out, literals - 15);
            }
            out.extend_from_slice(&data[literal_start..at]);
            out.extend_from_slice(&u16::try_from(offset).unwrap().to_le_bytes());
            if length - 4 >= 15 {
                lz4_length_bytes(&mut out, length - 4 - 15);
            }
            at += length;
            literal_start = at;
        }
        let literals = data.len() - literal_start;
        out.push(byte(literals.min(15) << 4));
        if literals >= 15 {
            lz4_length_bytes(&mut out, literals - 15);
        }
        out.extend_from_slice(&data[literal_start..]);
        out
    }

    #[test]
    fn decompresses_what_was_compressed() {
        for seed in 0..40 {
            let mut rng = Rng(seed);
            let size = 1 + rng.below(8192);
            let data = page_like(&mut rng, size);
            assert_eq!(
                pglz(&pglz_compress(&data), size),
                Some(data.clone()),
                "{seed}"
            );
            assert_eq!(lz4(&lz4_compress(&data), size), Some(data), "{seed}");
        }
    }

    #[test]
    fn refuses_what_does_not_come_out_whole() {
        let data = page_like(&mut Rng(7), 3000);
        let packed = pglz_compress(&data);
        assert!(pglz(&packed, 3001).is_none());
        assert!(pglz(&packed[..packed.len() - 1], 3000).is_none());
        // pglz, as PostgreSQL, cuts a last copy short at the size asked.
        let packed = lz4_compress(&data);
        assert!(lz4(&packed, 2999).is_none());
        assert!(lz4(&packed, 3001).is_none());
        assert!(lz4(&packed[..packed.len() - 1], 3000).is_none());
        // A copy from before the start.
        assert_eq!(pglz(&[0b10, b'a', 0x00, 0x05], 4), None);
        assert_eq!(lz4(&[0x10, b'a', 0x05, 0x00, 0x00], 5), None);
        assert_eq!(lz4(&[0x10, b'a', 0x00, 0x00, 0x00], 5), None);
        assert_eq!(lz4(&[], 0), None);
        // Bytes left over once the page is whole.
        assert_eq!(pglz(&[0, b'a', b'b'], 1), None);
        assert_eq!(pglz(&[0, b'a'], 1), Some(vec![b'a']));
    }
}
