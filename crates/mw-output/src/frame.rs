//! [`FrameSink`] — one information field, two framings, three transports.
//!
//! > *The split falls at `encode_info() -> InformationField` (pure, shared by all three) and
//! > `FrameSink::send(&InformationField)` (framing + transport, three implementations, two
//! > framings).*
//! > — `docs/component-model.md` §4b.4
//!
//! # Transport dispatch is exhaustive, because in Go it is a live bug
//!
//! ```text
//! // internal/controllers/aprs/controller.go:378
//! if strings.ToLower(device.APRSTransport) == transportKISS {
//!     a.sendReadingViaKISS(ctx, device, reading, windGustResult.WindGust)
//!     return
//! }
//! a.sendReadingToAPRSIS(ctx, device, reading, windGustResult.WindGust)
//! ```
//!
//! Two branches for three transports, and the `default:` arm is the dangerous one: **an
//! unrecognised transport string silently transmits over APRS-IS**, so
//! `aprs_transport = "kis"` puts the weather report on the internet instead of the radio,
//! with no error anywhere. The third distinction — KISS over serial against KISS over TCP —
//! is decided much deeper, inside `openKISSConnection`, on a *second* stringly-typed field
//! whose own `default:` arm is TCP.
//!
//! Here the three are an enum, parsed from configuration by `crate::plan` against the
//! `uploader` table's `CHECK (aprs_transport IN ('aprs-is', 'kiss-serial', 'kiss-tcp'))`,
//! so an unknown transport is a **plan error** before anything transmits.
//!
//! # The two KISS variants are one implementation
//!
//! [`FrameSink::transmit`] opens a `mw_station::ByteStream` and writes to it; whether that
//! is a character device or a socket is `mw_station::stream::open`'s business. The Go code
//! half-discovered this already — `openKISSConnection` returns an `io.ReadWriteCloser` and
//! both arms then run identical framing — and the reason to finish the thought here is that
//! the serial open is the one `unsafe` block in this workspace
//! (`mw_station::stream::serial`, which registers an `AsyncFd` over an owned `File`). A
//! second copy of it in this crate would be a second safety argument to audit.
//!
//! That is also the whole of why this crate depends on `mw-station`: a `Transport`, a
//! `ByteStream` and the function that turns one into the other. It plans no station, decodes
//! no frame and resolves no identity.
//!
//! # A connection per occurrence
//!
//! Every transport here opens, writes and closes. The Go comment says why and is right —
//! *"Connections are opened per report to match the APRS-IS path"* — and under this model
//! there is a second reason: `tower`'s retry layer sits **outside** the rate limiter
//! (**D-57**), so an attempt is the unit that is retried, and an attempt that reused a
//! connection would have to decide what a half-written frame on a reused socket means.
//! At one frame every ten minutes there is nothing to amortise.

use std::path::PathBuf;
use std::sync::Arc;

use mw_aprs::{Callsign, InformationField, Passcode};
use mw_station::session::ByteStream;
use mw_station::transport::Transport;
use tracing::debug;

use crate::error::{Target, UploadError};
use crate::interval::RadioInterval;
use crate::uploader::Transmit;

/// KISS framing bytes.
mod kiss {
    /// Frame delimiter.
    pub const FEND: u8 = 0xC0;
    /// Frame escape.
    pub const FESC: u8 = 0xDB;
    /// Transposed frame end.
    pub const TFEND: u8 = 0xDC;
    /// Transposed frame escape.
    pub const TFESC: u8 = 0xDD;
    /// A KISS data frame on port 0.
    pub const CMD_DATA: u8 = 0x00;
}

/// AX.25 framing bytes.
mod ax25 {
    /// An unnumbered information frame.
    pub const CONTROL_UI: u8 = 0x03;
    /// Protocol ID: no layer 3.
    pub const PID_NO_L3: u8 = 0xF0;
    /// The reserved bits of an SSID octet, set to one by convention.
    pub const SSID_RESERVED: u8 = 0x60;
    /// The command/response bit, set on the destination address of a UI command frame.
    pub const SSID_COMMAND: u8 = 0x80;
    /// The HDLC address-extension bit, set on the final address field.
    pub const SSID_LAST: u8 = 0x01;
}

/// The longest line this reads from APRS-IS.
///
/// A server greeting is ~60 bytes and a login reply ~50. The cap exists because the peer is
/// on the internet: a bounded read is the difference between a misbehaving server and a
/// daemon that allocates until it dies.
pub const MAX_LINE_BYTES: usize = 1024;

/// The APRS-IS pseudo-path an injected packet carries.
///
/// `TCPIP*` — the `*` is the has-been-repeated bit, which is set because the packet reached
/// the network through a server rather than over the air. The Go header writes `TCPIP`
/// without it; APRS-IS accepts either, and the form with the asterisk is what the
/// specification's APRS-IS section and every other client use.
pub const APRS_IS_PATH: &str = "TCPIP*";

/// Where a frame goes.
#[derive(Debug, Clone)]
pub enum FrameSink {
    /// The internet, over a plain TCP connection with a login.
    AprsIs {
        /// The server's host.
        host: String,
        /// Its port.
        port: u16,
        /// The callsign this beacons as, and logs in as.
        source: Callsign,
        /// The login credential.
        passcode: Passcode,
        /// What identifies this software in the login line.
        software: String,
    },
    /// A TNC on a character device.
    KissSerial {
        /// The device path.
        dev: PathBuf,
        /// The configured line rate. **Recorded and not applied** — see
        /// `mw_station::stream`'s module documentation and the D-32 amendment it names.
        baud: u32,
        /// The callsign this beacons as.
        source: Callsign,
        /// The AX.25 destination, conventionally `APRS`.
        dest: Callsign,
        /// The digipeater path.
        path: Vec<Callsign>,
        /// The beacon interval, which cannot be below the RF floor.
        interval: RadioInterval,
    },
    /// A TNC on a TCP socket.
    KissTcp {
        /// The TNC's host.
        host: String,
        /// Its port.
        port: u16,
        /// The callsign this beacons as.
        source: Callsign,
        /// The AX.25 destination.
        dest: Callsign,
        /// The digipeater path.
        path: Vec<Callsign>,
        /// The beacon interval, which cannot be below the RF floor.
        interval: RadioInterval,
    },
}

impl FrameSink {
    /// The default APRS-IS server, which is what `aprs_server` being NULL means.
    pub const DEFAULT_APRS_IS: (&'static str, u16) = ("noam.aprs2.net", 14580);

    /// The default AX.25 destination.
    pub const DEFAULT_DEST: &'static str = "APRS";

    /// The default digipeater path.
    pub const DEFAULT_PATH: &'static str = "WIDE1-1,WIDE2-1";

    /// The default KISS line rate.
    pub const DEFAULT_BAUD: u32 = 9600;

    /// Which transport this is, for a metric label and a log field.
    pub fn kind(&self) -> &'static str {
        match self {
            FrameSink::AprsIs { .. } => "aprs-is",
            FrameSink::KissSerial { .. } => "kiss-serial",
            FrameSink::KissTcp { .. } => "kiss-tcp",
        }
    }

    /// Whether this is a radio, which is what makes the interval floor a type rather than
    /// a warning.
    pub fn is_radio(&self) -> bool {
        matches!(
            self,
            FrameSink::KissSerial { .. } | FrameSink::KissTcp { .. }
        )
    }

    /// Where this sends, for an error message.
    pub fn target(&self) -> Target {
        match self {
            FrameSink::AprsIs { host, port, .. } | FrameSink::KissTcp { host, port, .. } => {
                Target::peer(host, *port)
            }
            FrameSink::KissSerial { dev, .. } => Target::device(&dev.to_string_lossy()),
        }
    }

    /// The transport `mw_station::stream::open` opens for this sink.
    fn transport(&self) -> Transport {
        match self {
            FrameSink::AprsIs { host, port, .. } | FrameSink::KissTcp { host, port, .. } => {
                Transport::TcpOut {
                    host: host.clone(),
                    port: *port,
                    // APRS-IS's TLS ports are a different service with a different login
                    // and are not configured anywhere in this schema; a TNC has no TLS at
                    // all.
                    tls: None,
                }
            }
            FrameSink::KissSerial { dev, baud, .. } => Transport::Serial {
                dev: dev.clone(),
                baud: *baud,
            },
        }
    }
}

/// Build the TNC2 text header and append the information field.
///
/// `SOURCE>DEST,PATH:INFO`, which is what APRS-IS reads. One function rather than a method
/// so that a golden test over it needs no sink.
pub fn tnc2(source: &Callsign, dest: &str, path: &str, info: &InformationField) -> String {
    if path.is_empty() {
        format!("{source}>{dest}:{info}")
    } else {
        format!("{source}>{dest},{path}:{info}")
    }
}

/// Build an AX.25 UI frame carrying the information field.
///
/// Address order on the wire is destination, source, then digipeaters; the command bit is
/// set on the destination and the extension bit on whichever address is last. Carried over
/// from `pkg/kiss`'s `EncodeAX25UI`, whose golden vector is a test below — the validation it
/// did on callsigns has moved into [`Callsign`], so this function cannot fail.
pub fn ax25_ui(
    source: &Callsign,
    dest: &Callsign,
    path: &[Callsign],
    info: &InformationField,
) -> Vec<u8> {
    let mut addresses: Vec<&Callsign> = Vec::with_capacity(2 + path.len());
    addresses.push(dest);
    addresses.push(source);
    addresses.extend(path.iter());

    let mut frame = Vec::with_capacity(addresses.len() * 7 + 2 + info.len());
    let last = addresses.len() - 1;
    for (i, address) in addresses.iter().enumerate() {
        encode_address(&mut frame, address, i == last, i == 0);
    }
    frame.push(ax25::CONTROL_UI);
    frame.push(ax25::PID_NO_L3);
    frame.extend_from_slice(info.as_bytes());
    frame
}

/// One AX.25 address field: six space-padded callsign bytes shifted left one bit, then the
/// SSID octet.
fn encode_address(out: &mut Vec<u8>, callsign: &Callsign, last: bool, command: bool) {
    let base = callsign.base().as_bytes();
    for i in 0..6 {
        let byte = base.get(i).copied().unwrap_or(b' ');
        out.push(byte << 1);
    }
    let mut ssid = ax25::SSID_RESERVED | (callsign.ssid() << 1);
    if command {
        ssid |= ax25::SSID_COMMAND;
    }
    if last {
        ssid |= ax25::SSID_LAST;
    }
    out.push(ssid);
}

/// Wrap a frame in KISS: `FEND`, the command byte, the escaped payload, `FEND`.
pub fn kiss_frame(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + 4);
    out.push(kiss::FEND);
    out.push(kiss::CMD_DATA);
    for byte in frame {
        match *byte {
            kiss::FEND => out.extend_from_slice(&[kiss::FESC, kiss::TFEND]),
            kiss::FESC => out.extend_from_slice(&[kiss::FESC, kiss::TFESC]),
            other => out.push(other),
        }
    }
    out.push(kiss::FEND);
    out
}

/// Open the sink's transport, frame the field and write it.
///
/// A free function over an `Arc<FrameSink>` rather than a method, because `Transmit` is
/// implemented for [`SharedSink`] alone: a `FrameSink` holds a `Vec<Callsign>` and a
/// `PathBuf`, and the retry layer clones the service once per attempt.
async fn send(sink: Arc<FrameSink>, info: InformationField) -> Result<(), UploadError> {
    let target = sink.target();
    let mut stream = open(&sink, &target).await?;
    match sink.as_ref() {
        FrameSink::AprsIs {
            source,
            passcode,
            software,
            ..
        } => {
            login(stream.as_mut(), source, *passcode, software, &target).await?;
            let line = tnc2(source, FrameSink::DEFAULT_DEST, APRS_IS_PATH, &info);
            debug!(peer = %target, bytes = line.len(), "injecting an APRS-IS packet");
            write_line(stream.as_mut(), &line, &target).await
        }
        FrameSink::KissSerial {
            source, dest, path, ..
        }
        | FrameSink::KissTcp {
            source, dest, path, ..
        } => {
            let frame = kiss_frame(&ax25_ui(source, dest, path, &info));
            debug!(peer = %target, bytes = frame.len(), "sending a KISS frame");
            stream
                .write_all(&frame)
                .await
                .map_err(|cause| UploadError::Connect {
                    what: "could not write a KISS frame to",
                    target: target.clone(),
                    source: cause,
                })
        }
    }
}

/// Open the sink's byte stream.
async fn open(sink: &FrameSink, target: &Target) -> Result<Box<dyn ByteStream>, UploadError> {
    mw_station::stream::open(&sink.transport(), sink.kind())
        .await
        .map_err(|e| UploadError::Connect {
            what: "could not open",
            target: target.clone(),
            // `OpenError`'s causes are an `io::Error` in every variant this sink can
            // produce; the two that are not are refused at plan time by
            // `mw_station::plan` and are unreachable from a `Transport` built above.
            source: std::io::Error::other(mw_types::DisplayChain(&e).to_string()),
        })
}

/// Run the APRS-IS login conversation.
///
/// Greeting, login line, reply, and the reply must say `verified`. Takes a `&mut dyn
/// ByteStream` rather than a socket so that `mw_station::testing::Transcript` can play the
/// server — which is what makes every refusal below testable without touching the public
/// network, and the item's own instruction: *a test that actually posts to CWOP is a test
/// that pollutes a public network with fabricated observations from the owner's callsign.*
pub async fn login(
    stream: &mut dyn ByteStream,
    source: &Callsign,
    passcode: Passcode,
    software: &str,
    target: &Target,
) -> Result<(), UploadError> {
    let mut reader = LineReader::new(stream);
    let greeting = reader.next_line(target, "an APRS-IS greeting").await?;
    if !greeting.starts_with('#') {
        return Err(UploadError::Malformed {
            target: target.clone(),
            expected: "an APRS-IS greeting",
            why: "the first line did not begin with `#`",
        });
    }
    // The passcode leaves `Passcode` here and nowhere else. The line is not logged.
    let line = format!("user {source} pass {} vers {software}", passcode.expose());
    write_line(&mut *reader.stream, &line, target).await?;
    let reply = reader.next_line(target, "an APRS-IS login reply").await?;
    if !reply.starts_with('#') {
        return Err(UploadError::Malformed {
            target: target.clone(),
            expected: "an APRS-IS login reply",
            why: "the reply did not begin with `#`",
        });
    }
    // The server says `# logresp CALL verified, server X` on success and
    // `# logresp CALL unverified, server X` on failure. Matching `verified` as a word
    // rather than a substring, because `unverified` contains it.
    let verified = reply
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word.eq_ignore_ascii_case("verified"));
    if !verified {
        return Err(UploadError::LoginRefused {
            target: target.clone(),
            response: reply,
        });
    }
    Ok(())
}

/// Write one CRLF-terminated line.
async fn write_line(
    stream: &mut dyn ByteStream,
    line: &str,
    target: &Target,
) -> Result<(), UploadError> {
    let mut bytes = Vec::with_capacity(line.len() + 2);
    bytes.extend_from_slice(line.as_bytes());
    bytes.extend_from_slice(b"\r\n");
    stream
        .write_all(&bytes)
        .await
        .map_err(|source| UploadError::Connect {
            what: "could not write to",
            target: target.clone(),
            source,
        })
}

/// A buffered line reader over a [`ByteStream`].
///
/// **One `read` is not one line**, and nor is one line one `read`: a TCP segment can carry
/// the greeting and the login reply together, or half of either. `mw_station::Lines` solves
/// the same problem for a Campbell datalogger and is deliberately not reused — its bound is
/// `bufio.Scanner`'s 64 KiB, which is right for a JSON record from a datalogger and three
/// orders of magnitude too generous for a 60-byte server greeting, and its failures are
/// `SessionError`s about a station. Thirty lines with the right bound and the right error
/// type is the smaller thing to own.
struct LineReader<'a> {
    stream: &'a mut dyn ByteStream,
    buffer: Vec<u8>,
}

impl<'a> LineReader<'a> {
    /// How much to read at a time. One segment's worth, which is more than any line the
    /// server sends.
    const READ_BYTES: usize = 512;

    fn new(stream: &'a mut dyn ByteStream) -> Self {
        Self {
            stream,
            buffer: Vec::with_capacity(Self::READ_BYTES),
        }
    }

    /// The next line, without its terminator, trimmed.
    ///
    /// `expected` names what the caller was waiting for, so that "the server said nothing"
    /// and "the server said something that is not a login reply" are different messages.
    async fn next_line(
        &mut self,
        target: &Target,
        expected: &'static str,
    ) -> Result<String, UploadError> {
        loop {
            if let Some(at) = self.buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=at).collect();
                return Ok(String::from_utf8_lossy(&line).trim().to_string());
            }
            if self.buffer.len() >= MAX_LINE_BYTES {
                return Err(UploadError::Malformed {
                    target: target.clone(),
                    expected,
                    why: "the server sent more than a kilobyte without a newline",
                });
            }
            let mut chunk = [0u8; Self::READ_BYTES];
            let read =
                self.stream
                    .read(&mut chunk)
                    .await
                    .map_err(|source| UploadError::Connect {
                        what: "could not read from",
                        target: target.clone(),
                        source,
                    })?;
            if read == 0 {
                if self.buffer.is_empty() {
                    return Err(UploadError::Malformed {
                        target: target.clone(),
                        expected,
                        why: "the server closed the connection without sending one",
                    });
                }
                // A final line with no terminator. Taken rather than refused: the
                // connection ending is the delimiter.
                let line = std::mem::take(&mut self.buffer);
                return Ok(String::from_utf8_lossy(&line).trim().to_string());
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

/// The sink, behind an `Arc` so that cloning it per attempt is a refcount bump rather than
/// a copy of the digipeater path.
///
/// `tower::retry` clones the request and the service clones the sink; a `FrameSink` holds a
/// `Vec<Callsign>` and a `PathBuf`, so the indirection is worth the one line.
#[derive(Debug, Clone)]
pub struct SharedSink(Arc<FrameSink>);

impl SharedSink {
    /// Wrap a sink.
    pub fn new(sink: FrameSink) -> Self {
        Self(Arc::new(sink))
    }

    /// The sink.
    pub fn get(&self) -> &FrameSink {
        &self.0
    }
}

impl Transmit for SharedSink {
    type Payload = InformationField;

    fn transmit(
        self,
        info: InformationField,
    ) -> mw_component::BoxFuture<'static, Result<(), UploadError>> {
        Box::pin(send(self.0, info))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use mw_station::testing::Transcript;
    use mw_types::{Class, Classify};

    fn call(text: &str) -> Callsign {
        Callsign::parse(text).expect("parses")
    }

    fn info(text: &str) -> InformationField {
        InformationField::new(text).expect("a valid field")
    }

    fn target() -> Target {
        Target::peer("noam.aprs2.net", 14580)
    }

    /// **`pkg/kiss`'s golden vector**, carried over byte for byte: the one test that tree
    /// had for its AX.25 encoder.
    #[test]
    fn the_ax25_frame_matches_the_go_implementations_golden_vector() {
        let frame = ax25_ui(
            &call("W1AW-9"),
            &call("APRS"),
            &[call("WIDE1-1")],
            &info("!hello"),
        );
        let want: Vec<u8> = vec![
            // dest "APRS" (SSID 0, command C-bit set, not last)
            0x82, 0xA0, 0xA4, 0xA6, 0x40, 0x40, 0xE0,
            // source "W1AW-9" (SSID 9, not last)
            0xAE, 0x62, 0x82, 0xAE, 0x40, 0x40, 0x72, // digi "WIDE1-1" (SSID 1, last)
            0xAE, 0x92, 0x88, 0x8A, 0x62, 0x40, 0x63, // control, PID
            0x03, 0xF0, // info
            b'!', b'h', b'e', b'l', b'l', b'o',
        ];
        assert_eq!(frame, want, "\n got {frame:02X?}\nwant {want:02X?}");
    }

    /// With no digipeater path the **source** is the last address, so its extension bit
    /// moves — which is the one thing a path-length change alters in the encoding.
    #[test]
    fn the_extension_bit_follows_the_last_address() {
        let frame = ax25_ui(&call("W1AW"), &call("APRS"), &[], &info("x"));
        assert_eq!(frame[13] & ax25::SSID_LAST, ax25::SSID_LAST);
        assert_eq!(frame[6] & ax25::SSID_LAST, 0, "the dest is not last");
        // And the command bit is on the destination only.
        assert_eq!(frame[6] & ax25::SSID_COMMAND, ax25::SSID_COMMAND);
        assert_eq!(frame[13] & ax25::SSID_COMMAND, 0);

        // Two digipeaters: the second is last.
        let two = ax25_ui(
            &call("W1AW"),
            &call("APRS"),
            &[call("WIDE1-1"), call("WIDE2-1")],
            &info("x"),
        );
        // Seven bytes per address: dest 0-6, source 7-13, digi1 14-20, digi2 21-27, and
        // the SSID octet is the seventh of each.
        assert_eq!(two[13] & ax25::SSID_LAST, 0, "the source is not last");
        assert_eq!(two[20] & ax25::SSID_LAST, 0, "the first digi is not last");
        assert_eq!(two[27] & ax25::SSID_LAST, ax25::SSID_LAST);
    }

    /// KISS escaping, over a payload that contains both bytes that need it.
    #[test]
    fn kiss_escapes_both_reserved_bytes() {
        let framed = kiss_frame(&[0x01, kiss::FEND, 0x02, kiss::FESC, 0x03]);
        assert_eq!(
            framed,
            vec![
                kiss::FEND,
                kiss::CMD_DATA,
                0x01,
                kiss::FESC,
                kiss::TFEND,
                0x02,
                kiss::FESC,
                kiss::TFESC,
                0x03,
                kiss::FEND,
            ]
        );
        // An information field is printable ASCII (`mw_aprs::InformationField`), so neither
        // reserved byte can occur in a real frame -- but the AX.25 addresses above it are
        // shifted bytes and `0xC0` is reachable there.
        let real = kiss_frame(&ax25_ui(
            &call("W1AW"),
            &call("APRS"),
            &[],
            &info("!4506.00N/12203.00W_.../...g...t041"),
        ));
        assert_eq!(real.first(), Some(&kiss::FEND));
        assert_eq!(real.last(), Some(&kiss::FEND));
    }

    /// The TNC2 header, which is the other framing over the same field.
    #[test]
    fn the_tnc2_header_is_the_aprs_is_pseudo_path() {
        let line = tnc2(
            &call("DW1234"),
            FrameSink::DEFAULT_DEST,
            APRS_IS_PATH,
            &info("!4506.00N/12203.00W_181/005g014t072"),
        );
        assert_eq!(
            line,
            "DW1234>APRS,TCPIP*:!4506.00N/12203.00W_181/005g014t072"
        );
        // The SSID is rendered when there is one, because the beaconing callsign is what a
        // receiver sees.
        let with_ssid = tnc2(&call("W1AW-13"), "APRS", APRS_IS_PATH, &info("!x"));
        assert!(with_ssid.starts_with("W1AW-13>APRS,TCPIP*:"), "{with_ssid}");
        // An empty path produces no stray comma.
        assert_eq!(tnc2(&call("W1AW"), "APRS", "", &info("!x")), "W1AW>APRS:!x");
    }

    /// The login conversation, played against a transcript rather than a socket.
    #[tokio::test]
    async fn a_successful_login_is_the_greeting_the_line_and_a_verified_reply() {
        let mut stream = Transcript::scripted(
            [
                b"# aprsc 2.1.19 noam.aprs2.net\r\n".to_vec(),
                b"# logresp N0CALL verified, server SECOND\r\n".to_vec(),
            ],
            Vec::<Vec<u8>>::new(),
        );
        let source = call("N0CALL");
        let passcode = Passcode::for_callsign(&source);
        login(
            &mut stream,
            &source,
            passcode,
            "modernweather-1.0",
            &target(),
        )
        .await
        .expect("logs in");
        let written = stream.written();
        assert_eq!(written.len(), 1);
        let line = String::from_utf8(written[0].clone()).expect("ASCII");
        assert!(line.starts_with("user N0CALL pass "), "{line}");
        assert!(line.ends_with(" vers modernweather-1.0\r\n"), "{line}");
        assert!(
            line.contains(&passcode.expose().to_string()),
            "the login line carries the passcode"
        );
    }

    /// `unverified` contains `verified`, which is why the match is over words.
    #[test]
    fn unverified_is_not_verified() {
        for (reply, ok) in [
            ("# logresp N0CALL verified, server X", true),
            ("# logresp N0CALL unverified, server X", false),
            ("# logresp N0CALL VERIFIED, server X", true),
            ("# logresp N0CALL unverified", false),
        ] {
            let verified = reply
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word.eq_ignore_ascii_case("verified"));
            assert_eq!(verified, ok, "{reply}");
        }
    }

    /// Every way the conversation can fail, and what each one classifies as. A refused
    /// login is `Fatal` because a callsign and a passcode that do not match each other do
    /// not start matching; everything else is retryable.
    #[tokio::test]
    async fn every_login_failure_is_named_and_classified() {
        let source = call("N0CALL");
        let passcode = Passcode::for_callsign(&source);

        // A refused login.
        let mut refused = Transcript::scripted(
            [
                b"# aprsc 2.1.19\r\n".to_vec(),
                b"# logresp N0CALL unverified, server X\r\n".to_vec(),
            ],
            Vec::<Vec<u8>>::new(),
        );
        let e = login(&mut refused, &source, passcode, "mw", &target())
            .await
            .expect_err("refused");
        assert!(matches!(e, UploadError::LoginRefused { .. }), "{e}");
        assert_eq!(e.class(), Class::Fatal);
        assert!(e.to_string().contains("unverified"), "{e}");

        // Something that is not an APRS-IS server at all.
        let mut wrong =
            Transcript::scripted([b"HTTP/1.1 200 OK\r\n".to_vec()], Vec::<Vec<u8>>::new());
        let e = login(&mut wrong, &source, passcode, "mw", &target())
            .await
            .expect_err("not a greeting");
        assert!(matches!(e, UploadError::Malformed { .. }), "{e}");
        assert!(matches!(e.class(), Class::Retryable { .. }));

        // A server that accepts the connection and says nothing.
        let mut silent = Transcript::scripted(Vec::<Vec<u8>>::new(), Vec::<Vec<u8>>::new());
        let e = login(&mut silent, &source, passcode, "mw", &target())
            .await
            .expect_err("no greeting");
        assert!(
            matches!(
                e,
                UploadError::Malformed {
                    why: "the server closed the connection without sending one",
                    ..
                }
            ),
            "{e}"
        );

        // A reply that is not a `#` line.
        let mut noise = Transcript::scripted(
            [b"# aprsc\r\n".to_vec(), b"nonsense\r\n".to_vec()],
            Vec::<Vec<u8>>::new(),
        );
        let e = login(&mut noise, &source, passcode, "mw", &target())
            .await
            .expect_err("not a login reply");
        assert!(
            matches!(
                e,
                UploadError::Malformed {
                    expected: "an APRS-IS login reply",
                    ..
                }
            ),
            "{e}"
        );
    }

    /// A server that never sends a newline is a bounded read rather than an allocation
    /// loop.
    #[tokio::test]
    async fn a_server_that_never_ends_a_line_is_refused() {
        // Chunked, because that is how bytes arrive: the reader has to accumulate across
        // reads before it can decide the line is too long.
        let flood: Vec<Vec<u8>> = (0..4).map(|_| vec![b'#'; 500]).collect();
        let mut stream = Transcript::scripted(flood, Vec::<Vec<u8>>::new());
        let source = call("N0CALL");
        let e = login(
            &mut stream,
            &source,
            Passcode::for_callsign(&source),
            "mw",
            &target(),
        )
        .await
        .expect_err("refused");
        assert!(
            matches!(
                e,
                UploadError::Malformed {
                    why: "the server sent more than a kilobyte without a newline",
                    ..
                }
            ),
            "{e}"
        );
    }

    /// The three transports are three variants and the defaults are named, which is what
    /// makes an unknown transport a plan error rather than a silent APRS-IS injection.
    #[test]
    fn the_three_transports_are_distinguishable_and_only_two_are_radios() {
        let interval = RadioInterval::new(Duration::from_secs(900)).expect("above the floor");
        let aprs_is = FrameSink::AprsIs {
            host: FrameSink::DEFAULT_APRS_IS.0.to_string(),
            port: FrameSink::DEFAULT_APRS_IS.1,
            source: call("N0CALL"),
            passcode: Passcode::CWOP,
            software: "mw".to_string(),
        };
        let serial = FrameSink::KissSerial {
            dev: PathBuf::from("/dev/ttyUSB0"),
            baud: FrameSink::DEFAULT_BAUD,
            source: call("N0CALL"),
            dest: call(FrameSink::DEFAULT_DEST),
            path: vec![call("WIDE1-1")],
            interval,
        };
        let tcp = FrameSink::KissTcp {
            host: "tnc.lan".to_string(),
            port: 8001,
            source: call("N0CALL"),
            dest: call(FrameSink::DEFAULT_DEST),
            path: vec![],
            interval,
        };

        assert_eq!(aprs_is.kind(), "aprs-is");
        assert_eq!(serial.kind(), "kiss-serial");
        assert_eq!(tcp.kind(), "kiss-tcp");
        assert!(!aprs_is.is_radio(), "the internet is not a radio");
        assert!(serial.is_radio());
        assert!(tcp.is_radio());
        assert_eq!(aprs_is.target().to_string(), "noam.aprs2.net:14580");
        assert_eq!(serial.target().to_string(), "/dev/ttyUSB0");
        assert_eq!(tcp.target().to_string(), "tnc.lan:8001");

        // The transport each one opens, which is the only place the serial/TCP split is
        // decided.
        assert!(matches!(serial.transport(), Transport::Serial { .. }));
        assert!(matches!(
            tcp.transport(),
            Transport::TcpOut { tls: None, .. }
        ));
        assert!(matches!(
            aprs_is.transport(),
            Transport::TcpOut { tls: None, .. }
        ));

        // The shared wrapper is the same sink.
        let shared = SharedSink::new(tcp.clone());
        assert_eq!(shared.get().kind(), "kiss-tcp");
    }
}
