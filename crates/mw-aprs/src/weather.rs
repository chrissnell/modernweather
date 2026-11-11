//! [`WeatherReport`] and [`InformationField`] — the two remaining functions of W2a §4b.4,
//! and the only thing in this crate a packet actually carries.
//!
//! `CreateCompleteWeatherReport` and `buildWeatherReportInfo` in the Go controller are one
//! `bytes.Buffer` and eleven `buffer.Write*` calls -- six of them formatting a value -- over
//! a `database.FetchedBucketReading`, which is why the encoder cannot be tested without a
//! database. Here the input is a
//! struct of `Option`s in the units `metrics/registry.toml` declares, and the output is an
//! [`InformationField`] — the payload **both** framings share, which is where
//! `docs/component-model.md` §4b.4 puts the split.
//!
//! # Absent is dots, and never zero
//!
//! Every optional field is `Option`, and a `None` renders the protocol's dots —
//! `t...`, `g...`, `b.....`. The Go builder formats `int64(reading.OutTemp)` out of a
//! non-nullable struct, so a station that reports no temperature beacons `t000`, which is
//! −18 °C and indistinguishable from a real reading. `ci/layers.toml`'s zero-fill rule
//! records this exact hazard against this exact crate: *"that leaves `mw-output` and
//! `mw-aprs` free to zero-fill an `Option<f32>` on its way to CWOP, which is the same
//! confusion one layer sideways"*. It is not free to; this module is where that is decided,
//! and the decision is dots.
//!
//! The four wind and temperature fields are **mandatory** in a complete weather report, so
//! they are emitted with dots when absent. The rest are optional and are **omitted**
//! entirely, because a receiver that sees no `r` field knows the station does not report
//! rainfall, where `r...` claims it reports it and has no value.
//!
//! # The units
//!
//! The fields carry the units the metric registry declares — `degF`, `mph`, `inHg`, `in`,
//! `percent`, `W_per_m2` — and every protocol-unit conversion happens here, in one place.
//! The Go builder does the barometer conversion inline
//! (`reading.Barometer*33.8638866666667*10`) inside the `Sprintf` that writes it, which is
//! the only place in that tree the constant appears and therefore the only place it could
//! disagree with itself.
//!
//! # What this does *not* encode
//!
//! The compressed position report. [`crate::position`] carries the whole compression family
//! because W2a §4b.4 names those functions, and nothing in this program emits one: a
//! compressed report has no room for the weather fields, which is why the uncompressed form
//! is what a weather station beacons.

use std::fmt;

use crate::position::Position;

/// Inches of mercury to hectopascals. The one place this program holds the constant.
const HPA_PER_INHG: f64 = 33.863_886_666_666_7;

/// The longest information field this will build.
///
/// AX.25 permits 256 bytes of information and APRS-IS lines are conventionally shorter.
/// The cap exists because the comment is operator-supplied: a 400-character
/// comment in a column whose schema comment calls it a comment would otherwise produce a
/// frame a TNC silently truncates mid-field.
pub const MAX_INFORMATION_BYTES: usize = 256;

/// The APRS symbol a station beacons under.
///
/// Two bytes that are always written together and are meaningless apart: the table
/// selector and the code. `aprs_symbol_table` and `aprs_symbol_code` are `CHAR(1)` columns
/// whose Go-side type is `string`, and the Go builder reads `device.APRSSymbolTable[0]`
/// after checking the string is non-empty — so a two-character value silently uses its
/// first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbol {
    table: u8,
    code: u8,
}

impl Symbol {
    /// The primary table's weather-station symbol, `/_`, which is what a weather beacon
    /// uses unless an operator has a reason.
    pub const WEATHER: Symbol = Symbol {
        table: b'/',
        code: b'_',
    };

    /// The symbol code a complete weather report must carry.
    ///
    /// Not a stylistic default. A position report is read as a weather report **because**
    /// its symbol code is `_` (Ch. 12); under any other code the bytes that follow are
    /// parsed as the course/speed extension, so `.../004g...t...` becomes a course of 0 at
    /// 4 knots and the rest becomes comment text. The weather data is destroyed silently.
    pub const WEATHER_CODE: u8 = b'_';

    /// A symbol from a configured table selector and code.
    ///
    /// Both must be single printable ASCII bytes. A table selector is `/`, `\` or an
    /// overlay character; a code is any printable byte. Validated rather than indexed,
    /// because the alternative is the Go behaviour.
    ///
    /// **This does not require [`Symbol::WEATHER_CODE`]**, because the same type is what
    /// [`parse_info`] reports for a packet somebody else sent.
    /// [`WeatherReport::encode_info`] is where the requirement is enforced.
    pub fn new(table: char, code: char) -> Result<Self, SymbolError> {
        for (what, c) in [("table selector", table), ("code", code)] {
            if !c.is_ascii_graphic() {
                return Err(SymbolError { what, bad: c });
            }
        }
        Ok(Symbol {
            table: table as u8,
            code: code as u8,
        })
    }

    /// The table selector byte.
    pub fn table(self) -> u8 {
        self.table
    }

    /// The symbol code byte.
    pub fn code(self) -> u8 {
        self.code
    }
}

/// Why a configured symbol is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "the APRS symbol {what} `{bad}` is not a printable ASCII character. The weather-station \
     symbol is table `/` with code `_`; leave both columns NULL to use it."
)]
pub struct SymbolError {
    /// Which of the two bytes.
    what: &'static str,
    /// What was configured.
    bad: char,
}

/// One station's weather, in the units the metric registry declares.
///
/// Every measurement is an `Option` because every one of them is an `Option<f32>` in
/// storage, and the whole of **D-26** is that an absent reading stays absent. `Default` so
/// that a test or an encoder names only the fields it is about.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WeatherReport {
    /// Wind direction, degrees from true north. Mandatory field; dots when absent.
    pub wind_direction_deg: Option<f64>,
    /// Sustained wind speed, mph. Mandatory field; dots when absent.
    pub wind_speed_mph: Option<f64>,
    /// Peak gust, mph. Mandatory field; dots when absent.
    pub wind_gust_mph: Option<f64>,
    /// Temperature, °F. Mandatory field; dots when absent.
    pub temperature_f: Option<f64>,
    /// Rainfall in the last hour, inches. Optional; omitted when absent.
    pub rain_last_hour_in: Option<f64>,
    /// Rainfall in the last 24 hours, inches. Optional; omitted when absent.
    pub rain_last_24h_in: Option<f64>,
    /// Rainfall since the station's local midnight, inches. Optional; omitted when absent.
    pub rain_since_midnight_in: Option<f64>,
    /// Relative humidity, per cent. Optional; omitted when absent.
    pub humidity_percent: Option<f64>,
    /// Barometric pressure, inches of mercury. Optional; omitted when absent.
    pub barometer_in_hg: Option<f64>,
    /// Solar irradiance, W/m². Optional; omitted when absent.
    pub solar_watts_per_m2: Option<f64>,
}

impl WeatherReport {
    /// Encode a complete weather report's information field.
    ///
    /// `!` — a real-time position report without messaging — then the position, the symbol,
    /// the weather data, the software identifier and the operator's comment. That is the
    /// order the protocol fixes; the only freedom is which optional fields appear.
    ///
    /// `software` identifies the program in the protocol's software/unit-type position and
    /// is not an opinion this crate has: the caller passes what it wants to be seen as,
    /// which `mw-output` takes from the build's version.
    pub fn encode_info(
        &self,
        position: Position,
        symbol: Symbol,
        software: &str,
        comment: Option<&str>,
    ) -> Result<InformationField, EncodeError> {
        // The one precondition this encoder has, and it is about the *protocol* rather than
        // about the bytes: see `Symbol::WEATHER_CODE`. The table selector is free -- an
        // overlay is legal and some operators use one -- and the code is not.
        if symbol.code() != Symbol::WEATHER_CODE {
            return Err(EncodeError::NotAWeatherSymbol {
                code: char::from(symbol.code()),
            });
        }
        let mut out = String::with_capacity(MAX_INFORMATION_BYTES);
        out.push('!');
        out.push_str(&position.latitude_text());
        out.push(char::from(symbol.table()));
        out.push_str(&position.longitude_text());
        out.push(char::from(symbol.code()));

        // The four mandatory fields, dots when absent.
        out.push_str(&three(self.wind_direction_deg.map(wrap_degrees)));
        out.push('/');
        out.push_str(&three(self.wind_speed_mph));
        out.push('g');
        out.push_str(&three(self.wind_gust_mph));
        out.push('t');
        out.push_str(&temperature(self.temperature_f));

        // The optional fields, in the protocol's order, omitted when absent.
        if let Some(inches) = self.rain_last_hour_in {
            out.push('r');
            out.push_str(&three(Some(hundredths(inches))));
        }
        if let Some(inches) = self.rain_last_24h_in {
            out.push('p');
            out.push_str(&three(Some(hundredths(inches))));
        }
        if let Some(inches) = self.rain_since_midnight_in {
            out.push('P');
            out.push_str(&three(Some(hundredths(inches))));
        }
        if let Some(percent) = self.humidity_percent {
            out.push('h');
            // `00` is one hundred per cent, which is the protocol's own encoding and the
            // reason this is not `{:02}` of a clamp to 0.
            let whole = percent.round().clamp(1.0, 100.0) as u32;
            out.push_str(&format!("{:02}", whole % 100));
        }
        if let Some(in_hg) = self.barometer_in_hg {
            out.push('b');
            let tenths = (in_hg * HPA_PER_INHG * 10.0).round().clamp(0.0, 99_999.0) as u32;
            out.push_str(&format!("{tenths:05}"));
        }
        if let Some(watts) = self.solar_watts_per_m2 {
            // `L` below 1000 W/m², `l` above it carrying the value less 1000 — the
            // protocol's own two-letter trick for a field that is three digits wide and a
            // quantity that is not.
            let whole = watts.round().clamp(0.0, 1999.0) as u32;
            if whole < 1000 {
                out.push('L');
                out.push_str(&format!("{whole:03}"));
            } else {
                out.push('l');
                out.push_str(&format!("{:03}", whole - 1000));
            }
        }

        out.push_str(software);
        if let Some(comment) = comment.map(str::trim).filter(|c| !c.is_empty()) {
            out.push(' ');
            out.push_str(comment);
        }
        InformationField::new(out)
    }
}

/// Three digits, or three dots for an absent value.
///
/// Clamped to 0..=999 rather than refused: these are sensor readings, and a wind speed of
/// 1200 mph is a fault whose right answer is to beacon the position with a saturated gust
/// rather than to beacon nothing.
fn three(value: Option<f64>) -> String {
    match value {
        None => "...".to_string(),
        Some(v) if !v.is_finite() => "...".to_string(),
        Some(v) => format!("{:03}", v.round().clamp(0.0, 999.0) as u32),
    }
}

/// The temperature field: three characters, where below zero is `-NN`.
fn temperature(value: Option<f64>) -> String {
    match value {
        None => "...".to_string(),
        Some(v) if !v.is_finite() => "...".to_string(),
        Some(v) => {
            let whole = v.round().clamp(-99.0, 999.0) as i32;
            if whole < 0 {
                format!("-{:02}", -whole)
            } else {
                format!("{whole:03}")
            }
        }
    }
}

/// Inches to hundredths of an inch, which is the unit every APRS rain field uses.
fn hundredths(inches: f64) -> f64 {
    inches * 100.0
}

/// A direction folded into 0..360.
///
/// A vane with a configured correction can report 370 after the correction is applied, and
/// `wind_dir_correction_degrees` is applied at ingest by three drivers — so the fold is
/// here rather than relying on every producer.
///
/// `360` folds to `000`, which in a **weather** report is north: the "000 means unknown"
/// convention belongs to the course/speed extension of a non-weather position report, which
/// is [`crate::position::compress_course`]'s field and not this one. An unknown direction
/// here is `None`, which renders dots.
fn wrap_degrees(degrees: f64) -> f64 {
    if !degrees.is_finite() {
        return degrees;
    }
    degrees.rem_euclid(360.0)
}

/// An APRS information field — the payload after the TNC2 or AX.25 header.
///
/// A newtype rather than a `String` because it is the **shared** output of the encoder:
/// `docs/component-model.md` §4b.4 puts the split between the encoder and the three
/// transports exactly here, and two framings read the same bytes. A `String` would make
/// "the thing all three send" indistinguishable from any other string in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InformationField(String);

impl InformationField {
    /// Wrap an information field, checking what the framings require of it.
    ///
    /// Printable ASCII and within [`MAX_INFORMATION_BYTES`]. A control byte is the one that
    /// matters: a `\r` or `\n` inside the field terminates an APRS-IS line early, so an
    /// operator comment containing a newline would inject a second packet — which is why
    /// this is a check and not documentation.
    pub fn new(text: impl Into<String>) -> Result<Self, EncodeError> {
        let text = text.into();
        if text.len() > MAX_INFORMATION_BYTES {
            return Err(EncodeError::TooLong {
                bytes: text.len(),
                limit: MAX_INFORMATION_BYTES,
            });
        }
        if let Some(bad) = text.chars().find(|c| !c.is_ascii_graphic() && *c != ' ') {
            return Err(EncodeError::NotPrintable { bad });
        }
        Ok(Self(text))
    }

    /// The field as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The field as bytes, which is what a framing writes.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// How many bytes it is.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it is empty, which [`InformationField::new`] permits and no encoder
    /// produces.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for InformationField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why an information field could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// Longer than a frame holds.
    #[error(
        "the APRS information field is {bytes} bytes and the limit is {limit}. The \
         configured `aprs_comment` is the only part of it an operator controls; shorten it."
    )]
    TooLong {
        /// How long it came out.
        bytes: usize,
        /// The limit.
        limit: usize,
    },

    /// A symbol code under which weather data is not weather data.
    #[error(
        "the APRS symbol code `{code}` is not `_`, and a position report is read as a \
         weather report only under `_` (protocol reference Ch. 12). Under any other code a \
         receiver parses the wind field as a course and speed and the rest as a comment, so \
         the weather data would be silently destroyed. Set `uploader.aprs_symbol_code` to \
         `_` or leave it NULL; the symbol TABLE may be overlaid freely."
    )]
    NotAWeatherSymbol {
        /// What was configured.
        code: char,
    },

    /// A byte a framing cannot carry.
    #[error(
        "the APRS information field contains `{bad:?}`, which is not printable ASCII. A \
         carriage return or newline in `aprs_comment` would end the APRS-IS line early and \
         the rest of the field would be read as a second packet."
    )]
    NotPrintable {
        /// The first offending character.
        bad: char,
    },
}

/// Parse a complete weather report's information field.
///
/// The decode half of this crate's responsibility, and the reason the golden test is a
/// **round trip**: a captured packet parses to values, those values re-encode, and the
/// bytes are compared. An encoder tested against bytes a human typed tests the human.
///
/// It is deliberately narrow. The data type indicators it accepts are `!` and `=` — a
/// real-time position report with and without messaging — because those are the two a
/// weather station beacons. A timestamped report (`/` or `@`) carries a six-byte timestamp
/// this program has no use for: the observation instant is the pipeline's, not the packet's.
pub fn parse_info(text: &str) -> Result<Parsed, ParseError> {
    let bytes = text.as_bytes();
    // `!` lat(8) table(1) lon(9) code(1) = 20 bytes, which is all this checks: the
    // mandatory wind and temperature block after it is a further 15 (`ddd/sssgNNNtNNN`)
    // and is checked field by field below, because a packet that stops inside it is a
    // different complaint from one that is not a position report at all.
    const HEADER: usize = 20;
    match bytes.first() {
        Some(b'!' | b'=') => {}
        Some(other) => {
            return Err(ParseError::DataType {
                indicator: char::from(*other),
            })
        }
        None => return Err(ParseError::Short { bytes: 0 }),
    }
    // `!` lat(8) table(1) lon(9) code(1): the offsets are the protocol's, written out
    // rather than computed so that an off-by-one is visible beside the layout above.
    //
    // **Taken as bytes and checked for ASCII first.** An information field is printable
    // ASCII by construction ([`InformationField::new`]), but this is a `pub fn` over an
    // arbitrary `&str` and the next caller of it is a receive path (W2a §4a.7's APRS-IS
    // input). Slicing `&text[1..9]` on a multi-byte character panics — `"!€€€€€€€"` is 22
    // bytes and byte 9 falls inside a `€` — so the length check is not sufficient and a
    // `debug_assert` would only move the panic to release.
    let header = bytes
        .get(..HEADER)
        .filter(|h| h.is_ascii())
        .ok_or(ParseError::Short { bytes: bytes.len() })?;
    let position = parse_position(&header[1..9], &header[10..19])?;
    let symbol = Symbol::new(char::from(header[9]), char::from(header[19]))
        .map_err(|_| ParseError::Symbol)?;

    let rest = &text[HEADER..];
    let mut report = WeatherReport::default();
    // The mandatory block, `ddd/sssgNNN` with dots permitted in every digit position.
    let (dir, rest) = take(rest, 3).ok_or(ParseError::Mandatory { field: "direction" })?;
    report.wind_direction_deg = digits(dir);
    let (slash, rest) = take(rest, 1).ok_or(ParseError::Mandatory { field: "direction" })?;
    if slash != "/" {
        return Err(ParseError::Mandatory { field: "direction" });
    }
    let (speed, rest) = take(rest, 3).ok_or(ParseError::Mandatory { field: "speed" })?;
    report.wind_speed_mph = digits(speed);

    let mut rest = rest;
    while let Some(indicator) = rest.as_bytes().first().copied() {
        let width = match indicator {
            b'g' | b't' | b'r' | b'p' | b'P' | b'L' | b'l' => 3,
            b'h' => 2,
            b'b' => 5,
            // Anything else is the software identifier and the comment, which is where the
            // weather data ends.
            //
            // **`s` (snowfall) and `#` (the raw rain counter) are deliberately NOT
            // consumed**, although they are three-digit weather fields and common in live
            // CWOP traffic. This program holds neither metric, so parsing them would read
            // a value it then discards -- and a discarded field cannot be re-emitted, which
            // would make any capture containing one unable to round trip. Breaking here
            // instead puts them in the trailer, so they survive an encode byte for byte.
            // The cost is that a `b` or `L` field *after* an `s` is not parsed; the
            // protocol's field order puts both before it.
            _ => break,
        };
        let Some((value, tail)) = take(&rest[1..], width) else {
            // A truncated trailing field is a comment that happens to start with a field
            // letter, which is legal and common (`g` is also the first letter of a word).
            break;
        };
        // A field letter followed by something that is neither digits, dots, spaces nor a
        // sign is also a comment rather than a field.
        //
        // **Spaces are accepted because the protocol permits them for an unavailable
        // value**, alongside dots (Ch. 12: "unknown or invalid data may be represented by
        // spaces, dots or a software-specific value"). Rejecting them made the scan stop at
        // the first space-padded field and silently drop every field after it -- so
        // `t   r000p000P000h50b09900` lost the rain, the humidity and the barometer with no
        // error. This program always *emits* dots, so such a packet parses and re-encodes
        // to the dotted form rather than byte-identically.
        if !value
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-' || b == b' ')
        {
            break;
        }
        match indicator {
            b'g' => report.wind_gust_mph = digits(value),
            b't' => report.temperature_f = digits(value),
            b'r' => report.rain_last_hour_in = digits(value).map(|v| v / 100.0),
            b'p' => report.rain_last_24h_in = digits(value).map(|v| v / 100.0),
            b'P' => report.rain_since_midnight_in = digits(value).map(|v| v / 100.0),
            // `h00` is one hundred per cent.
            b'h' => {
                report.humidity_percent = digits(value).map(|v| if v == 0.0 { 100.0 } else { v })
            }
            b'b' => report.barometer_in_hg = digits(value).map(|v| v / 10.0 / HPA_PER_INHG),
            b'L' => report.solar_watts_per_m2 = digits(value),
            b'l' => report.solar_watts_per_m2 = digits(value).map(|v| v + 1000.0),
            // Snowfall and the raw rain counter are fields this program does not hold, and
            // skipping them is what keeps the scan positional.
            _ => {}
        }
        rest = tail;
    }
    Ok(Parsed {
        position,
        symbol,
        weather: report,
        trailer: rest.to_string(),
    })
}

/// What [`parse_info`] read out of one information field.
///
/// The trailer is carried because the golden test is a **round trip**: the software
/// identifier and the operator comment are the part of a captured packet this program did
/// not produce, so reproducing the bytes means putting them back. Without it the test
/// would have to assert over a truncated prefix, which is the assertion that stops being
/// about the encoding.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    /// Where the station is.
    pub position: Position,
    /// What it beacons as.
    pub symbol: Symbol,
    /// What it reported.
    pub weather: WeatherReport,
    /// Everything after the last weather field: the software identifier and the comment.
    pub trailer: String,
}

/// `DDMM.hhN` and `DDDMM.hhW` back to decimal degrees.
///
/// Over **bytes**, which the caller has already established are ASCII: a `&str` slice at a
/// fixed offset is a panic on a multi-byte character and these offsets are fixed by the
/// protocol rather than by the input.
fn parse_position(lat: &[u8], lon: &[u8]) -> Result<Position, ParseError> {
    let latitude = parse_degrees(lat, 2, b'N', b'S')?;
    let longitude = parse_degrees(lon, 3, b'E', b'W')?;
    Position::new(latitude, longitude).map_err(|_| ParseError::Position)
}

/// One `DD[D]MM.hhH` field.
fn parse_degrees(
    bytes: &[u8],
    degree_digits: usize,
    plus: u8,
    minus: u8,
) -> Result<f64, ParseError> {
    // degrees + `MM.hh` + hemisphere.
    if bytes.len() != degree_digits + 6 || !bytes.is_ascii() {
        return Err(ParseError::Position);
    }
    let hemisphere = *bytes.last().ok_or(ParseError::Position)?;
    let sign = if hemisphere == plus {
        1.0
    } else if hemisphere == minus {
        -1.0
    } else {
        return Err(ParseError::Position);
    };
    let text = |slice: &[u8]| -> Result<f64, ParseError> {
        std::str::from_utf8(slice)
            .map_err(|_| ParseError::Position)?
            .parse()
            .map_err(|_| ParseError::Position)
    };
    let degrees = text(&bytes[..degree_digits])?;
    let minutes = text(&bytes[degree_digits..bytes.len() - 1])?;
    // `-0.0` for a southern or western field whose value is exactly zero, which is what
    // makes the hemisphere survive a round trip -- see `Position::latitude_text`.
    Ok(sign * (degrees + minutes / 60.0))
}

/// The first `n` bytes and the rest, or `None` if there are not `n`.
fn take(text: &str, n: usize) -> Option<(&str, &str)> {
    text.is_char_boundary(n).then(|| text.split_at(n))
}

/// Digits, or `None` for a dotted or space-padded field.
///
/// Trimmed first, because the protocol's "unavailable" is dots *or spaces* and a partially
/// padded value (`  5`) is a real reading in a fixed-width field.
fn digits(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    text.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Why an information field did not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Fewer bytes than a position report's header.
    #[error("{bytes} bytes is shorter than an APRS position report's 20-byte header")]
    Short {
        /// How many there were.
        bytes: usize,
    },

    /// A data type indicator this does not read.
    #[error(
        "`{indicator}` is not a data type indicator this parses. A weather station beacons \
         `!` or `=`; a timestamped report carries an instant this program takes from the \
         pipeline instead."
    )]
    DataType {
        /// The first byte.
        indicator: char,
    },

    /// The position fields are not positions.
    #[error("the latitude or longitude field is not `DDMM.hhN`/`DDDMM.hhW`")]
    Position,

    /// A symbol byte that is not printable.
    #[error("the symbol table selector or code is not a printable ASCII character")]
    Symbol,

    /// A missing mandatory wind field.
    #[error("the mandatory wind {field} field is absent; a complete weather report has `ddd/sss`")]
    Mandatory {
        /// Which one.
        field: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position() -> Position {
        Position::new(45.123_4, -122.456_7).expect("a position")
    }

    fn full() -> WeatherReport {
        WeatherReport {
            wind_direction_deg: Some(181.0),
            wind_speed_mph: Some(5.0),
            wind_gust_mph: Some(14.0),
            temperature_f: Some(72.0),
            rain_last_hour_in: Some(0.02),
            rain_last_24h_in: Some(0.31),
            rain_since_midnight_in: Some(0.25),
            humidity_percent: Some(55.0),
            barometer_in_hg: Some(29.92),
            solar_watts_per_m2: Some(640.0),
        }
    }

    #[test]
    fn a_complete_report_is_the_protocols_field_order() {
        let info = full()
            .encode_info(position(), Symbol::WEATHER, ".mw", None)
            .expect("encodes");
        // 29.92 inHg = 1013.20 hPa = 10132 tenths.
        assert_eq!(
            info.as_str(),
            "!4507.40N/12227.40W_181/005g014t072r002p031P025h55b10132L640.mw"
        );
    }

    /// **The dots**, and the defect they replace: a station reporting nothing beacons a
    /// field a receiver can tell is absent, not `t000`.
    #[test]
    fn an_absent_value_is_dots_or_nothing_and_never_zero() {
        let info = WeatherReport::default()
            .encode_info(position(), Symbol::WEATHER, ".mw", None)
            .expect("encodes");
        assert_eq!(info.as_str(), "!4507.40N/12227.40W_.../...g...t....mw");
        assert!(!info.as_str().contains("t000"));
        // The optional fields are absent entirely rather than dotted, so a receiver can
        // tell "reports no rainfall" from "reports rainfall and has no value".
        for letter in ['r', 'p', 'P', 'h', 'b', 'L'] {
            assert!(
                !info.as_str().contains(letter),
                "{letter} should be omitted: {info}"
            );
        }
        // A NaN that reached here from arithmetic over an empty window is an absence too,
        // not a `000`.
        let nan = WeatherReport {
            temperature_f: Some(f64::NAN),
            wind_speed_mph: Some(f64::INFINITY),
            ..WeatherReport::default()
        };
        let info = nan
            .encode_info(position(), Symbol::WEATHER, "", None)
            .expect("encodes");
        assert!(info.as_str().ends_with(".../...g...t..."), "{info}");
    }

    /// The field-by-field encodings the protocol fixes, each with the case that would be
    /// wrong under the obvious formatting.
    #[test]
    fn each_field_uses_the_protocols_own_encoding() {
        let encode = |r: WeatherReport| {
            r.encode_info(position(), Symbol::WEATHER, "", None)
                .expect("encodes")
                .as_str()
                .to_string()
        };

        // Humidity: 100% is `h00`, and 0% is not representable so it clamps to 1.
        assert!(encode(WeatherReport {
            humidity_percent: Some(100.0),
            ..Default::default()
        })
        .contains("h00"));
        assert!(encode(WeatherReport {
            humidity_percent: Some(0.0),
            ..Default::default()
        })
        .contains("h01"));
        assert!(encode(WeatherReport {
            humidity_percent: Some(5.4),
            ..Default::default()
        })
        .contains("h05"));

        // Temperature below zero is `-NN`, and three digits above 99.
        assert!(encode(WeatherReport {
            temperature_f: Some(-5.0),
            ..Default::default()
        })
        .contains("t-05"));
        assert!(encode(WeatherReport {
            temperature_f: Some(105.4),
            ..Default::default()
        })
        .contains("t105"));
        assert!(encode(WeatherReport {
            temperature_f: Some(-120.0),
            ..Default::default()
        })
        .contains("t-99"));

        // Rain is hundredths of an inch.
        assert!(encode(WeatherReport {
            rain_since_midnight_in: Some(1.5),
            ..Default::default()
        })
        .contains("P150"));

        // Solar above 1000 W/m² switches letter and carries the remainder.
        assert!(encode(WeatherReport {
            solar_watts_per_m2: Some(1234.0),
            ..Default::default()
        })
        .contains("l234"));
        assert!(encode(WeatherReport {
            solar_watts_per_m2: Some(999.0),
            ..Default::default()
        })
        .contains("L999"));

        // A direction past the circle is folded rather than truncated to `999`.
        assert!(encode(WeatherReport {
            wind_direction_deg: Some(370.0),
            ..Default::default()
        })
        .starts_with("!4507.40N/12227.40W_010/"));
    }

    /// The round trip the golden test is built on.
    #[test]
    fn a_report_survives_an_encode_and_a_parse() {
        let info = full()
            .encode_info(position(), Symbol::WEATHER, ".mw", None)
            .expect("encodes");
        let parsed = parse_info(info.as_str()).expect("parses");
        let (p, r) = (parsed.position, parsed.weather);
        assert_eq!(parsed.symbol, Symbol::WEATHER);
        assert_eq!(parsed.trailer, ".mw");
        // The position round trips to the precision the field holds, which is 0.01 minutes.
        assert!((p.latitude() - position().latitude()).abs() < 0.001, "{p}");
        assert!(
            (p.longitude() - position().longitude()).abs() < 0.001,
            "{p}"
        );
        assert_eq!(r.wind_direction_deg, Some(181.0));
        assert_eq!(r.wind_speed_mph, Some(5.0));
        assert_eq!(r.wind_gust_mph, Some(14.0));
        assert_eq!(r.temperature_f, Some(72.0));
        assert_eq!(r.rain_last_hour_in, Some(0.02));
        assert_eq!(r.rain_last_24h_in, Some(0.31));
        assert_eq!(r.rain_since_midnight_in, Some(0.25));
        assert_eq!(r.humidity_percent, Some(55.0));
        assert_eq!(r.solar_watts_per_m2, Some(640.0));
        let barometer = r.barometer_in_hg.expect("a barometer");
        assert!((barometer - 29.92).abs() < 0.005, "{barometer}");

        // Dots parse back to absences, which is the property that makes them worth
        // emitting.
        let empty = parse_info("!4507.40N/12227.40W_.../...g...t...").expect("parses");
        assert_eq!(empty.weather, WeatherReport::default());
        assert_eq!(empty.trailer, "");
    }

    /// The comment is where an operator can break a frame, so it is the one input that is
    /// checked rather than trusted.
    #[test]
    fn an_operator_comment_cannot_inject_a_second_packet() {
        let r = WeatherReport::default();
        let e = r
            .encode_info(
                position(),
                Symbol::WEATHER,
                ".mw",
                Some("hello\r\nN0CALL>APRS:!"),
            )
            .expect_err("refused");
        assert!(matches!(e, EncodeError::NotPrintable { .. }), "{e}");

        let long = "x".repeat(MAX_INFORMATION_BYTES);
        let e = r
            .encode_info(position(), Symbol::WEATHER, ".mw", Some(&long))
            .expect_err("refused");
        assert!(matches!(e, EncodeError::TooLong { .. }), "{e}");
        assert!(e.to_string().contains("aprs_comment"), "{e}");

        // A comment that is whitespace is not a comment, so no trailing space is emitted.
        let info = r
            .encode_info(position(), Symbol::WEATHER, ".mw", Some("   "))
            .expect("encodes");
        assert!(info.as_str().ends_with(".mw"), "{info}");
    }

    /// A configured symbol is validated rather than indexed, and the weather symbol is the
    /// default.
    #[test]
    fn a_symbol_is_two_validated_bytes() {
        assert_eq!(Symbol::WEATHER.table(), b'/');
        assert_eq!(Symbol::WEATHER.code(), b'_');
        let overlay = Symbol::new('\\', '_').expect("a valid symbol");
        let info = WeatherReport::default()
            .encode_info(position(), overlay, "", None)
            .expect("encodes");
        assert!(info.as_str().starts_with("!4507.40N\\12227.40W_"), "{info}");
        for (table, code) in [(' ', '_'), ('/', '\n'), ('/', '\u{e9}')] {
            assert!(Symbol::new(table, code).is_err(), "{table:?}{code:?}");
        }
    }

    /// **The parser does not panic on any input.** It is a `pub fn` over an arbitrary
    /// `&str`, and the offsets it slices at are the protocol's rather than the input's.
    ///
    /// Each of these panicked before the header was taken as ASCII bytes: the first two at
    /// the position slice (byte 9 and byte 10 fall inside a multi-byte character) and the
    /// third inside `parse_degrees`.
    #[test]
    fn a_multi_byte_character_in_the_header_is_a_refusal_and_not_a_panic() {
        for text in [
            "!\u{20ac}\u{20ac}\u{20ac}\u{20ac}\u{20ac}\u{20ac}\u{20ac}",
            "!\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}",
            "!\u{20ac}aaaaN/12203.00W_000/000",
            "\u{20ac}",
            "!4507.40N/12227.40W\u{20ac}181/005",
        ] {
            // `is_err` rather than a specific variant: what matters is that it returns.
            assert!(parse_info(text).is_err(), "{text:?} should be refused");
        }
        // And a field VALUE that is not ASCII stops the scan rather than slicing into a
        // character, because the trailer is taken from a byte offset too.
        let parsed =
            parse_info("!4507.40N/12227.40W_181/005t\u{20ac}0").expect("the header is ASCII");
        assert_eq!(parsed.weather.temperature_f, None);
    }

    /// **A space-padded absent field is the protocol's other "unavailable", and dropping it
    /// used to take every field after it.**
    #[test]
    fn a_space_padded_field_is_an_absence_and_does_not_swallow_the_rest() {
        let parsed = parse_info("!4903.50N/07201.75W_220/004g005t   r000p000P000h50b09900wRSW")
            .expect("parses");
        assert_eq!(parsed.weather.temperature_f, None);
        // The four fields that used to be lost with no error.
        assert_eq!(parsed.weather.rain_last_hour_in, Some(0.0));
        assert_eq!(parsed.weather.rain_last_24h_in, Some(0.0));
        assert_eq!(parsed.weather.rain_since_midnight_in, Some(0.0));
        assert_eq!(parsed.weather.humidity_percent, Some(50.0));
        assert!(parsed.weather.barometer_in_hg.is_some());
        assert_eq!(parsed.trailer, "wRSW");
        // A partially padded value is a reading, not an absence.
        let parsed = parse_info("!4903.50N/07201.75W_220/004g005t 72").expect("parses");
        assert_eq!(parsed.weather.temperature_f, Some(72.0));
    }

    /// **Snowfall and the raw rain counter stay in the trailer**, so a capture carrying one
    /// still re-encodes byte for byte — which it could not if they were parsed and then
    /// discarded.
    #[test]
    fn a_field_this_program_does_not_hold_survives_in_the_trailer() {
        let captured = "!4903.50N/07201.75W_220/004g005t077r000p000P000h50b09900s050#123wRSW";
        let parsed = parse_info(captured).expect("parses");
        assert_eq!(parsed.trailer, "s050#123wRSW");
        let encoded = parsed
            .weather
            .encode_info(parsed.position, parsed.symbol, &parsed.trailer, None)
            .expect("encodes");
        assert_eq!(encoded.as_str(), captured);
    }

    /// **The symbol code is not a style choice.** A position report is read as a weather
    /// report only under `_`; under anything else the wind field becomes a course and speed
    /// and the rest becomes comment text, so the weather data is destroyed silently.
    #[test]
    fn weather_data_cannot_be_encoded_under_a_non_weather_symbol() {
        let report = WeatherReport {
            wind_speed_mph: Some(4.0),
            temperature_f: Some(72.0),
            ..WeatherReport::default()
        };
        let car = Symbol::new('/', '>').expect("a valid symbol");
        let e = report
            .encode_info(position(), car, ".mw", None)
            .expect_err("refused");
        assert!(
            matches!(e, EncodeError::NotAWeatherSymbol { code: '>' }),
            "{e}"
        );
        assert!(e.to_string().contains("aprs_symbol_code"), "{e}");
        // An overlaid TABLE is legal and still encodes: only the code is fixed.
        assert_eq!(Symbol::WEATHER_CODE, b'_');
        let overlay = Symbol::new('\\', '_').expect("a valid symbol");
        assert!(report.encode_info(position(), overlay, ".mw", None).is_ok());
    }

    /// The hemisphere survives a value of exactly zero, which is the one boundary a
    /// `< 0.0` test gets wrong — `-0.0` is not less than zero.
    #[test]
    fn a_southern_or_western_zero_keeps_its_hemisphere() {
        let captured = "!0000.00S/00000.00W_000/000g000t000";
        let parsed = parse_info(captured).expect("parses");
        let encoded = parsed
            .weather
            .encode_info(parsed.position, parsed.symbol, "", None)
            .expect("encodes");
        assert_eq!(encoded.as_str(), captured);
        // And a configured `0.0` is northern and eastern, which is what an operator typing
        // zero means.
        let p = Position::new(0.0, 0.0).expect("a position");
        assert_eq!(p.latitude_text(), "0000.00N");
        assert_eq!(p.longitude_text(), "00000.00E");
    }

    /// The parser's refusals, each of which is a packet from somewhere else rather than a
    /// bug here.
    #[test]
    fn the_parser_refuses_what_it_does_not_read() {
        assert!(matches!(
            parse_info(""),
            Err(ParseError::Short { bytes: 0 })
        ));
        assert!(matches!(
            parse_info("@092345z4903.50N/07201.75W_"),
            Err(ParseError::DataType { indicator: '@' })
        ));
        assert!(matches!(
            parse_info("!4507.40N/122"),
            Err(ParseError::Short { .. })
        ));
        assert!(matches!(
            parse_info("!99XX.XXN/12227.40W_000/000"),
            Err(ParseError::Position)
        ));
        // A field letter followed by a word is a comment, not a truncated field.
        let parsed = parse_info("!4507.40N/12227.40W_181/005t072 gusty later").expect("parses");
        assert_eq!(parsed.weather.temperature_f, Some(72.0));
        assert_eq!(parsed.weather.wind_gust_mph, None);
        assert_eq!(parsed.trailer, " gusty later");
    }
}
