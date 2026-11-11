//! `CampbellAscii` — one line of JSON from a Campbell Scientific datalogger.
//!
//! The datalogger is programmed in CRBasic by whoever installed it, and what it emits is
//! whatever that program writes: this one writes a JSON object per output interval,
//! newline-terminated, continuously, over serial or TCP. There is no command and no
//! response — W2a §4a.5's "command/response" row is the *capability* the transport has, and
//! this program does not use it — so the session above is a line framer and nothing more
//! (`mw_station::session::Lines`).
//!
//! # The ten fields, and the Go struct they come from
//!
//! | JSON key | Metric | Unit |
//! |---|---|---|
//! | `airtemp_f` | `outtemp` | °F |
//! | `rh` | `outhumidity` | % |
//! | `baro` | `barometer` | inHg |
//! | `baro_temp_f` | `extratemp1` | °F |
//! | `slr_w` | `solarwatts` | W/m² |
//! | `slr_mj` | `solarjoules` | MJ/m² — see below |
//! | `rain_in` | `rainincremental` | in |
//! | `wind_s` | `windspeed` | mph |
//! | `wind_d` | `winddir` | ° |
//! | `batt_volt` | `stationbatteryvoltage` | V |
//!
//! Every one is a straight copy: the datalogger is programmed in the registry's units,
//! which is the one driver where that is true by construction rather than by luck.
//!
//! **`slr_mj` is megajoules and `metrics/registry.toml` declares the column `J_per_m2`.**
//! That is **N-23**, recorded in the registry's own comment for `solar_joules` and
//! deliberately not corrected there, because **DEV-7** already asks the owner to
//! acknowledge one change to what that key means. So this codec copies the datalogger's
//! number unscaled, which is what the Go driver does — correcting it here would make the
//! column disagree with its own declared unit in the opposite direction and would be a
//! second unacknowledged change.
//!
//! # Where this disagrees with the Go driver
//!
//! **1. An absent key is absent.** `encoding/json` leaves a missing field at its zero
//! value, so a datalogger program that does not output `slr_mj` stores 0.0 MJ/m² every
//! interval under Go. **D-26**: a key the packet did not carry is `None` and reaches SQL as
//! NULL. Same correction as the Davis dash values, same reason.
//!
//! **2. `wind_d` is read as a number, not as a `uint16`.** The Go struct types it
//! `uint16`, so `"wind_d": 183.5` fails `json.Unmarshal`, which makes
//! `ParseCampbellScientificPackets` return an error, which closes the connection and
//! reconnects — one fractional degree takes the station down until the datalogger's next
//! whole-numbered output. Read as a number here, which is also what makes the vane
//! correction lossless (`crate::winddir`).
//!
//! **3. `potentialsolarwatts`, `windchill` and `heatindex` are not computed here.** All
//! three are in the Go Campbell driver and all three moved to the enrich stage under
//! **R1**/**DEV-9** — `potentialsolarwatts` is the one that only this driver ever computed,
//! which is why DEV-9 names it as the single key that changes on the wire.
//!
//! # An unrecognised key is dropped, and that is **OQ-W4-3**
//!
//! A CRBasic program can output anything, and the field below this comment is the exhaustive
//! list of what this build maps. An extra key — `soil_vwc`, a second wind sensor, a
//! thermocouple string — is **silently ignored**: `serde` does not reject unknown fields
//! here (rejecting them would make one added `Public` table column stop the whole station),
//! and there is nowhere to put it. W4's **OQ-W4-3** is exactly this question — whether the
//! registry gains a `jsonb` type so `observations.extra` can be a registry entry — and W4
//! flagged it as "worth deciding before W7 writes its first driver that receives an unknown
//! field". This is that driver.
//!
//! The question is **raised and not answered here**: `docs/reconciliation.md` **O-14** is
//! where it is put to the owner, with the two drivers that reach it, the three options and a
//! recommendation. Nothing in this file anticipates an answer, and
//! [`tests::an_unrecognised_key_is_dropped_and_does_not_fail_the_packet`] pins the current
//! behaviour so that whichever way it goes, the change is visible.

use mw_types::{PartialReading, Reading};
use serde::Deserialize;

use crate::frame::{Codec, CodecError, Frame};
use crate::winddir::WindDirCorrection;

/// The ten keys this build maps, as the datalogger writes them.
///
/// `Option` on every field, which is the whole of this module's first disagreement with the
/// Go struct: a key the program did not output is absent, not zero.
#[derive(Debug, Clone, Default, Deserialize)]
struct Packet {
    #[serde(default)]
    batt_volt: Option<f32>,
    #[serde(default)]
    airtemp_f: Option<f32>,
    #[serde(default)]
    rh: Option<f32>,
    #[serde(default)]
    baro: Option<f32>,
    #[serde(default)]
    baro_temp_f: Option<f32>,
    #[serde(default)]
    slr_w: Option<f32>,
    #[serde(default)]
    slr_mj: Option<f32>,
    #[serde(default)]
    rain_in: Option<f32>,
    #[serde(default)]
    wind_s: Option<f32>,
    #[serde(default)]
    wind_d: Option<f32>,
}

/// The Campbell Scientific codec for one station's vane calibration.
#[derive(Debug, Clone, Default)]
pub struct CampbellAscii {
    correction: WindDirCorrection,
}

impl CampbellAscii {
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
}

impl Codec for CampbellAscii {
    fn name(&self) -> &'static str {
        "campbell-ascii"
    }

    fn decode(&self, frame: &Frame) -> Result<Vec<PartialReading>, CodecError> {
        let text = frame.as_str()?.trim();
        // A datalogger that is powered but between outputs sends nothing; a line framer
        // that is handed `\r\n` produces an empty frame. `NoRecords` rather than a parse
        // failure, because "nothing arrived" and "this is not JSON" are different
        // sentences to an operator and only the second one means the program is wrong.
        if text.is_empty() {
            return Err(CodecError::NoRecords);
        }
        let packet: Packet =
            serde_json::from_str(text).map_err(|source| CodecError::Json { source })?;

        let r = Reading {
            station_battery_voltage: packet.batt_volt,
            out_temp: packet.airtemp_f,
            out_humidity: packet.rh,
            barometer: packet.baro,
            extra_temp1: packet.baro_temp_f,
            solar_watts: packet.slr_w,
            solar_joules: packet.slr_mj,
            rain_incremental: packet.rain_in,
            wind_speed: packet.wind_s,
            wind_dir: self.correction.apply(packet.wind_d),
            // Ten fields, and the rest of the registry absent. A struct literal rather than
            // field assignment because the list IS the mapping table in this module's
            // documentation, and the two have to be read against each other.
            ..Reading::default()
        };

        // No `observed_at`: the packet carries no time. The datalogger has a clock and its
        // program does not output it, which is a CRBasic change rather than a decoder one;
        // admission substitutes the arrival instant, which is what `time.Now()` at
        // `station.go:191` approximates.
        Ok(vec![PartialReading::new(r)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cmd/campbell-emulator`'s own packet shape, with every key present.
    const FULL: &str = r#"{"batt_volt":13.2,"airtemp_f":68.4,"rh":41.5,"baro":30.12,
                           "baro_temp_f":70.1,"slr_w":512.5,"slr_mj":1.84,"rain_in":0.02,
                           "wind_s":7.5,"wind_d":183}"#;

    fn decode(codec: &CampbellAscii, text: &str) -> Reading {
        let batch = codec
            .decode(&Frame::new(text.as_bytes().to_vec()))
            .expect("the packet decodes");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].observed_at, None);
        batch[0].values.clone()
    }

    #[test]
    fn every_key_the_datalogger_writes_lands_on_its_metric() {
        let r = decode(&CampbellAscii::default(), FULL);
        assert_eq!(r.station_battery_voltage, Some(13.2));
        assert_eq!(r.out_temp, Some(68.4));
        assert_eq!(r.out_humidity, Some(41.5));
        assert_eq!(r.barometer, Some(30.12));
        assert_eq!(r.extra_temp1, Some(70.1));
        assert_eq!(r.solar_watts, Some(512.5));
        assert_eq!(r.solar_joules, Some(1.84));
        assert_eq!(r.rain_incremental, Some(0.02));
        assert_eq!(r.wind_speed, Some(7.5));
        assert_eq!(r.wind_dir, Some(183.0));
        // The enrich stage's three (DEV-9), including the one only this driver computed.
        assert_eq!(r.wind_chill, None);
        assert_eq!(r.heat_index, None);
        assert_eq!(r.potential_solar_watts, None);
        // Not this driver's columns.
        assert_eq!(r.radiation, None);
        assert_eq!(r.in_temp, None);
    }

    /// The first disagreement: a CRBasic program that outputs six of the ten keys stores
    /// four NULLs rather than four zeros.
    #[test]
    fn a_key_the_program_does_not_output_is_absent_rather_than_zero() {
        let r = decode(
            &CampbellAscii::default(),
            r#"{"airtemp_f":68.4,"rh":41.5,"wind_s":0.0,"wind_d":0}"#,
        );
        assert_eq!(r.out_temp, Some(68.4));
        assert_eq!(r.out_humidity, Some(41.5));
        assert_eq!(r.solar_joules, None, "`slr_mj` was not output");
        assert_eq!(r.barometer, None);
        assert_eq!(r.rain_incremental, None);
        assert_eq!(r.station_battery_voltage, None);
        // A key that IS output and happens to be zero is a measurement, not an absence.
        assert_eq!(r.wind_speed, Some(0.0));
        assert_eq!(r.wind_dir, Some(0.0));
    }

    /// The second disagreement, as the value that takes the Go station offline.
    #[test]
    fn a_fractional_wind_direction_decodes_rather_than_closing_the_connection() {
        let r = decode(
            &CampbellAscii::default(),
            r#"{"wind_d":183.5,"wind_s":3.0}"#,
        );
        assert_eq!(r.wind_dir, Some(183.5));
        let corrected = decode(
            &CampbellAscii::new(WindDirCorrection::new(10)),
            r#"{"wind_d":183.5}"#,
        );
        assert_eq!(corrected.wind_dir, Some(193.5));
    }

    #[test]
    fn the_vane_calibration_wraps_and_touches_nothing_else() {
        let codec = CampbellAscii::new(WindDirCorrection::new(-30));
        assert_eq!(codec.correction().degrees(), -30);
        let r = decode(&codec, r#"{"wind_d":10,"wind_s":4.0}"#);
        assert_eq!(r.wind_dir, Some(340.0));
        assert_eq!(r.wind_speed, Some(4.0));
    }

    /// OQ-W4-3's case. Recorded as a test so that the behaviour is pinned while the
    /// question is open: an unknown key is dropped and the known ones still decode.
    #[test]
    fn an_unrecognised_key_is_dropped_and_does_not_fail_the_packet() {
        let r = decode(
            &CampbellAscii::default(),
            r#"{"airtemp_f":68.4,"soil_vwc_1":0.31,"tc_string":[1,2,3]}"#,
        );
        assert_eq!(r.out_temp, Some(68.4));
    }

    #[test]
    fn an_empty_line_is_no_records_and_a_bad_one_is_a_parse_failure() {
        let codec = CampbellAscii::default();
        for blank in ["", "\r\n", "   "] {
            let e = codec
                .decode(&Frame::new(blank.as_bytes().to_vec()))
                .expect_err("nothing to decode");
            assert!(matches!(e, CodecError::NoRecords), "`{blank:?}`: {e}");
        }
        let e = codec
            .decode(&Frame::new(b"airtemp_f=68.4".to_vec()))
            .expect_err("not JSON");
        assert!(matches!(e, CodecError::Json { .. }), "{e}");
        let e = codec
            .decode(&Frame::new(vec![0x7b, 0xff, 0x7d]))
            .expect_err("not text");
        assert!(matches!(e, CodecError::NotText), "{e}");
    }

    #[test]
    fn the_codec_names_itself_and_claims_no_station() {
        let codec = CampbellAscii::default();
        assert_eq!(codec.name(), "campbell-ascii");
        assert_eq!(
            codec.claimed_station(&Frame::new(FULL.as_bytes().to_vec())),
            None
        );
    }
}
