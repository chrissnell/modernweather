//! [`Callsign`] — a parsed amateur or CWOP station identifier, with its SSID.
//!
//! The Go tree carries a callsign as a `string` and re-derives facts from it at four call
//! sites: `strings.ToUpper` in the packet builder, `strings.ToUpper(strings.Split(c,
//! "-")[0])` in the passcode calculation, a `strings.HasPrefix(…, "DW")` test for the CWOP
//! rule, and `parseCallSSID` in the AX.25 encoder — which is the only one of the four that
//! validates anything. So an `aprs_callsign` of `"W1AW-99"` reaches APRS-IS as written and
//! fails only on the KISS path, and a lowercase callsign produces a different passcode
//! depending on which function saw it first.
//!
//! Here the parse is the validation (the same rule `mw_component::Zone` follows): a
//! `Callsign` is uppercase, 1–6 base characters of `[A-Z0-9]`, and an SSID of 0–15. Every
//! fact the four call sites re-derived is a method, so there is one answer each.

use std::fmt;

/// The largest SSID AX.25 can address: the field is four bits.
const MAX_SSID: u8 = 15;

/// The longest base callsign an AX.25 address field holds: six shifted bytes.
const MAX_BASE: usize = 6;

/// A station identifier, parsed.
///
/// `PartialOrd`/`Ord` so a configured digipeater path has a stable rendering in a log line
/// and in a test assertion.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Callsign {
    /// The uppercase base, 1–6 of `[A-Z0-9]`.
    base: String,
    /// 0–15.
    ssid: u8,
}

impl Callsign {
    /// Parse `CALL` or `CALL-SSID`.
    ///
    /// Surrounding whitespace is trimmed and the base is uppercased, because both are what
    /// an operator types into a text column. Everything else is a refusal: this is
    /// configuration being read once at plan time, and the alternative — the Go behaviour —
    /// is a callsign that transmits on one transport and fails on another.
    pub fn parse(text: &str) -> Result<Self, CallsignError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(CallsignError::Empty);
        }
        let (base, ssid) = match text.split_once('-') {
            None => (text, 0u8),
            Some((base, tail)) => {
                let ssid = tail.parse::<u8>().map_err(|_| CallsignError::BadSsid {
                    callsign: text.to_string(),
                    ssid: tail.to_string(),
                })?;
                if ssid > MAX_SSID {
                    return Err(CallsignError::SsidRange {
                        callsign: text.to_string(),
                        ssid,
                    });
                }
                (base, ssid)
            }
        };
        let base = base.to_ascii_uppercase();
        if base.is_empty() || base.len() > MAX_BASE {
            return Err(CallsignError::BaseLength {
                callsign: text.to_string(),
                base,
            });
        }
        if let Some(bad) = base.chars().find(|c| !c.is_ascii_alphanumeric()) {
            return Err(CallsignError::BaseCharacter {
                callsign: text.to_string(),
                bad,
            });
        }
        Ok(Self { base, ssid })
    }

    /// The base callsign, uppercase and without the SSID.
    ///
    /// What the passcode is computed over and what the AX.25 address field holds.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The SSID, 0–15.
    pub fn ssid(&self) -> u8 {
        self.ssid
    }

    /// Whether this looks like a CWOP-issued identifier rather than an amateur callsign.
    ///
    /// CWOP issues `CW`, `DW` and `EW` followed by **four digits**, and those stations log
    /// in to APRS-IS with the fixed passcode rather than a computed one
    /// ([`crate::Passcode::CWOP`]). The Go controller tests `strings.HasPrefix(…, "DW")`
    /// alone, in two places, so a `CW`-prefixed station — what a CWOP registration has
    /// issued since the `DW` block filled — computes a passcode the server will not accept,
    /// and the upload fails with "unable to log into APRS-IS" and no indication why.
    ///
    /// **The four digits are load-bearing and the prefix alone is not enough.** `EW` is
    /// Belarus's amateur prefix and `CW` is Uruguay's, so `EW1ABC` is a real callsign whose
    /// operator has a real passcode; matching the prefix alone would send `-1` for it and
    /// the login would be refused. A six-character all-digit suffix under those prefixes is
    /// still ambiguous in principle, which is why this is only a **default**:
    /// [`crate::Passcode::resolve`] prefers a configured passcode over it, so an operator
    /// in that position has somewhere to say so.
    pub fn is_cwop(&self) -> bool {
        let b = self.base.as_bytes();
        matches!(b, [b'C' | b'D' | b'E', b'W', ..])
            && b.len() == 6
            && b[2..].iter().all(u8::is_ascii_digit)
    }
}

impl fmt::Display for Callsign {
    /// `CALL` for SSID 0 and `CALL-SSID` otherwise, which is the form APRS-IS and a TNC2
    /// header use — and the form an operator typed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ssid == 0 {
            f.write_str(&self.base)
        } else {
            write!(f, "{}-{}", self.base, self.ssid)
        }
    }
}

/// Why a configured callsign is not one.
///
/// Every variant names the configured text, because this is read from a column an operator
/// edited and the message is the whole of what they get back (§5.9).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallsignError {
    /// Blank, or whitespace only.
    #[error(
        "a callsign is required and the configured value is blank. APRS transmits the \
         callsign on every packet, so there is no default that would be correct."
    )]
    Empty,

    /// The part after the `-` is not a number.
    #[error(
        "`{callsign}` has an SSID of `{ssid}`, which is not a number. An SSID is 0-15 and \
         the form is `CALL-SSID`, as in `W1AW-13`."
    )]
    BadSsid {
        /// What was configured.
        callsign: String,
        /// The part after the `-`.
        ssid: String,
    },

    /// A numeric SSID above 15.
    #[error(
        "`{callsign}` has an SSID of {ssid}, and an AX.25 address field holds 0-15. Drop \
         the SSID or choose one in range."
    )]
    SsidRange {
        /// What was configured.
        callsign: String,
        /// The parsed value.
        ssid: u8,
    },

    /// A base of zero characters, or more than six.
    #[error(
        "`{callsign}` has a base callsign of `{base}`, which is {} characters; an AX.25 \
         address field holds 1-6.", base.len()
    )]
    BaseLength {
        /// What was configured.
        callsign: String,
        /// The part before the `-`, uppercased.
        base: String,
    },

    /// A base character outside `[A-Z0-9]`.
    #[error(
        "`{callsign}` contains `{bad}`, and a base callsign is letters and digits only. \
         An AX.25 address field shifts each byte left one bit, so a character outside that \
         set does not survive the encoding."
    )]
    BaseCharacter {
        /// What was configured.
        callsign: String,
        /// The first offending character.
        bad: char,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callsign_is_uppercased_and_its_ssid_split_off() {
        let c = Callsign::parse(" w1aw-13 ").expect("parses");
        assert_eq!(c.base(), "W1AW");
        assert_eq!(c.ssid(), 13);
        assert_eq!(c.to_string(), "W1AW-13");

        let bare = Callsign::parse("n0call").expect("parses");
        assert_eq!(bare.ssid(), 0);
        // SSID 0 is not rendered, so a round trip through configuration is stable.
        assert_eq!(bare.to_string(), "N0CALL");
        assert_eq!(Callsign::parse("N0CALL-0").expect("parses"), bare);
    }

    /// The four refusals, each naming the configured text. The Go tree accepts all four on
    /// the APRS-IS path and rejects two of them on the KISS path.
    #[test]
    fn every_unencodable_callsign_is_refused_by_the_parse() {
        for (text, matches) in [
            ("", matches!(Callsign::parse(""), Err(CallsignError::Empty))),
            (
                "   ",
                matches!(Callsign::parse("   "), Err(CallsignError::Empty)),
            ),
            (
                "W1AW-X",
                matches!(
                    Callsign::parse("W1AW-X"),
                    Err(CallsignError::BadSsid { .. })
                ),
            ),
            (
                "W1AW-99",
                matches!(
                    Callsign::parse("W1AW-99"),
                    Err(CallsignError::SsidRange { .. })
                ),
            ),
            (
                "TOOLONG",
                matches!(
                    Callsign::parse("TOOLONG"),
                    Err(CallsignError::BaseLength { .. })
                ),
            ),
            (
                "-7",
                matches!(Callsign::parse("-7"), Err(CallsignError::BaseLength { .. })),
            ),
            (
                "W1*W",
                matches!(
                    Callsign::parse("W1*W"),
                    Err(CallsignError::BaseCharacter { .. })
                ),
            ),
        ] {
            assert!(matches, "{text:?} should not parse");
        }
        // An SSID of 256 does not fit a `u8`, and must still be the range error's story
        // rather than the "not a number" one as far as an operator is concerned: both
        // messages name the configured text and say what the range is.
        let e = Callsign::parse("W1AW-256").expect_err("out of range");
        assert!(e.to_string().contains("W1AW-256"), "{e}");
        assert!(e.to_string().contains("0-15"), "{e}");
    }

    /// All three CWOP prefixes, not just the `DW` the Go controller tests.
    #[test]
    fn every_cwop_prefix_is_recognised() {
        for text in ["CW1234", "DW1234", "EW1234", "dw0001"] {
            assert!(
                Callsign::parse(text).expect("parses").is_cwop(),
                "{text} is a CWOP identifier"
            );
        }
        // `EW1ABC` is a Belarusian amateur callsign and `CW5A` a Uruguayan one, so the
        // prefix alone would send the CWOP passcode for both and the login would fail.
        for text in [
            "W1AW", "KE7XYZ", "DX1ABC", "D1W234", "EW1ABC", "CW5A", "DW12",
        ] {
            assert!(
                !Callsign::parse(text).expect("parses").is_cwop(),
                "{text} is an amateur callsign"
            );
        }
    }

    /// Every message names the configured value and says what to do about it (§5.9).
    #[test]
    fn every_message_names_the_configured_value() {
        for text in ["", "W1AW-X", "W1AW-99", "TOOLONG", "W1*W"] {
            let e = Callsign::parse(text).expect_err("refused");
            assert!(e.to_string().len() > 60, "{e}");
            if !text.is_empty() {
                assert!(e.to_string().contains(text.trim()), "{e}");
            }
        }
    }
}
