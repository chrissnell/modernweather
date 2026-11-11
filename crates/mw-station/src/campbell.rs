//! The Campbell Scientific driver: what the component is called, and what it is made of.
//!
//! There is almost nothing here, and that is the point. Campbell is `Serial | TcpOut`, a
//! line-delimited session ([`crate::lines::Lines`]) and a pure codec
//! ([`mw_codec::CampbellAscii`]) — every one of which exists for another driver — so the
//! whole of this file is the two names and the one constant that are *this* station's.
//! `crate::driver` is the body both it and Davis run in.
//!
//! The Go driver is 352 lines for the same thing, and the difference is almost entirely the
//! two nested reconnect loops: `GetCampbellScientificPackets` closes the connection and
//! calls `Connect` on an error, `Connect` loops forever with a sleep, and
//! `ConnectToStation` separately decodes one packet before returning so that startup can
//! log "Station is alive". Here a connection is a *build* (**W2a Decision 3**) and a
//! reconnect is the supervisor's `Backoff` — so there is no loop to write and the states an
//! operator sees are `Starting`, `Running` and `Backoff { until }` rather than a log line.
//!
//! # The configured type is `campbellscientific` and the driver name is `campbell`
//!
//! `docs/legacy-api-contract.md` §3.1 is explicit that `/stationinfo`'s configured type and
//! `/latest`'s driver-self-reported `stationtype` "are not the same set and must not be
//! unified", and Campbell is the case that proves it: five frozen fixtures carry
//! `campbellscientific` and the Go driver stamps `campbell` on every reading. So
//! [`STATION_TYPE`] is what the `type` column holds — and the schema's own `CHECK` lists it
//! in that spelling — while the driver name is the codec's
//! ([`mw_codec::Codec::name`]).

use mw_component::{ComponentId, ComponentIdError};

/// The `type` column value this driver serves.
///
/// The frozen contract's spelling, which is also the schema's `CHECK` list's.
pub const STATION_TYPE: &str = "campbellscientific";

/// The component id of one station's line reader.
pub fn station_id(station_key: &str) -> Result<ComponentId, ComponentIdError> {
    ComponentId::new(format!("station:campbell:{station_key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_component_id_and_the_configured_type_are_the_ones_the_contract_names() {
        assert_eq!(
            station_id("suncrest:meadow").expect("a legal id").as_str(),
            "station:campbell:suncrest:meadow"
        );
        assert!(station_id("Meadow Station").is_err());
        assert_eq!(STATION_TYPE, "campbellscientific");
        // The distinction §3.1 forbids unifying: the configured type and the driver name
        // are different strings for the same station.
        assert_ne!(
            STATION_TYPE,
            mw_codec::Codec::name(&mw_codec::CampbellAscii::default())
        );
    }
}
