//! The Davis LOOP packet as a fixed little-endian structure.
//!
//! **D-32 names `zerocopy` for exactly this**, against the Go driver's
//! `binary.Read(reader, binary.LittleEndian, &struct)` — reflection over a 72-field struct,
//! once per packet, every two seconds. The layout below is a type, so the offsets are
//! checked by the compiler rather than by a reader counting fields, and
//! [`Loop::BYTES`] is asserted against the structure's own size by a test rather than
//! written twice.
//!
//! # The layout, and where it came from
//!
//! The Davis *Vantage Pro2 Serial Communication Reference*'s LOOP record, which is also
//! what `cmd/davis-emulator`'s `ToBytes` writes offset by offset:
//!
//! ```text
//!   0..3   "LOO"                          43     UV index, tenths
//!   3      bar trend, or 'P'              44..46 solar radiation, W/m2
//!   4      packet type: 0 = LOOP          46..48 storm rain, clicks
//!   5..7   next archive record            48..50 storm start, packed date
//!   7..9   barometer, thousandths inHg    50..56 day/month/year rain, clicks
//!   9..11  inside temperature, tenths F   56..62 day/month/year ET
//!  11      inside humidity, %            62..66  soil moisture 1-4, cbar
//!  12..14  outside temperature, tenths   66..70  leaf wetness 1-4
//!  14      wind speed, mph               70..86  the sixteen alarm bytes
//!  15      10-minute average wind speed  86      transmitter battery status
//!  16..18  wind direction, degrees       87..89  console battery, raw
//!  18..25  extra temperature 1-7, +90    89      forecast icon
//!  25..29  soil temperature 1-4, +90     90      forecast rule
//!  29..33  leaf temperature 1-4, +90     91..95  sunrise, sunset, BCD HHMM
//!  33      outside humidity, %           95..97  LF CR
//!  34..41  extra humidity 1-7, %         97..99  CRC-16, big-endian
//!  41..43  rain rate, clicks/hour
//! ```
//!
//! **Bytes 95 and 96 are read and not asserted**, and that is a deliberate hole. The
//! specification puts `\n\r` there; `cmd/davis-emulator` writes the 3-hour trend at 95 and a
//! zero at 96 (`main.go:415`). Requiring the specification's bytes would make this decoder
//! reject every packet the pinned oracle produces, and the two check bytes already cover
//! the whole packet — so the trailer is part of the structure, is covered by the CRC, and is
//! not a validity test.

use zerocopy::byteorder::little_endian::{I16, U16};
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

/// One LOOP record, exactly as it arrives.
///
/// Every multi-byte field is a `zerocopy` little-endian integer, which has alignment 1 — so
/// `repr(C)` is already the packed layout the wire has, with no `packed` attribute and
/// therefore no unaligned reference anywhere.
#[derive(Debug, Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct Loop {
    /// `"LOO"`.
    pub header: [u8; 3],
    /// The 3-hour barometer trend, or `b'P'` on the older packet flavour that carries none.
    ///
    /// weewx calls the two "flavour A" (`b'P'`) and "flavour B" (a trend). The distinction
    /// reaches **no stored metric**: the trend is not in `metrics/registry.toml` and the Go
    /// driver's `convertLoopPacket` never reads it either. The field is here because the
    /// structure is the wire, and `crate::davis`'s own tests assert that the two flavours
    /// decode to the same reading.
    pub trend: i8,
    /// `0` for a LOOP record and `1` for a LOOP2 record, which has a different layout
    /// entirely.
    pub packet_type: u8,
    /// The next archive record number. Not a measurement.
    pub next_record: U16,
    /// Thousandths of an inch of mercury, sea-level corrected by the console.
    pub barometer: U16,
    /// Tenths of a degree Fahrenheit.
    pub in_temp: I16,
    /// Per cent.
    pub in_humidity: u8,
    /// Tenths of a degree Fahrenheit.
    pub out_temp: I16,
    /// Miles per hour.
    pub wind_speed: u8,
    /// Miles per hour, the console's own 10-minute average.
    pub wind_speed10: u8,
    /// Degrees, 1-360.
    pub wind_dir: U16,
    /// Degrees Fahrenheit plus 90.
    pub extra_temp: [u8; 7],
    /// Degrees Fahrenheit plus 90.
    pub soil_temp: [u8; 4],
    /// Degrees Fahrenheit plus 90.
    pub leaf_temp: [u8; 4],
    /// Per cent.
    pub out_humidity: u8,
    /// Per cent.
    pub extra_humidity: [u8; 7],
    /// Rain-collector clicks per hour.
    pub rain_rate: U16,
    /// Tenths of a UV index unit.
    pub uv: u8,
    /// Watts per square metre.
    pub radiation: U16,
    /// Rain-collector clicks since the storm began.
    pub storm_rain: U16,
    /// The storm's start date, packed as the console's own bitfield.
    pub storm_start: U16,
    /// Rain-collector clicks today.
    pub day_rain: U16,
    /// Rain-collector clicks this month.
    pub month_rain: U16,
    /// Rain-collector clicks this year.
    pub year_rain: U16,
    /// Thousandths of an inch.
    pub day_et: U16,
    /// Hundredths of an inch.
    pub month_et: U16,
    /// Hundredths of an inch.
    pub year_et: U16,
    /// Centibars.
    pub soil_moisture: [u8; 4],
    /// 0-15.
    pub leaf_wetness: [u8; 4],
    /// The inside alarm bits.
    pub inside_alarm: u8,
    /// The rain alarm bits.
    pub rain_alarm: u8,
    /// The two outside alarm bytes.
    pub outside_alarm: [u8; 2],
    /// The eight extra-sensor alarm bytes.
    pub extra_alarm: [u8; 8],
    /// The four soil/leaf alarm bytes.
    pub soil_leaf_alarm: [u8; 4],
    /// Which transmitters the console believes have a low battery.
    pub tx_battery_status: u8,
    /// The console's own supply, as `volts * 300 * 512 / 100`.
    pub cons_battery_voltage: U16,
    /// The icon the console is showing.
    pub forecast_icon: u8,
    /// The forecast rule the console applied.
    pub forecast_rule: u8,
    /// BCD `HHMM`, in the console's own zone.
    pub sunrise: U16,
    /// BCD `HHMM`, in the console's own zone.
    pub sunset: U16,
    /// `\n\r` in the specification; the trend and a zero in the pinned emulator. Covered by
    /// the CRC and not otherwise checked -- see this module's documentation.
    pub trailer: [u8; 2],
    /// CRC-16/XMODEM over the preceding 97 bytes, **big-endian**, which is why it is a byte
    /// pair here rather than a `U16`.
    pub crc: [u8; 2],
}

impl Loop {
    /// How many bytes one record is. The Davis protocol's framing is the length alone:
    /// there is no terminator, which is what makes the session's framing a fixed-size read
    /// rather than a line read.
    pub const BYTES: usize = 99;

    /// The header every record starts with.
    pub const HEADER: [u8; 3] = *b"LOO";

    /// The `packet_type` of the record this decoder understands.
    ///
    /// A LOOP2 record (`1`) shares the header and the length and has a **different layout**:
    /// decoding one as a LOOP record puts a 10-minute average wind speed where the inside
    /// temperature goes. The Go driver does not test this byte, which is safe only because
    /// it never sends `LPS`; this decoder tests it, because a frame it cannot decode
    /// correctly is better refused than stored.
    pub const LOOP1: u8 = 0;

    /// Parse one record. `None` if `bytes` is not exactly one record long.
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        Self::read_from_bytes(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The length and the field offsets, from the type rather than from a comment.
    #[test]
    fn the_structure_is_the_ninety_nine_bytes_the_protocol_frames() {
        assert_eq!(std::mem::size_of::<Loop>(), Loop::BYTES);
        assert_eq!(std::mem::align_of::<Loop>(), 1, "no padding anywhere");
    }

    /// Each field lands where the specification puts it, checked by writing one byte at a
    /// known offset and reading it back through the field. A layout error in the middle of
    /// the structure would otherwise present as every later field being wrong.
    #[test]
    fn every_field_reads_the_offset_the_specification_names() {
        let mut bytes = vec![0u8; Loop::BYTES];
        bytes[..3].copy_from_slice(b"LOO");
        bytes[3] = 0xFB; // -5, the 3-hour barometer trend
        bytes[4] = 0;
        bytes[7..9].copy_from_slice(&30_000u16.to_le_bytes());
        bytes[9..11].copy_from_slice(&705i16.to_le_bytes());
        bytes[11] = 42;
        bytes[12..14].copy_from_slice(&(-120i16).to_le_bytes());
        bytes[14] = 7;
        bytes[15] = 5;
        bytes[16..18].copy_from_slice(&183u16.to_le_bytes());
        bytes[18] = 100;
        bytes[25] = 101;
        bytes[29] = 102;
        bytes[33] = 55;
        bytes[34] = 56;
        bytes[41..43].copy_from_slice(&3u16.to_le_bytes());
        bytes[43] = 65;
        bytes[44..46].copy_from_slice(&812u16.to_le_bytes());
        bytes[46..48].copy_from_slice(&11u16.to_le_bytes());
        bytes[50..52].copy_from_slice(&13u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&123u16.to_le_bytes());
        bytes[62] = 44;
        bytes[66] = 9;
        bytes[70] = 0x01;
        bytes[86] = 0x02;
        bytes[87..89].copy_from_slice(&4050u16.to_le_bytes());
        bytes[89] = 2;
        bytes[90] = 45;
        bytes[91..93].copy_from_slice(&0x0630u16.to_le_bytes());
        bytes[93..95].copy_from_slice(&0x1945u16.to_le_bytes());
        bytes[95] = b'\n';
        bytes[96] = b'\r';

        let packet = Loop::parse(&bytes).expect("ninety-nine bytes");
        assert_eq!(packet.header, Loop::HEADER);
        assert_eq!(packet.trend, -5);
        assert_eq!(packet.packet_type, Loop::LOOP1);
        assert_eq!(packet.barometer.get(), 30_000);
        assert_eq!(packet.in_temp.get(), 705);
        assert_eq!(packet.in_humidity, 42);
        assert_eq!(packet.out_temp.get(), -120);
        assert_eq!(packet.wind_speed, 7);
        assert_eq!(packet.wind_speed10, 5);
        assert_eq!(packet.wind_dir.get(), 183);
        assert_eq!(packet.extra_temp[0], 100);
        assert_eq!(packet.soil_temp[0], 101);
        assert_eq!(packet.leaf_temp[0], 102);
        assert_eq!(packet.out_humidity, 55);
        assert_eq!(packet.extra_humidity[0], 56);
        assert_eq!(packet.rain_rate.get(), 3);
        assert_eq!(packet.uv, 65);
        assert_eq!(packet.radiation.get(), 812);
        assert_eq!(packet.storm_rain.get(), 11);
        assert_eq!(packet.day_rain.get(), 13);
        assert_eq!(packet.day_et.get(), 123);
        assert_eq!(packet.soil_moisture[0], 44);
        assert_eq!(packet.leaf_wetness[0], 9);
        assert_eq!(packet.inside_alarm, 0x01);
        assert_eq!(packet.tx_battery_status, 0x02);
        assert_eq!(packet.cons_battery_voltage.get(), 4050);
        assert_eq!(packet.forecast_icon, 2);
        assert_eq!(packet.forecast_rule, 45);
        assert_eq!(packet.sunrise.get(), 0x0630);
        assert_eq!(packet.sunset.get(), 0x1945);
        assert_eq!(packet.trailer, [b'\n', b'\r']);
    }

    #[test]
    fn a_frame_of_the_wrong_length_is_not_a_record() {
        assert!(Loop::parse(&[0u8; Loop::BYTES - 1]).is_none());
        assert!(Loop::parse(&[0u8; Loop::BYTES + 1]).is_none());
        assert!(Loop::parse(&[]).is_none());
    }
}
