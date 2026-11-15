//! The two decompressors PostgreSQL uses, for full-page images that
//! `wal_compression` compresses and for values TOAST compresses: pglz,
//! PostgreSQL's own, as `pglz_decompress` in `src/common/pg_lzcompress.c`
//! reads it, and lz4, as a bare LZ4 block that `LZ4_decompress_safe`
//! reads. Compressed bytes go in, the page or value comes out. Both refuse
//! input that does not come out at exactly `size` bytes.

/// A pglz copy tag's first byte: the high nibble is the top of a 12-bit
/// offset, the low one the length less 3. A nibble of 15 (a length of 18)
/// says a third byte adds to the length (`pglz_decompress`).
const PGLZ_OFFSET_HIGH: usize = 0xF0;
const PGLZ_LENGTH: usize = 0x0F;
const PGLZ_SHORTEST_COPY: usize = 3;
const PGLZ_LONG_COPY: usize = 18;
/// How far the offset's high nibble moves up, above its low byte.
const PGLZ_OFFSET_HIGH_SHIFT: usize = 4;
/// A control byte says, bit by bit, what each of the next eight items is.
const PGLZ_ITEMS_PER_CONTROL: usize = 8;
/// An LZ4 token: the literals' length nibble high, the match's low. A
/// nibble of 15 goes on in the bytes after, while they are 255; a match is
/// 4 bytes at least.
const LZ4_LITERALS_SHIFT: u8 = 4;
const LZ4_MATCH_LENGTH: u8 = 0x0F;
const LZ4_LONG_LENGTH: usize = 15;
const LZ4_MORE_LENGTH: u8 = 255;
const LZ4_SHORTEST_MATCH: usize = 4;
/// A match's offset back: a little-endian `uint16`.
const LZ4_OFFSET_SIZE: usize = 2;

/// A pglz stream: control bytes, each saying which of the next eight
/// items are literals and which copy from what came out before.
pub(crate) fn pglz(source: &[u8], size: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(size);
    let mut at = 0;
    while at < source.len() && out.len() < size {
        let mut control = source[at];
        at += 1;
        for _ in 0..PGLZ_ITEMS_PER_CONTROL {
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
                let offset = ((first & PGLZ_OFFSET_HIGH) << PGLZ_OFFSET_HIGH_SHIFT) | second;
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
    if out.len() != size || at != source.len() {
        return None;
    }
    Some(out)
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
        let offset = source.get(at..at + LZ4_OFFSET_SIZE)?;
        let offset = usize::from(u16::from_le_bytes([offset[0], offset[1]]));
        at += LZ4_OFFSET_SIZE;
        let nibble = usize::from(token & LZ4_MATCH_LENGTH);
        let length = lz4_length(source, &mut at, nibble)? + LZ4_SHORTEST_MATCH;
        if offset == 0 || offset > out.len() || out.len() + length > size {
            return None;
        }
        copy_back(&mut out, offset, length);
    }
    if out.len() != size {
        return None;
    }
    Some(out)
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
    use crate::wal::test_support::Rng;

    /// `PGLZ_MAX_OFFSET` and `PGLZ_MAX_MATCH`: how far back and how long
    /// a pglz copy may reach.
    const PGLZ_MAX_OFFSET: usize = 4095;
    const PGLZ_MAX_MATCH: usize = 273;
    /// How far back an LZ4 match may reach, and `LASTLITERALS`: the bytes
    /// at the end of a block that stay literal.
    const LZ4_MAX_OFFSET: usize = 65_535;
    const LZ4_LAST_LITERALS: usize = 5;

    /// Data with repeats near and far, like a page of tuples.
    fn page_like(rng: &mut Rng, size: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            if data.len() > 4 && rng.below_usize(2) == 0 {
                let back = 1 + rng.below_usize(data.len().min(4000));
                let length = 1 + rng.below_usize(300);
                for _ in 0..length {
                    data.push(data[data.len() - back]);
                }
            } else {
                for _ in 0..rng.below_usize(20) {
                    data.push(u8::try_from(rng.below_usize(8)).unwrap());
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
                let Match { offset, length } = longest(data, at, PGLZ_MAX_OFFSET, PGLZ_MAX_MATCH);
                if length >= PGLZ_SHORTEST_COPY {
                    out[control] |= 1 << bit;
                    let offset_high = (offset >> PGLZ_OFFSET_HIGH_SHIFT) & PGLZ_OFFSET_HIGH;
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

    /// A length's nibble in a token: 15 says more bytes follow.
    fn lz4_nibble(length: usize) -> u8 {
        byte(length.min(LZ4_LONG_LENGTH))
    }

    fn lz4_length_bytes(out: &mut Vec<u8>, mut rest: usize) {
        let more = usize::from(LZ4_MORE_LENGTH);
        while rest >= more {
            out.push(LZ4_MORE_LENGTH);
            rest -= more;
        }
        out.push(byte(rest));
    }

    /// An LZ4 block, with a greedy search; the last 5 bytes stay literal,
    /// as the format asks.
    fn lz4_compress(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut literal_start = 0;
        let mut at = 0;
        while at + LZ4_LAST_LITERALS < data.len() {
            let most = data.len() - LZ4_LAST_LITERALS - at;
            let Match { offset, length } = longest(data, at, LZ4_MAX_OFFSET, most);
            if length < LZ4_SHORTEST_MATCH {
                at += 1;
                continue;
            }
            let literals = at - literal_start;
            let extra = length - LZ4_SHORTEST_MATCH;
            let token = lz4_nibble(literals) << LZ4_LITERALS_SHIFT | lz4_nibble(extra);
            out.push(token);
            if literals >= LZ4_LONG_LENGTH {
                lz4_length_bytes(&mut out, literals - LZ4_LONG_LENGTH);
            }
            out.extend_from_slice(&data[literal_start..at]);
            out.extend_from_slice(&u16::try_from(offset).unwrap().to_le_bytes());
            if extra >= LZ4_LONG_LENGTH {
                lz4_length_bytes(&mut out, extra - LZ4_LONG_LENGTH);
            }
            at += length;
            literal_start = at;
        }
        let literals = data.len() - literal_start;
        out.push(lz4_nibble(literals) << LZ4_LITERALS_SHIFT);
        if literals >= LZ4_LONG_LENGTH {
            lz4_length_bytes(&mut out, literals - LZ4_LONG_LENGTH);
        }
        out.extend_from_slice(&data[literal_start..]);
        out
    }

    #[test]
    fn decompresses_what_was_compressed() {
        for seed in 0..40 {
            let mut rng = Rng(seed);
            let size = 1 + rng.below_usize(8192);
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
