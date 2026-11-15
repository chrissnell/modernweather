//! `almanac:records`' declaration — which stations it covers, and the two handles it needs.

use std::sync::Arc;
use std::time::Duration;

use mw_component::{
    AlmanacCache, Builder, Capability, ComponentId, ComponentIdError, ComponentSpec, ConfigDigest,
    FirstRun, Freshness, Need, Phase, ReadingStore, Store, Trigger,
};
use mw_types::StationKey;
use tracing::{debug, info};

use super::{Records, DEADLINE, ID, PERIOD};

/// One station [`register`] plans records for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlmanacStation {
    /// Whose.
    pub key: StationKey,
    /// `config.stations.type` — the **configured** device type, which selects the two
    /// records that are device-specific. `super::records_for` has which and why.
    pub kind: String,
}

/// What this crate needs from the configuration to plan `almanac:records`.
pub trait AlmanacConfig {
    /// Whether this node has a weather database. Without one there is no `db:postgres`, so
    /// neither hard need below would have a provider and `plan()` would refuse the whole
    /// configuration (**D-44**).
    fn has_weather_database(&self) -> bool;

    /// Every station whose records this node maintains, in station order.
    fn almanac_stations(&self) -> Vec<AlmanacStation>;
}

/// Declare `almanac:records`.
///
/// # One component or none
///
/// No weather database plans nothing, for `mw_pipeline::rollup::register`'s reason. No
/// station plans nothing either, and says so at `debug` rather than `warn`: a node that
/// collects for nobody and serves no website has no almanac to compute, and there is
/// nothing for an operator to fix.
///
/// There is no third refusal. Unlike `snow:*` this component needs no calibration and no
/// time zone — every one of the twelve records is an extreme over the whole history, and an
/// all-time maximum has no local boundary to be measured from. That absence is what makes
/// this a single component with a station loop rather than one component per station.
pub fn register<C: AlmanacConfig + ?Sized>(
    cfg: &C,
    out: &mut Vec<ComponentSpec>,
) -> Result<(), AlmanacPlanError> {
    if !cfg.has_weather_database() {
        return Ok(());
    }
    let stations = cfg.almanac_stations();
    if stations.is_empty() {
        debug!(
            "no station is configured, so `almanac:records` is not planned -- there are no \
             extremes to compute and /almanac answers `{{}}` for every name, which is what \
             it does for an unknown station anyway"
        );
        return Ok(());
    }
    info!(
        stations = stations.len(),
        period_secs = PERIOD.as_secs(),
        "planned `almanac:records`: the all-time extremes behind /almanac, recomputed on a \
         schedule"
    );
    out.push(spec(Arc::<[AlmanacStation]>::from(stations))?);
    Ok(())
}

/// Why a configuration does not produce an almanac plan.
///
/// One variant, and it is the structural one every registrar in this crate has: a component
/// id that cannot be one. There is no configuration an operator can write that reaches it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AlmanacPlanError {
    /// A component id that cannot be one.
    #[error("`{what}` cannot name a component")]
    BadId {
        /// What was being named.
        what: String,
        /// Why.
        #[source]
        source: ComponentIdError,
    },
}

/// The spec.
fn spec(stations: Arc<[AlmanacStation]>) -> Result<ComponentSpec, AlmanacPlanError> {
    let id = component_id(ID)?;
    let digest = digest_of(&stations);
    Ok(ComponentSpec::new(
        id,
        Trigger::Every {
            period: PERIOD,
            deadline: DEADLINE,
            // **`Immediate`, unlike every `snow:*` component**, and the difference is the
            // state this component was written for. The snow cache persists, so a restart
            // serves the last computed depth and thirty seconds of staleness is invisible.
            // An almanac cache that has never been written serves `{}` — a frozen endpoint
            // answering `200` with nothing in it — so waiting a period before the first
            // occurrence would reproduce the defect for an hour on every fresh deployment,
            // which is exactly the hour an operator looks at the page.
            first: FirstRun::Immediate,
            // `Spread`, not `Aligned`: nothing here reads a bucket that materialises on a
            // wall-clock boundary. The day level's refresh policy runs every thirty minutes
            // and this reads whatever is closed, so there is no boundary to be early for.
            phase: Phase::Spread,
            // No floor. **W2a Decision 56**'s floors exist for third-party rate limits —
            // Weather Underground's sixty seconds — and every read here is local. An
            // operator who wants these recomputed every five minutes is paying for it in
            // their own database and nobody else's.
            floor: Duration::ZERO,
        },
        Builder::tick(move |cx| {
            let stations = Arc::clone(&stations);
            async move {
                let cache = cx.hard::<Arc<dyn AlmanacCache>>()?;
                // The upcast to the half this component declared, which is what makes
                // `ReadingStore H` a fact of the type rather than only of the declaration:
                // after this line the body cannot store a reading, even though both halves
                // share the capability.
                let store: Arc<dyn ReadingStore> = cx.hard::<Arc<dyn Store>>()?;
                Ok(Records {
                    stations,
                    store,
                    cache,
                })
            }
        }),
    )
    .requires([
        Need::hard_write(Capability::AlmanacCache),
        Need::hard(Capability::ReadingStore),
    ])
    // **W2a Decision 20's declaration for the almanac**, and the reason it is a freshness
    // rather than an edge is the same as the snow cache's: `/almanac` depending on the
    // records having been computed is *data presence*, not capability availability, and
    // modelling it as a startup edge would mean the website does not serve until the first
    // occurrence has finished.
    //
    // Rendered by `mw_api_mgmt::freshness` as "Almanac records last calculated 20 minutes
    // ago", or "Almanac records have never been calculated" — which is the sentence that
    // was missing while the cache was empty, and the whole reason this declaration is worth
    // more here than it is on a component whose table is never empty.
    //
    // **Twice the period**, which is the bound `snow:depth` uses: two missed occurrences is
    // already a component that is not keeping up, and these numbers change at most once a
    // day.
    .freshness([Freshness {
        capability: Capability::AlmanacCache,
        populated_by: component_id(ID)?,
        expected_max_age: PERIOD * 2,
    }])
    .config_digest(digest))
}

/// What goes into the spec's [`ConfigDigest`].
///
/// The stations and their configured types: a station added, removed or re-typed restarts
/// the component, and a change elsewhere does not. The **type** is in the digest and not
/// only the key, because it is what selects `high_pm10_in`'s column — an AirGradient
/// retyped as an Ambient AQIN reads a different column for the same record.
fn digest_of(stations: &[AlmanacStation]) -> ConfigDigest {
    let mut s = String::new();
    for station in stations {
        s.push_str(station.key.as_str());
        s.push('=');
        s.push_str(&station.kind);
        s.push(';');
    }
    ConfigDigest::of(&s)
}

/// A component id, with the refusal naming what could not be one.
fn component_id(name: &str) -> Result<ComponentId, AlmanacPlanError> {
    ComponentId::new(name).map_err(|source| AlmanacPlanError::BadId {
        what: name.to_string(),
        source,
    })
}
