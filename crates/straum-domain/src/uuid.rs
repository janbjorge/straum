//! UUIDs as PostgreSQL keeps them: sixteen bytes, ordered byte by byte.
//! Text comes in and goes out as PostgreSQL prints a `uuid`: lowercase
//! hex in groups of 8, 4, 4, 4 and 12 digits joined by hyphens. The
//! stream reads a uuid's text with [`parse_uuid`], an ask's parameter is
//! read the same way, and an answer prints a uuid with [`format_uuid`].

/// The hex digits in each group of a uuid's text, in order.
const GROUP_DIGITS: [usize; 5] = [8, 4, 4, 4, 12];
const UUID_BYTES: usize = 16;
/// The bytes a hyphen goes before: where groups 2 to 5 start.
const HYPHEN_BEFORE_BYTE: [usize; 4] = [4, 6, 8, 10];
const UUID_TEXT_LENGTH: usize = 36;
const BITS_PER_DIGIT: u32 = 4;
const LOW_DIGIT: u8 = 0x0f;
const HEX: &[u8; 16] = b"0123456789abcdef";

/// A uuid's text as PostgreSQL prints it, as its sixteen bytes. `None`
/// for any other spelling PostgreSQL would read, such as uppercase,
/// braces or no hyphens: one spelling per uuid, so a filter's value finds
/// the uuids an answer prints so.
pub fn parse_uuid(text: &str) -> Option<[u8; UUID_BYTES]> {
    let mut value: u128 = 0;
    let mut groups = text.split('-');
    for digits in GROUP_DIGITS {
        let group = groups.next()?;
        if group.len() != digits {
            return None;
        }
        for byte in group.bytes() {
            let digit = lowercase_hex(byte)?;
            value = (value << BITS_PER_DIGIT) | u128::from(digit);
        }
    }
    if groups.next().is_some() {
        return None;
    }
    Some(value.to_be_bytes())
}

/// A uuid's sixteen bytes as PostgreSQL prints them.
pub fn format_uuid(bytes: &[u8; UUID_BYTES]) -> String {
    let mut text = String::with_capacity(UUID_TEXT_LENGTH);
    for (index, byte) in bytes.iter().enumerate() {
        if HYPHEN_BEFORE_BYTE.contains(&index) {
            text.push('-');
        }
        text.push(char::from(HEX[usize::from(byte >> BITS_PER_DIGIT)]));
        text.push(char::from(HEX[usize::from(byte & LOW_DIGIT)]));
    }
    text
}

/// The value of a lowercase hex digit.
fn lowercase_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_uuids_as_postgresql_prints_them() {
        let cases = [
            ("00000000-0000-0000-0000-000000000000", [0; 16]),
            ("ffffffff-ffff-ffff-ffff-ffffffffffff", [0xff; 16]),
            (
                "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
                [
                    0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd,
                    0x38, 0x0a, 0x11,
                ],
            ),
        ];
        for (text, bytes) in cases {
            assert_eq!(parse_uuid(text), Some(bytes), "{text}");
            assert_eq!(format_uuid(&bytes), text);
        }
    }

    #[test]
    fn refuses_other_spellings() {
        let cases = [
            "",
            "A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380A11",
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}",
            "a0eebc999c0b4ef8bb6d6bb9bd380a11",
            "a0eebc99-9c0b4ef8-bb6d-6bb9bd380a11",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a111",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11-",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1g",
            "+0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1é",
        ];
        for text in cases {
            assert_eq!(parse_uuid(text), None, "{text}");
        }
    }
}
