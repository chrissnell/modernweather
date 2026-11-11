//! [`WeatherReport`] and [`InformationField`] — the two remaining functions of W2a §4b.4,
//! and the only thing in this crate a packet actually carries.
//!
//! `CreateCompleteWeatherReport` and `buildWeatherReportInfo` in the Go controller are one
//! `bytes.Buffer` and eleven `fmt.Sprintf` calls over a `database.FetchedBucketReading`,
//! which is why the encoder cannot be tested without a database. Here the input is a
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

    /// A symbol from a configured table selector and code.
    ///
    /// Both must be single printable ASCII bytes. A table selector is `/`, `\` or an
    /// overlay character; a code is any printable byte. Validated rather than indexed,
    /// because the alternative is the Go behaviour.
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
    // `!` lat(8) table(1) lon(9) code(1) = 20 bytes, then `ddd/sssgggg` = at least 12.
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
    if bytes.len() < HEADER {
        return Err(ParseError::Short { bytes: bytes.len() });
    }
    // `!` lat(8) table(1) lon(9) code(1): the offsets are the protocol's, written out
    // rather than computed so that an off-by-one is visible beside the layout above.
    let position = parse_position(&text[1..9], &text[10..19])?;
    let symbol =
        Symbol::new(char::from(bytes[9]), char::from(bytes[19])).map_err(|_| ParseError::Symbol)?;

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
            b'g' | b't' | b'r' | b'p' | b'P' | b'L' | b'l' | b's' | b'#' => 3,
            b'h' => 2,
            b'b' => 5,
            // Anything else is the software identifier and the comment, which is where the
            // weather data ends.
            _ => break,
        };
        let Some((value, tail)) = take(&rest[1..], width) else {
            // A truncated trailing field is a comment that happens to start with a field
            // letter, which is legal and common (`g` is also the first letter of a word).
            break;
        };
        // A field letter followed by something that is neither digits nor dots is also a
        // comment rather than a field.
        if !value
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-')
        {
            break;
        }
        match indicator {
            b'g' => report.wind_gust_mph = digits(value),
            b't' => report.temperature_f = signed(value),
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
fn parse_position(lat: &str, lon: &str) -> Result<Position, ParseError> {
    let latitude = parse_degrees(lat, 2, b'N', b'S')?;
    let longitude = parse_degrees(lon, 3, b'E', b'W')?;
    Position::new(latitude, longitude).map_err(|_| ParseError::Position)
}

/// One `DD[D]MM.hhH` field.
fn parse_degrees(text: &str, degree_digits: usize, plus: u8, minus: u8) -> Result<f64, ParseError> {
    let bytes = text.as_bytes();
    // degrees + `MM.hh` + hemisphere.
    if bytes.len() != degree_digits + 6 {
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
    let degrees: f64 = text[..degree_digits]
        .parse()
        .map_err(|_| ParseError::Position)?;
    let minutes: f64 = text[degree_digits..text.len() - 1]
        .parse()
        .map_err(|_| ParseError::Position)?;
    Ok(sign * (degrees + minutes / 60.0))
}

/// The first `n` bytes and the rest, or `None` if there are not `n`.
fn take(text: &str, n: usize) -> Option<(&str, &str)> {
    text.is_char_boundary(n).then(|| text.split_at(n))
}

/// Digits, or `None` for a dotted field.
fn digits(text: &str) -> Option<f64> {
    text.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Digits with an optional leading `-`, for the temperature field.
fn signed(text: &str) -> Option<f64> {
    digits(text.trim_start_matches(' '))
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
