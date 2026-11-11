//! CRC-16/XMODEM — the check byte pair on every Davis LOOP packet.
//!
//! `x^16 + x^12 + x^5 + 1`, initial value zero, no final XOR, no reflection. The property
//! the Davis protocol relies on is that running the CRC over *the packet including its own
//! two check bytes* yields **zero**, so a receiver compares against one constant rather
//! than extracting the trailing pair and comparing it against a computed one. That is what
//! [`check`] is, and it is the same test `pkg/crc16/crc16.go` makes at
//! `davis/station.go:398`.
//!
//! **The table is generated at compile time rather than transcribed.** `pkg/crc16` carries
//! 256 literals ported from weewx, and a transcribed table is 256 opportunities for one
//! digit to be wrong in a way that only shows up for one byte value — which would present
//! as a Davis console whose packets occasionally fail their check for no reason. The
//! generator below is the polynomial, and the test asserts the four entries weewx's own
//! table publishes plus the end-to-end property.

/// The polynomial, CCITT's.
const POLYNOMIAL: u16 = 0x1021;

/// One entry per leading byte, derived from [`POLYNOMIAL`].
const TABLE: [u16; 256] = table();

const fn table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut byte = 0usize;
    while byte < 256 {
        let mut crc = (byte as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ POLYNOMIAL
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
}

/// The CRC of `bytes`.
#[must_use]
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for byte in bytes {
        crc = (crc << 8) ^ TABLE[usize::from((crc >> 8) ^ u16::from(*byte))];
    }
    crc
}

/// Whether `bytes` carries its own correct check pair in its last two bytes.
///
/// A frame shorter than the two check bytes cannot carry one, and is therefore not
/// checkable rather than trivially valid: `crc16(&[])` is zero, so without this guard an
/// empty frame would pass.
#[must_use]
pub fn check(bytes: &[u8]) -> bool {
    bytes.len() > 2 && crc16(bytes) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four entries of the table `pkg/crc16/crc16.go` publishes, at the indices where a
    /// transcription error is most likely to hide: the first, the two that fix the
    /// polynomial, and the last.
    #[test]
    fn the_generated_table_is_weewxs_table() {
        assert_eq!(TABLE[0], 0x0000);
        assert_eq!(TABLE[1], 0x1021);
        assert_eq!(TABLE[2], 0x2042);
        assert_eq!(TABLE[255], 0x1ef0);
        // Row boundaries from the same source, every 16th entry on the first two rows.
        assert_eq!(TABLE[8], 0x8108);
        assert_eq!(TABLE[16], 0x1231);
        assert_eq!(TABLE[128], 0x9188);
    }

    /// The standard XMODEM check value: `"123456789"` is `0x31C3`.
    #[test]
    fn the_check_value_is_xmodems() {
        assert_eq!(crc16(b"123456789"), 0x31C3);
        assert_eq!(crc16(b""), 0);
    }

    /// The property the Davis receiver uses: appending the CRC big-endian makes the CRC of
    /// the whole thing zero.
    #[test]
    fn a_frame_carrying_its_own_check_pair_sums_to_zero() {
        let payload = b"LOO\x00\x00 and ninety more bytes of nothing in particular";
        let crc = crc16(payload);
        let mut framed = payload.to_vec();
        framed.extend_from_slice(&crc.to_be_bytes());
        assert_eq!(crc16(&framed), 0);
        assert!(check(&framed));

        // One flipped bit anywhere fails it.
        for at in 0..framed.len() {
            let mut corrupt = framed.clone();
            corrupt[at] ^= 0x01;
            assert!(!check(&corrupt), "a flipped bit at {at} passed the check");
        }
    }

    /// `crc16(&[])` is zero, so "the CRC is zero" is not on its own a valid frame.
    #[test]
    fn a_frame_too_short_to_carry_a_check_pair_is_not_checkable() {
        assert!(!check(b""));
        assert!(!check(b"\x00\x00"));
        assert!(check(b"\x00\x00\x00"), "three zero bytes do sum to zero");
    }
}
