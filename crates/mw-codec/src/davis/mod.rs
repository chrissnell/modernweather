//! `DavisLoop` — the Davis Vantage Pro/Pro2 LOOP record.
//!
//! The decoder half of W2a §4a.5's Davis row. The conversation that produces the records —
//! the wake handshake and `LOOP n`, re-issued when a run ends — is a `Session` in
//! `mw-station`, which is the split this crate exists for: the framing and the re-issue are
//! where the Davis defect W3 **D-82** names actually lived, and they are impure. What is
//! here is one 99-byte record in and one reading out, which is a function over a byte string.
//!
//! # Validity is four tests, and the Go driver makes two of them
//!
//! | Test | Go | Here |
//! |---|---|---|
//! | 99 bytes | `station.go:476` | [`wire::Loop::parse`] |
//! | CRC-16 over all 99 is zero | `station.go:483` | [`crate::crc16::check`] |
//! | the header is `LOO` | the scanner, `station.go:519-523` | here |
//! | `packet_type` is a LOOP and not a LOOP2 | **not made** | here |
//!
//! The fourth is the one worth the sentence. A LOOP2 record carries the same header and the
//! same length and a **different layout**; the Go driver's protection against decoding one
//! is that it never sends the command that asks for one, which is a property of a different
//! file. Refusing the record is a counted drop ([`CodecError`]); decoding it would put a
//! wind gust where the inside temperature goes and store it.
//!
//! # Where this disagrees with the Go driver, and why
//!
//! **1. A dashed field is absent, not zero.** Every one of Go's eleven `conv*` helpers
//! returns `0` for the sentinel the console sends when a sensor is not connected, so a
//! Vantage with no leaf/soil station stores a tidy row of zeros — and `min(soiltemp1)` over
//! a month is then 0 °F rather than NULL. **D-3** and **D-26** are explicit that "the sensor
//! did not report" and "the sensor reported zero" are different facts and that the only
//! place a zero is substituted is the frozen DTO's `zf`, where it is substituted *on the
//! wire* and not in the column. So a sentinel decodes to `None`. The frozen JSON is
//! unaffected, because `zf` fills it one layer later; the column changes meaning, in the
//! direction the design already decided. This is the Davis instance of **DEV-8**.
//!
//! **2. Both dash values for a 16-bit temperature, not one.** The specification's dashed
//! value for the inside and outside temperature is `32767`; `convBigVal10` tests `-32768`
//! (`station.go:727-732`). So a Vantage
//! with the outdoor transmitter out of range stores **3276.7 °F** under Go, which then
//! enters `max(outtemp)` and every aggregate above it. Both values are refused here: 32767
//! because it is the documented sentinel, and -32768 because it is what Go tested for and no
//! console reports -3276.8 °F.
//!
//! **3. `windchill` and `heatindex` are not computed here.** Go computes both in the driver
//! (`station.go:683`). **R1**/**DEV-9** moved every derived value to the enrich stage, which
//! `mw_pipeline::enrich` owns and which computes them for every station rather than for the
//! drivers that remembered to. A codec that computed them would be the second implementation.
//!
//! **4. Wind direction `0` is absent, not due north.** The serial protocol gives the field
//! the range 1 to 360 and reserves `0` for "no wind direction data"; `convWindDir` stores it
//! as a direction anyway, so a vane that is not reporting reads as due north and enters
//! `min(winddir)` and every wind rose. Refused here. The cost is stated because it is
//! measurable: the vendored emulator draws `uint16(rand.Float64() * 360)`, so about one
//! packet in three hundred and sixty now has an absent direction where Go stored `0`.
//!
//! **5. The console battery lands in `stationbatteryvoltage`, which is Go's column and not
//! the obvious one.** `station.go:679` assigns `convConsBatteryVoltage` to
//! `StationBatteryVoltage`, and no Go driver writes `consbatteryvoltage` at all. Faithful on
//! purpose: M5-2 diffs this driver against that one by value, and a column moved here would
//! be reported as a migration defect.
//!
//! # Two fields the record carries and this does not store
//!
//! `storm_start` is a packed **local** date — the console has a time zone and the packet does
//! not carry it — and `metrics/registry.toml` declares the column `timestamptz`. A codec
//! cannot resolve a local date without the station's zone, and it may not read a clock to
//! guess the year's century either. Go does not store it; neither does this, and the
//! difference between "not implemented" and "not expressible here" is the reason this
//! paragraph exists.
//!
//! `sunrise`/`sunset` are BCD `HHMM` in the same zone, and **W2a Decision 36** settles them
//! independently: they are functions of `(date, latitude, longitude)`, are computed where
//! they are served, and M5-4 deletes the stored table. Storing the console's opinion of them
//! would be a third source for a value that already has two.

mod wire;

use mw_types::{PartialReading, Reading};

use crate::crc16;
use crate::frame::{Codec, CodecError, Frame};
use crate::winddir::WindDirCorrection;
use wire::Loop;

/// How many bytes one LOOP record is.
pub const LOOP_BYTES: usize = Loop::BYTES;

/// The three ASCII bytes a LOOP record starts with.
pub const LOOP_HEADER: [u8; 3] = Loop::HEADER;

/// Rain-collector clicks per inch, as this decoder assumes it.
///
/// **The packet does not carry it.** A Vantage's collector is 0.01 in, 0.2 mm or 0.1 mm and
/// which one is in the console's EEPROM, reachable only by a separate `EEBRD` command that
/// neither implementation issues. So `rainrate`, `stormrain` and the three rain totals are
/// clicks scaled by this constant, which is what `convVal100` and `convBigVal100` do.
///
/// **A divisor, not a multiplier, and that is the whole point.** `convVal100` is
/// `float32(v) / 100.0`; multiplying by the nearest `f32` to 0.01 is a different function,
/// because 0.01 is not representable. The two disagree in the last bit for 17 484 of the
/// 65 536 possible raw values -- raw 33, the `monthrain` byte of `davis-loop-1.bin`, gives
/// 0.32999998 one way and 0.33000001 the other -- and both are stored in a `float4`
/// column. M5-2 diffs this driver against that one by value, so the last bit is the unit
/// of correctness here.
///
/// Compare the WeatherLink Live codec, which leaves rainfall **absent** when `rain_size` is
/// missing (`crate::wll::map`): there the field exists in the payload and was being ignored,
/// so the choice was between a right answer and a wrong one. Here there is no field at all,
/// and the alternative to assuming the commonest collector is a Davis station that never
/// reports rain. Recorded rather than silent, because a 0.2 mm collector reads 1.27x high.
pub const CLICKS_PER_INCH: f32 = 100.0;

/// Wind direction's own "no data" value, which is not a dash.
///
/// The Vantage serial protocol gives this field the range **1 to 360**, with 360 for north
/// and `0` reserved for "no wind direction data" -- so a vane that is not reporting sends a
/// value the field's own documented range excludes. `convWindDir` stores it as a direction,
/// which is 0 degrees, which is due north: a failed vane becomes a real reading that feeds
/// `min(winddir)` and every wind rose. Refused here instead, which is **DEV-16(g)** and the
/// same choice D-3/D-26 made for every other absent value. Note that it is not the
/// 0x7FFF dash: the protocol gives this one field two distinct ways to say nothing.
const NO_DIRECTION: u16 = 0;

/// The dashed value of a 16-bit signed field, per the Davis specification.
const DASH_I16: i16 = 32_767;

/// What `davis/station.go` tests for instead. Refused as well; see this module's second
/// disagreement.
const GO_DASH_I16: i16 = i16::MIN;

/// The dashed value of the fields whose sentinel is `0x7FFF` read unsigned — the barometer
/// and the solar radiation (`convVal1000Zero`, `convBigVal`).
const DASH_U16_HIGH: u16 = 0x7FFF;

/// The dashed value of a 16-bit field that uses all of its range — the rain rate
/// (`convBigVal100`).
const DASH_U16_FULL: u16 = 0xFFFF;

/// The dashed value of every single-byte field.
const DASH_U8: u8 = 0xFF;

/// The offset a Davis extra/soil/leaf temperature byte carries.
const TEMP_OFFSET: f32 = 90.0;

/// The Davis LOOP codec for one station's vane calibration.
#[derive(Debug, Clone, Default)]
pub struct DavisLoop {
    correction: WindDirCorrection,
}

impl DavisLoop {
    /// The codec for a station whose vane is offset by `correction`.
    #[must_use]
    pub const fn new(correction: WindDirCorrection) -> Self {
        Self { correction }
    }

    /// The vane calibration this codec applies.
    #[must_use]
    pub const fn correction(&self) -> WindDirCorrection {
        self.correction
    }

    /// Validate and parse one record.
    fn packet(frame: &Frame) -> Result<Loop, CodecError> {
        let bytes = frame.bytes();
        let Some(packet) = Loop::parse(bytes) else {
            return Err(CodecError::Framing {
                what: "a Davis LOOP record",
                expected: LOOP_BYTES,
                found: bytes.len(),
            });
        };
        if packet.header != Loop::HEADER {
            return Err(CodecError::Incomplete {
                what: "`LOO` header; this is ninety-nine bytes of something else",
            });
        }
        // Before the CRC, because "this is a LOOP2 record" is a better sentence than "the
        // check bytes do not match" for a frame whose check bytes do match.
        if packet.packet_type != Loop::LOOP1 {
            return Err(CodecError::Unsupported {
                what: "LOOP record type",
                found: i64::from(packet.packet_type),
            });
        }
        if !crc16::check(bytes) {
            return Err(CodecError::Check {
                what: "a Davis LOOP record",
            });
        }
        Ok(packet)
    }
}

impl Codec for DavisLoop {
    fn name(&self) -> &'static str {
        "davis-loop"
    }

    fn decode(&self, frame: &Frame) -> Result<Vec<PartialReading>, CodecError> {
        let p = Self::packet(frame)?;
        let mut r = Reading::default();
        fill(&mut r, &p, self.correction);
        // No `observed_at`: a LOOP record is the console's *current* state and carries no
        // timestamp at all, so admission substitutes the frame's arrival time -- which is
        // the honest answer and is what `r.Timestamp = time.Now()` approximates at
        // `station.go:501`, one layer further from the wire.
        Ok(vec![PartialReading::new(r)])
    }
}

/// Every field of one record, onto one reading.
///
/// A function over `&mut Reading` rather than a struct literal, for `crate::wll::map`'s
/// reason: the mapping is one line per metric and reads against the specification's own
/// table, and a 72-field initialiser reads against nothing.
fn fill(r: &mut Reading, p: &Loop, correction: WindDirCorrection) {
    r.barometer = thousandths(p.barometer.get());
    r.in_temp = tenths(p.in_temp.get());
    r.in_humidity = percent(p.in_humidity);
    r.out_temp = tenths(p.out_temp.get());
    r.out_humidity = percent(p.out_humidity);
    r.wind_speed = whole_byte(p.wind_speed);
    r.wind_speed10 = whole_byte(p.wind_speed10);
    r.wind_dir = correction.apply(direction(p.wind_dir.get()));

    let extra_temp = p.extra_temp.map(offset_temp);
    r.extra_temp1 = extra_temp[0];
    r.extra_temp2 = extra_temp[1];
    r.extra_temp3 = extra_temp[2];
    r.extra_temp4 = extra_temp[3];
    r.extra_temp5 = extra_temp[4];
    r.extra_temp6 = extra_temp[5];
    r.extra_temp7 = extra_temp[6];

    let soil_temp = p.soil_temp.map(offset_temp);
    r.soil_temp1 = soil_temp[0];
    r.soil_temp2 = soil_temp[1];
    r.soil_temp3 = soil_temp[2];
    r.soil_temp4 = soil_temp[3];

    let leaf_temp = p.leaf_temp.map(offset_temp);
    r.leaf_temp1 = leaf_temp[0];
    r.leaf_temp2 = leaf_temp[1];
    r.leaf_temp3 = leaf_temp[2];
    r.leaf_temp4 = leaf_temp[3];

    let extra_humidity = p.extra_humidity.map(percent);
    r.extra_humidity1 = extra_humidity[0];
    r.extra_humidity2 = extra_humidity[1];
    r.extra_humidity3 = extra_humidity[2];
    r.extra_humidity4 = extra_humidity[3];
    r.extra_humidity5 = extra_humidity[4];
    r.extra_humidity6 = extra_humidity[5];
    r.extra_humidity7 = extra_humidity[6];

    r.rain_rate = clicks(p.rain_rate.get(), DASH_U16_FULL);
    r.uv = byte_tenths(p.uv);
    // Go's column, not `radiation`: `station.go:646` assigns the LOOP packet's solar
    // radiation to `SolarWatts`, and the WeatherLink Live driver assigns its own to
    // `radiation` (`data_transformer.go:222`). Two drivers, two columns, both frozen
    // keys -- faithful per driver is the only way M5-2's value diff can be read.
    r.solar_watts = radiation(p.radiation.get());
    r.storm_rain = clicks(p.storm_rain.get(), None);
    r.day_rain = clicks(p.day_rain.get(), None);
    r.month_rain = clicks(p.month_rain.get(), None);
    r.year_rain = clicks(p.year_rain.get(), None);
    r.day_et = scaled(p.day_et.get(), 1000.0);
    r.month_et = scaled(p.month_et.get(), 100.0);
    r.year_et = scaled(p.year_et.get(), 100.0);

    let soil_moisture = p.soil_moisture.map(whole_byte);
    r.soil_moisture1 = soil_moisture[0];
    r.soil_moisture2 = soil_moisture[1];
    r.soil_moisture3 = soil_moisture[2];
    r.soil_moisture4 = soil_moisture[3];

    let leaf_wetness = p.leaf_wetness.map(whole_byte);
    r.leaf_wetness1 = leaf_wetness[0];
    r.leaf_wetness2 = leaf_wetness[1];
    r.leaf_wetness3 = leaf_wetness[2];
    r.leaf_wetness4 = leaf_wetness[3];

    // The alarm, battery and forecast bytes are **bitfields and codes**, so zero is a
    // value and there is no sentinel: "no alarms are set" is what a healthy console
    // reports, and treating it as absent would make the column NULL for every station
    // that is working. Carried verbatim, as Go carries them.
    r.inside_alarm = state(p.inside_alarm);
    r.rain_alarm = state(p.rain_alarm);
    r.outside_alarm1 = state(p.outside_alarm[0]);
    r.outside_alarm2 = state(p.outside_alarm[1]);
    r.extra_alarm1 = state(p.extra_alarm[0]);
    r.extra_alarm2 = state(p.extra_alarm[1]);
    r.extra_alarm3 = state(p.extra_alarm[2]);
    r.extra_alarm4 = state(p.extra_alarm[3]);
    r.extra_alarm5 = state(p.extra_alarm[4]);
    r.extra_alarm6 = state(p.extra_alarm[5]);
    r.extra_alarm7 = state(p.extra_alarm[6]);
    r.extra_alarm8 = state(p.extra_alarm[7]);
    r.soil_leaf_alarm1 = state(p.soil_leaf_alarm[0]);
    r.soil_leaf_alarm2 = state(p.soil_leaf_alarm[1]);
    r.soil_leaf_alarm3 = state(p.soil_leaf_alarm[2]);
    r.soil_leaf_alarm4 = state(p.soil_leaf_alarm[3]);
    r.tx_battery_status = state(p.tx_battery_status);
    r.station_battery_voltage = Some(console_volts(p.cons_battery_voltage.get()));
    r.forecast_icon = state(p.forecast_icon);
    r.forecast_rule = state(p.forecast_rule);
}

/// Thousandths of an inch of mercury.
fn thousandths(raw: u16) -> Option<f32> {
    (raw != DASH_U16_HIGH).then(|| f32::from(raw) / 1000.0)
}

/// Tenths of a degree Fahrenheit, with both dash values refused.
fn tenths(raw: i16) -> Option<f32> {
    (raw != DASH_I16 && raw != GO_DASH_I16).then(|| f32::from(raw) / 10.0)
}

/// A whole-number byte field: humidity, wind speed, soil moisture, leaf wetness.
fn whole_byte(raw: u8) -> Option<f32> {
    (raw != DASH_U8).then(|| f32::from(raw))
}

/// A per-cent byte field. The same arithmetic as [`whole_byte`] and a different name,
/// because a humidity and a leaf-wetness index are not the same quantity and a reader
/// should not have to check.
fn percent(raw: u8) -> Option<f32> {
    whole_byte(raw)
}

/// Tenths of a unit in one byte — the UV index.
fn byte_tenths(raw: u8) -> Option<f32> {
    (raw != DASH_U8).then(|| f32::from(raw) / 10.0)
}

/// Degrees Fahrenheit, from a byte offset by 90.
fn offset_temp(raw: u8) -> Option<f32> {
    (raw != DASH_U8).then(|| f32::from(raw) - TEMP_OFFSET)
}

/// Wind direction in degrees.
fn direction(raw: u16) -> Option<f32> {
    (raw != DASH_U16_HIGH && raw != NO_DIRECTION).then(|| f32::from(raw))
}

/// Watts per square metre.
fn radiation(raw: u16) -> Option<f32> {
    (raw != DASH_U16_HIGH).then(|| f32::from(raw))
}

/// Rain-collector clicks as inches, with an optional dash value.
fn clicks(raw: u16, dash: impl Into<Option<u16>>) -> Option<f32> {
    match dash.into() {
        Some(dash) if raw == dash => None,
        _ => Some(f32::from(raw) / CLICKS_PER_INCH),
    }
}

/// A 16-bit field divided by a constant, with no dash value.
fn scaled(raw: u16, by: f32) -> Option<f32> {
    Some(f32::from(raw) / by)
}

/// An alarm, battery or forecast byte, which has no sentinel.
fn state(raw: u8) -> Option<i32> {
    Some(i32::from(raw))
}

/// The console's supply voltage: `raw * 300 / 512 / 100`, which is `convConsBatteryVoltage`.
fn console_volts(raw: u16) -> f32 {
    (f32::from(raw) * 300.0) / 512.0 / 100.0
}

#[cfg(test)]
mod tests;
