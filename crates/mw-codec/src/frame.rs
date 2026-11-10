//! [`Frame`], [`Codec`] and [`CodecError`] — the pure half of W2a §4a.2.
//!
//! A frame is bytes a transport produced and a session delimited. It carries no socket, no
//! peer address and no arrival time: everything a decoder is allowed to know about *who
//! sent this* is in `mw_component::FrameMeta`, which the impure half builds, and everything
//! it is allowed to know about *when* is either inside the payload or supplied by
//! admission. That split is the reason this crate can be golden-tested with no hardware,
//! and it is the one property worth protecting (W2a §4a.2).

use std::fmt;

use mw_types::{PartialReading, StationKey};

/// One delimited unit of transport output: a UDP datagram, an HTTP response body, a Davis
/// `LOOP` packet, a line of Campbell ASCII.
///
/// Owned rather than borrowed. A borrowed frame would make `Session::next_frame` return a
/// view into a buffer the session still owns, which is exactly the lifetime a receive loop
/// cannot satisfy once it reads the next datagram into the same buffer.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    payload: Vec<u8>,
}

impl Frame {
    /// Wrap a delimited payload.
    pub fn new(payload: impl Into<Vec<u8>>) -> Self {
        Self {
            payload: payload.into(),
        }
    }

    /// The bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.payload
    }

    /// The bytes as text, for a codec over a textual protocol.
    ///
    /// An error rather than a lossy conversion: a frame that is not the text this codec
    /// decodes is a counted drop, and replacing a bad byte with `U+FFFD` would turn it into
    /// a reading with a plausible wrong value in it.
    pub fn as_str(&self) -> Result<&str, CodecError> {
        std::str::from_utf8(&self.payload).map_err(|_| CodecError::NotText)
    }

    /// How many bytes the frame carries.
    pub fn len(&self) -> usize {
        self.payload.len()
    }

    /// Whether the frame is empty. A transport can deliver a zero-length datagram.
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

impl fmt::Debug for Frame {
    /// The length and nothing else. A frame can carry a device credential — WeatherLink
    /// Live does not, but the Ambient query string does (`?PASSWORD=`) — and a `Debug` that
    /// printed the payload would put it in a log line the moment a decode failed, which is
    /// the defect **D-33** exists against.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("len", &self.payload.len())
            .finish()
    }
}

/// Why a frame did not decode.
///
/// Every variant is a **counted drop**, never a component failure: a station that sends one
/// malformed datagram has not broken the driver, and the Go WeatherLink Live receiver's
/// answer — `logger.Errorf("JSON parse error: %v", err)` once per packet — is a log line
/// per second on a device with a firmware quirk and a number nobody can alert on.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The frame is not UTF-8, and this codec decodes text.
    #[error("the frame is not text")]
    NotText,

    /// The frame is not the JSON document this codec decodes.
    #[error("the frame is not the JSON this codec decodes")]
    Json {
        /// The parse failure, with its position.
        #[source]
        source: serde_json::Error,
    },

    /// The payload is well-formed and carries a device-reported error instead of data.
    ///
    /// Distinct from [`CodecError::Json`] because the two mean opposite things to an
    /// operator: one is "this is not a WeatherLink Live", the other is "it is, and it is
    /// telling you something".
    #[error("the device reported error {code}: {message}")]
    Device {
        /// The device's own code.
        code: i64,
        /// The device's own message.
        message: String,
    },

    /// The device answered the right shape with the wrong contents — a field the caller
    /// needs is absent or unusable.
    ///
    /// Distinct from [`CodecError::NoRecords`] because the two are different sentences to an
    /// operator. An arming response with no broadcast port used to report "the frame carries
    /// no sensor records", which the driver then wrapped as "reading a WeatherLink Live
    /// arming response" — so somebody debugging a broadcast that never starts got a sentence
    /// about sensors.
    #[error("the device's answer carries no {what}")]
    Incomplete {
        /// What was missing, named as the operator would name it.
        what: &'static str,
    },

    /// The frame is not the length this protocol frames.
    ///
    /// Distinct from [`CodecError::Incomplete`] because a fixed-length binary protocol has
    /// no partial record: a Davis LOOP packet is ninety-nine bytes with no terminator, so a
    /// short read is a framing failure in the session above rather than a device reporting
    /// less than it has, and the two numbers are what tell them apart.
    #[error("{what} is {expected} bytes and this frame is {found}")]
    Framing {
        /// What was expected, named as the operator would name it.
        what: &'static str,
        /// How many bytes one frame is.
        expected: usize,
        /// How many arrived.
        found: usize,
    },

    /// The frame's own check bytes do not match its contents.
    ///
    /// Distinct from every other variant because it is the one that means **the bytes were
    /// corrupted in transit**: a Davis console on a long serial run with a marginal cable
    /// produces these and nothing else, and `undecodable` without this distinction would
    /// make "a flaky cable" and "not a Davis" the same number.
    #[error("{what} failed its own checksum")]
    Check {
        /// What was being checked.
        what: &'static str,
    },

    /// The frame is a record of a kind this build does not decode.
    ///
    /// A Davis LOOP2 record shares the LOOP record's header and length and has a different
    /// field layout, so decoding one as the other produces plausible wrong numbers rather
    /// than an error. Refusing it by its own discriminant is what makes that a counted drop.
    #[error("{what} {found} is not one this codec decodes")]
    Unsupported {
        /// Which discriminant was read.
        what: &'static str,
        /// What it held.
        found: i64,
    },

    /// The frame is not the protobuf message this codec decodes.
    ///
    /// Distinct from [`CodecError::Json`] for its reason: two wire formats, two parsers, and
    /// a decoder that reported "not JSON" for a protobuf frame would send a reader to the
    /// wrong half of the system.
    #[error("the frame is not the protobuf message this codec decodes")]
    Protobuf {
        /// The decode failure.
        #[source]
        source: prost::DecodeError,
    },

    /// The frame decoded, and what it decoded to is a fault the station's own configuration
    /// says it cannot be.
    ///
    /// The one variant that is not about the *bytes*. A snow gauge reporting a distance
    /// greater than its configured base distance has not sent a malformed frame; it has sent
    /// a measurement that would store a negative depth, and `snowgauge/station.go:171`
    /// discards it at the device. Carried as an error so that the drop is counted with a
    /// reason an operator can act on rather than returned as an empty batch, which is
    /// indistinguishable from a frame that mapped nothing.
    #[error("{what} is {value} and the station's own limit is {limit}")]
    Implausible {
        /// What was measured, named as the operator would name it.
        what: &'static str,
        /// What the station reported.
        value: f64,
        /// The limit its configuration declares.
        limit: f64,
    },

    /// The frame is structurally valid and carries nothing this codec can map.
    ///
    /// Not an empty *reading* — that is admission's [`mw_component::Reject::EmptyReading`]
    /// — but an empty *frame*: a `conditions` array with no records in it.
    #[error("the frame carries no sensor records")]
    NoRecords,
}

impl mw_types::Classify for CodecError {
    /// All ten are [`mw_types::Class::Ignorable`], and that is the whole of this impl's
    /// content: retrying a decode of the same bytes against the same codec cannot change
    /// the answer, and a frame the device sent wrong is not a failure of the component that
    /// received it (§4.4's canonical example).
    fn class(&self) -> mw_types::Class {
        mw_types::Class::Ignorable
    }
}

/// A pure decoder: bytes in, readings out.
///
/// **No I/O, no clock, no connection** — enforced by `cargo xtask gate layout`, which
/// rejects this crate naming a runtime and rejects the four source patterns a clock or a
/// socket would need (`ci/layers.toml`, `mw-codec`). That is not a style rule: a codec that
/// can read the time is not golden-testable, because the value it produces then depends on
/// when the test ran.
///
/// `Send + Sync` because a driver holds one for the life of the component and a `Serve`
/// body moves across an await; `Debug` because the workspace lints require it and because
/// "which codec refused this frame" is a useful log field.
pub trait Codec: fmt::Debug + Send + Sync + 'static {
    /// A stable, lower-case name for a log field and a test assertion — `wll-conditions`.
    fn name(&self) -> &'static str;

    /// Decode one frame.
    ///
    /// `Vec<PartialReading>` and not one reading: a Davis `LOOP n` response and a batched
    /// vendor-cloud poll both carry several observations in one frame (W2a §4a.2).
    fn decode(&self, frame: &Frame) -> Result<Vec<PartialReading>, CodecError>;

    /// The station identity the payload claims, for `mw_station`'s
    /// `StationIdentity::FromPayload`.
    ///
    /// W2a §4a.4 writes this strategy as `FromPayload(fn(&Frame) -> Option<StationKey>)`. A
    /// method on the codec rather than a function pointer beside it, because the codec is
    /// the only thing in the system that is allowed to read a payload *and* pure enough to
    /// be trusted with it — a second parser for the same bytes, stored next to the first,
    /// is how the two disagree.
    ///
    /// The default is `None`: most protocols carry no identity, and a driver whose identity
    /// is `Fixed` never asks.
    fn claimed_station(&self, frame: &Frame) -> Option<StationKey> {
        let _ = frame;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_types::{Class, Classify};

    #[test]
    fn a_frame_hides_its_payload_from_debug() {
        let rendered = format!("{:?}", Frame::new(b"ID=roof&PASSWORD=hunter2".to_vec()));
        assert!(rendered.contains("len: 24"), "{rendered}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
    }

    #[test]
    fn a_non_text_frame_is_refused_rather_than_mangled() {
        let frame = Frame::new(vec![0x7b, 0xff, 0x7d]);
        assert!(matches!(frame.as_str(), Err(CodecError::NotText)));
        assert_eq!(frame.len(), 3);
        assert!(!frame.is_empty());
        assert!(Frame::new(Vec::new()).is_empty());
    }

    #[test]
    fn every_decode_failure_is_ignorable() {
        assert_eq!(CodecError::NotText.class(), Class::Ignorable);
        assert_eq!(CodecError::NoRecords.class(), Class::Ignorable);
        assert_eq!(
            CodecError::Incomplete {
                what: "broadcast port"
            }
            .class(),
            Class::Ignorable
        );
        assert_eq!(
            CodecError::Device {
                code: 409,
                message: "busy".into()
            }
            .class(),
            Class::Ignorable
        );
        assert_eq!(
            CodecError::Framing {
                what: "a Davis LOOP record",
                expected: 99,
                found: 40
            }
            .class(),
            Class::Ignorable
        );
        assert_eq!(
            CodecError::Check {
                what: "a Davis LOOP record"
            }
            .class(),
            Class::Ignorable
        );
        assert_eq!(
            CodecError::Unsupported {
                what: "LOOP record type",
                found: 1
            }
            .class(),
            Class::Ignorable
        );
    }

    /// Each of the three framing-class variants says which it is, because `undecodable`
    /// with no sentence behind it cannot tell a flaky cable from a device that is not the
    /// one configured.
    #[test]
    fn the_three_binary_framing_failures_read_as_three_different_sentences() {
        assert_eq!(
            CodecError::Framing {
                what: "a Davis LOOP record",
                expected: 99,
                found: 40
            }
            .to_string(),
            "a Davis LOOP record is 99 bytes and this frame is 40"
        );
        assert_eq!(
            CodecError::Check {
                what: "a Davis LOOP record"
            }
            .to_string(),
            "a Davis LOOP record failed its own checksum"
        );
        assert_eq!(
            CodecError::Unsupported {
                what: "LOOP record type",
                found: 1
            }
            .to_string(),
            "LOOP record type 1 is not one this codec decodes"
        );
    }
}
