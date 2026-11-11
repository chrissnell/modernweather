//! The snow gauge driver's **names**. The body is `mw_grpc::snowgauge`'s.
//!
//! W2a §4a.5 maps the gauge as `GrpcOut { streaming: true }`, `Fixed`, `Service` — and
//! `ci/layers.toml` forbids this crate from naming `tonic`, because the gRPC *socket* and
//! every gRPC service belong to `mw-http` and `mw-grpc` (docs/service-architecture.md §1.4).
//! So the station component that dials the gauge cannot live here.
//!
//! What lives here is everything about the gauge that is **not** gRPC, which is exactly the
//! part two crates have to agree on:
//!
//! - [`STATION_TYPE`], the `type` column value, so `crate::plan` can recognise the row;
//! - [`station_id`], the component id, so `crate::plan`'s [`crate::fixed_receivers`] and
//!   `mw-grpc`'s own registrar mint the **same** one. Two functions that minted it
//!   independently would be two functions that can disagree, and the symptom would be a
//!   `Fixed` frame that resolves to no station: the registry resolves such a frame *by its
//!   receiver*, so an id mismatch is a gauge whose every reading is `unknown_station`.
//! - [`check`], the plan-time validation of the row's shape, so that a malformed gauge row
//!   is refused by the same function that refuses every other station's — with the same
//!   error type and the same sentences.
//!
//! `crate::plan` therefore **validates** a snow gauge row and emits no spec for it, and
//! says so in its own documentation. The alternative — refusing the row here — would make a
//! correctly configured gauge a startup failure, which is the opposite of the problem the
//! `NoDriver` refusal exists for.

use mw_component::{ComponentId, ComponentIdError};

/// The `type` column value this driver serves.
pub const STATION_TYPE: &str = "snowgauge";

/// The component id of one gauge's stream.
pub fn station_id(station_key: &str) -> Result<ComponentId, ComponentIdError> {
    ComponentId::new(format!("station:snowgauge:{station_key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_component_id_is_the_one_both_crates_mint() {
        assert_eq!(
            station_id("suncrest:gauge").expect("a legal id").as_str(),
            "station:snowgauge:suncrest:gauge"
        );
        assert!(station_id("Snow Gauge").is_err());
        assert_eq!(STATION_TYPE, "snowgauge");
    }
}
