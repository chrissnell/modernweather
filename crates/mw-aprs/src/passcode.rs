//! [`Passcode`] — the APRS-IS login credential, and the one piece of `pkg/aprs` that
//! already existed in Go.
//!
//! The algorithm is unchanged: initialise at `0x73E2`, fold the uppercase **base** callsign
//! in character pairs with `code ^= c1 << 8; code ^= c2`, mask to `0x7FFF`. Two things
//! around it are not.
//!
//! **`CalculatePasscodeDebug` is not carried forward.** It is a 36-line copy of the same
//! loop whose only difference is twelve calls that narrate the arithmetic to standard
//! output — a path `ci/layers.toml`'s `no-stdout-logging` rule forbids outright, and one
//! that prints the credential. A second implementation of a one-line XOR is also a second
//! place for it to be wrong. What it was for is served by [`Passcode::for_callsign`]'s unit
//! tests, which pin the published vectors rather than narrating an arithmetic.
//!
//! **The CWOP rule and the configured override are resolved once, here.** The Go tree
//! decides between the three sources in two places — `sendReadingToAPRSIS` and
//! `testAPRSISLogin` — and they disagree: the first honours `device.APRSPasscode` and the
//! second ignores it, so a station with an assigned passcode is health-checked with a
//! computed one and reports unhealthy while uploading fine. [`Passcode::resolve`] is the
//! one answer.
//!
//! # Why this is a redacting wrapper and not a `secrecy::SecretString`
//!
//! **D-33** asks for a wrapper whose `Debug` and `Display` redact, which this is. It is not
//! `secrecy`'s type for two reasons: this crate is pure and `secrecy` is not among the
//! dependencies `ci/layers.toml` leaves it (one more crate in a golden-testable protocol
//! library buys nothing), and the value is an integer that a login line formats — a
//! `SecretString` would be constructed by formatting the integer, which puts the credential
//! in a `String` the wrapper then guards. There is no comparison path here, so D-33's
//! constant-time half has nothing to apply to.
//!
//! The honest part of the redaction, stated because a reader should not over-trust it: a
//! computed passcode is a **pure function of a public callsign**, so redacting it protects
//! a log file rather than a secret. What it does protect is the assigned passcode a CWOP or
//! APRS-IS registration issues, which [`Passcode::parse`] is the path for, and the habit —
//! a credential type that is sometimes printable is one that gets printed.

use std::fmt;

use crate::callsign::Callsign;

/// The algorithm's initial value.
const SEED: i32 = 0x73E2;

/// The mask the result is reduced by: APRS-IS passcodes are 15 bits.
const MASK: i32 = 0x7FFF;

/// An APRS-IS login passcode.
///
/// No `Display`, no `Debug` of the value and no `PartialEq`: the only way to the number is
/// [`Passcode::expose`], which reads as what it is at the call site that formats the login
/// line. Equality is deliberately absent because comparing credentials is not this type's
/// job and `==` on a secret is how a non-constant-time comparison gets written (**D-33**).
#[derive(Clone, Copy)]
pub struct Passcode(i32);

impl Passcode {
    /// What a CWOP-issued station logs in with.
    ///
    /// Negative, which is the protocol's way of saying "receive-only credentials": a CWOP
    /// station may inject its own weather reports and nothing else.
    pub const CWOP: Passcode = Passcode(-1);

    /// Compute the passcode for a callsign.
    ///
    /// Over the **base** callsign, uppercase, which [`Callsign`] has already established —
    /// the Go version re-derives both with `strings.ToUpper(strings.Split(c, "-")[0])`
    /// inside the algorithm, so a caller holding a parsed callsign could still pass the
    /// wrong thing.
    pub fn for_callsign(callsign: &Callsign) -> Self {
        let bytes = callsign.base().as_bytes();
        let mut code = SEED;
        for pair in bytes.chunks(2) {
            code ^= i32::from(pair[0]) << 8;
            // An odd-length callsign folds its last character against a zero, which is
            // what `var c2 byte = 0` does in the Go loop and what every other
            // implementation of this algorithm does.
            code ^= i32::from(pair.get(1).copied().unwrap_or(0));
        }
        Passcode(code & MASK)
    }

    /// An assigned passcode, as configured.
    ///
    /// `-1` is accepted because it is what a CWOP registration issues; anything else
    /// outside 15 bits is a typo rather than a credential, and refusing it at plan time is
    /// the difference between a named configuration error and a login the server rejects
    /// every five minutes forever.
    pub fn parse(text: &str) -> Result<Self, PasscodeError> {
        let text = text.trim();
        let value = text.parse::<i32>().map_err(|_| PasscodeError::NotANumber {
            len: text.chars().count(),
        })?;
        if value != -1 && !(0..=MASK).contains(&value) {
            return Err(PasscodeError::OutOfRange);
        }
        Ok(Passcode(value))
    }

    /// The passcode this station logs in with, from the three sources in precedence order.
    ///
    /// 1. **A configured passcode**, if there is one. It is the only source that can carry
    ///    an assignment this program cannot derive, so nothing may override it.
    /// 2. **[`Passcode::CWOP`]** for a callsign that looks CWOP-issued
    ///    ([`Callsign::is_cwop`]).
    /// 3. **The computed passcode** for an amateur callsign.
    ///
    /// The Go order is 2, 1, 3 in one call site and 2, 3 in the other. Putting the
    /// configured value first is the behaviour change, and it is the one that makes the
    /// column mean something: a CWOP-prefixed callsign with a passcode in the database is
    /// currently a passcode nothing reads.
    pub fn resolve(callsign: &Callsign, configured: Option<&str>) -> Result<Self, PasscodeError> {
        match configured.map(str::trim).filter(|t| !t.is_empty()) {
            Some(text) => Passcode::parse(text),
            None if callsign.is_cwop() => Ok(Passcode::CWOP),
            None => Ok(Passcode::for_callsign(callsign)),
        }
    }

    /// The number, for the one line that writes it to a socket.
    pub fn expose(self) -> i32 {
        self.0
    }
}

impl fmt::Debug for Passcode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Passcode(<redacted>)")
    }
}

/// Why a configured passcode is not one.
///
/// **Neither variant quotes the configured text**, which is the one place a credential
/// error message has to be less helpful than §5.9 would otherwise ask: an error that echoes
/// a mistyped passcode puts it in the log, and a mistyped passcode is usually a correct one
/// with a character wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasscodeError {
    /// Not an integer.
    #[error(
        "the configured APRS passcode is not a number ({len} characters). It is the \
         number APRS-IS or CWOP issued for this callsign; leave the column NULL to have it \
         computed from the callsign instead. The value is not quoted here on purpose."
    )]
    NotANumber {
        /// How long it was, which is enough to tell a blank-looking value from a long one
        /// without reproducing it.
        len: usize,
    },

    /// A number outside the protocol's range.
    #[error(
        "the configured APRS passcode is outside the range APRS-IS issues (0-32767, or -1 \
         for a CWOP station). Check it against the registration, or leave the column NULL \
         to have it computed from the callsign."
    )]
    OutOfRange,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(text: &str) -> Callsign {
        Callsign::parse(text).expect("parses")
    }

    /// The published vectors, which is what `CalculatePasscodeDebug` was for.
    ///
    /// Computed independently from the algorithm's statement — seed `0x73E2`, pairwise
    /// `^= c1 << 8` then `^= c2`, masked to 15 bits — rather than copied from a run of
    /// this code.
    #[test]
    fn the_passcode_matches_the_published_algorithm() {
        // 0x73E2 ^ ('W'<<8 ^ '1') ^ ('A'<<8 ^ 'W') = 0x73E2 ^ 0x5731 ^ 0x4157, masked.
        let expected = (0x73E2 ^ 0x5731 ^ 0x4157) & 0x7FFF;
        assert_eq!(Passcode::for_callsign(&call("W1AW")).expose(), expected);

        // An odd-length base folds its last character against zero.
        // 0x73E2 ^ ('K'<<8 ^ 'E') ^ ('7'<<8 ^ 'X') ^ ('Y'<<8 ^ 0)
        let odd = (0x73E2 ^ 0x4B45 ^ 0x3758 ^ 0x5900) & 0x7FFF;
        assert_eq!(Passcode::for_callsign(&call("KE7XY")).expose(), odd);
    }

    /// The SSID does not change the passcode, and neither does the case an operator typed.
    #[test]
    fn the_passcode_is_over_the_base_callsign_only() {
        let bare = Passcode::for_callsign(&call("KE7XYZ")).expose();
        assert_eq!(Passcode::for_callsign(&call("KE7XYZ-13")).expose(), bare);
        assert_eq!(Passcode::for_callsign(&call("ke7xyz")).expose(), bare);
    }

    /// Every result is 15 bits, over a corpus wide enough that a sign or shift mistake
    /// shows up.
    #[test]
    fn every_passcode_fits_the_protocols_fifteen_bits() {
        for base in [
            "A", "ZZ", "9", "W1AW", "KE7XYZ", "N0CALL", "ZZ9ZZZ", "000000",
        ] {
            let p = Passcode::for_callsign(&call(base)).expose();
            assert!((0..=0x7FFF).contains(&p), "{base} produced {p}");
        }
    }

    /// The precedence the Go tree's two call sites disagree about.
    #[test]
    fn a_configured_passcode_wins_over_the_cwop_default() {
        let cwop = call("DW1234");
        assert_eq!(
            Passcode::resolve(&cwop, None).expect("resolves").expose(),
            -1
        );
        // A blank column is not a configured passcode.
        assert_eq!(
            Passcode::resolve(&cwop, Some("  "))
                .expect("resolves")
                .expose(),
            -1
        );
        assert_eq!(
            Passcode::resolve(&cwop, Some("12345"))
                .expect("resolves")
                .expose(),
            12345
        );

        let ham = call("W1AW");
        assert_eq!(
            Passcode::resolve(&ham, None).expect("resolves").expose(),
            Passcode::for_callsign(&ham).expose()
        );
        assert_eq!(
            Passcode::resolve(&ham, Some("-1"))
                .expect("resolves")
                .expose(),
            -1
        );
    }

    /// A typo is a refusal at plan time, not a login the server rejects forever.
    #[test]
    fn an_unusable_configured_passcode_is_refused() {
        // `Passcode` has no `PartialEq` on purpose (see the type's documentation), so the
        // refusals are matched rather than compared.
        assert_eq!(
            Passcode::parse("nope").expect_err("refused"),
            PasscodeError::NotANumber { len: 4 }
        );
        assert_eq!(
            Passcode::parse("32768").expect_err("refused"),
            PasscodeError::OutOfRange
        );
        assert_eq!(
            Passcode::parse("-2").expect_err("refused"),
            PasscodeError::OutOfRange
        );
        assert_eq!(Passcode::parse("0").expect("in range").expose(), 0);
        assert_eq!(
            Passcode::parse(" 32767 ").expect("in range").expose(),
            32767
        );
    }

    /// The credential does not reach a log line through `Debug`, and the error messages do
    /// not carry the value either.
    #[test]
    fn the_credential_redacts_and_the_errors_do_not_quote_it() {
        let p = Passcode::for_callsign(&call("W1AW"));
        let rendered = format!("{p:?}");
        assert_eq!(rendered, "Passcode(<redacted>)");
        assert!(!rendered.contains(&p.expose().to_string()));
        // `Passcode` has no `Display`, so `{}` does not compile; `{:?}` is the only way to
        // put one in a message and it redacts. The same property for the errors:
        // The structural property, which is what actually guarantees it: neither variant
        // has a field that could carry the configured text. `NotANumber` carries a length
        // and `OutOfRange` carries nothing, so there is no value of the input that could
        // reach either message -- an assertion over one literal would not say that.
        for e in [
            PasscodeError::NotANumber { len: 7 },
            PasscodeError::OutOfRange,
        ] {
            assert!(e.to_string().len() > 60, "{e}");
            assert!(!e.to_string().contains("passcode is 7"), "{e}");
        }
        assert_eq!(
            Passcode::parse("hunter2").expect_err("refused"),
            PasscodeError::NotANumber { len: 7 },
            "the only thing the error carries about the input is how long it was"
        );
    }
}
