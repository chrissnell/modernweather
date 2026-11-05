//! "Which sensor is which" — the WeatherLink Live sensor mapping, parsed.
//!
//! A WeatherLink Live reports a flat array of `conditions` records keyed by transmitter id
//! and says nothing about what is attached to which transmitter. Which one is the outdoor
//! Integrated Sensor Suite, which soil probe is in which port, and which transmitter's
//! battery is worth a column are facts about the deployment, so they are configuration:
//! `config.stations.wll_sensor_mapping`, in the format the Go driver already parses, so an
//! operator's existing value migrates unchanged (M7-4).
//!
//! ```text
//! th:1, rain:1, wind:1, windchill:1, thw:1:appTemp, th_indoor, baro, battery:1
//! └┬┘ │                              └──┬──┘
//!  │  └ transmitter id                  └ an option
//!  └ the role
//! ```
//!
//! **The parse is stricter than the Go one, in four places, and each rejection is a
//! configuration mistake that is otherwise silent.** `ParseMappingString` accepts any role
//! name and `transformToReading`'s `switch` then has no arm for it, so `wnid:1` is a wind
//! sensor that never reports and nothing anywhere says why; it accepts a port of 3 or 4 for
//! `leaf_wet` when the unit has two leaf channels; it accepts an unknown option, so
//! `thsw:1:apptemp` silently does not populate apparent temperature; and it accepts the
//! same sensor twice. All four are refused here, which makes them a plan error that names
//! the station and the column rather than a station that runs and reports less than it
//! should.

use std::fmt;

/// A parsed sensor mapping: the roles, in the order they were written.
///
/// Order is load-bearing for one role. The first `th` entry that matches a record is the
/// *outdoor* temperature and humidity; the second is `extratemp1`/`extrahumidity1`, and so
/// on. That is the Go behaviour and it is observable in stored data, so it is preserved
/// exactly — including the detail that a `th` entry which matches **no** record does not
/// consume an index (`transformToReading` increments its counter only after applying one).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SensorMapping {
    entries: Vec<SensorEntry>,
}

/// One mapping entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SensorEntry {
    /// What this sensor is.
    pub role: SensorRole,
    /// Which transmitter it is on. `None` only for the unit's own records.
    pub txid: Option<u8>,
    /// Which port, for the leaf/soil station's channels.
    pub port: Option<u8>,
    /// Whether this index should also populate apparent temperature.
    pub app_temp: bool,
}

/// The roles a WeatherLink Live sensor can play.
///
/// A closed enum rather than the Go string, because the whole of this module's value is
/// that a role nobody handles is rejected rather than ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SensorRole {
    /// Temperature and humidity. Ordered: see [`SensorMapping`].
    Th,
    /// Rain rate and daily rainfall.
    Rain,
    /// Wind speed and direction.
    Wind,
    /// Solar radiation.
    Solar,
    /// UV index.
    Uv,
    /// Wind chill, as the unit computes it.
    WindChill,
    /// Temperature–humidity–wind index.
    Thw,
    /// Temperature–humidity–solar–wind index.
    Thsw,
    /// The unit's internal barometer.
    Baro,
    /// The unit's internal temperature and humidity.
    ThIndoor,
    /// A soil temperature probe.
    SoilTemp,
    /// A soil moisture probe.
    SoilMoist,
    /// A leaf wetness sensor.
    LeafWet,
    /// A transmitter's battery flag.
    Battery,
}

impl SensorRole {
    /// The name this role is written as in the mapping string.
    pub const fn as_str(self) -> &'static str {
        match self {
            SensorRole::Th => "th",
            SensorRole::Rain => "rain",
            SensorRole::Wind => "wind",
            SensorRole::Solar => "solar",
            SensorRole::Uv => "uv",
            SensorRole::WindChill => "windchill",
            SensorRole::Thw => "thw",
            SensorRole::Thsw => "thsw",
            SensorRole::Baro => "baro",
            SensorRole::ThIndoor => "th_indoor",
            SensorRole::SoilTemp => "soil_temp",
            SensorRole::SoilMoist => "soil_moist",
            SensorRole::LeafWet => "leaf_wet",
            SensorRole::Battery => "battery",
        }
    }

    /// Every role, so the error message can list what was permitted.
    pub const ALL: &'static [SensorRole] = &[
        SensorRole::Th,
        SensorRole::Rain,
        SensorRole::Wind,
        SensorRole::Solar,
        SensorRole::Uv,
        SensorRole::WindChill,
        SensorRole::Thw,
        SensorRole::Thsw,
        SensorRole::Baro,
        SensorRole::ThIndoor,
        SensorRole::SoilTemp,
        SensorRole::SoilMoist,
        SensorRole::LeafWet,
        SensorRole::Battery,
    ];

    fn parse(name: &str) -> Option<Self> {
        SensorRole::ALL.iter().copied().find(|r| r.as_str() == name)
    }

    /// Whether this role is attached to a transmitter rather than to the unit itself.
    ///
    /// The two that are not are the unit's own barometer and its own temperature and
    /// humidity, which is also why they are matched by record type and everything else is
    /// matched by transmitter.
    pub const fn needs_txid(self) -> bool {
        !matches!(self, SensorRole::Baro | SensorRole::ThIndoor)
    }

    /// Whether this role names one channel of the leaf/soil station.
    pub const fn needs_port(self) -> bool {
        matches!(
            self,
            SensorRole::SoilTemp | SensorRole::SoilMoist | SensorRole::LeafWet
        )
    }

    /// The highest channel number this role has.
    const fn ports(self) -> u8 {
        match self {
            // The leaf/soil station has four soil channels and two leaf channels, and the
            // payload says so: `moist_soil_1..4`, `wet_leaf_1..2`. The Go parser's bound is
            // 1-4 for all three, so `leaf_wet:2:3` is accepted and then matches nothing.
            SensorRole::LeafWet => 2,
            _ => 4,
        }
    }

    /// Which `data_structure_type` a record for this role is expected to carry.
    pub const fn structure(self) -> i64 {
        match self {
            SensorRole::SoilTemp | SensorRole::SoilMoist | SensorRole::LeafWet => {
                super::wire::structure::LEAF_SOIL
            }
            SensorRole::Baro => super::wire::structure::BAROMETER,
            SensorRole::ThIndoor => super::wire::structure::TEMP_HUM,
            _ => super::wire::structure::ISS,
        }
    }
}

impl fmt::Display for SensorRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The one option the mapping format carries.
const APP_TEMP: &str = "appTemp";

/// Why a mapping string is not a mapping.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MappingError {
    /// Nothing, or only separators.
    #[error("the sensor mapping is empty")]
    Empty,

    /// A role nobody handles.
    #[error("`{found}` is not a sensor role; the roles are {}", roles())]
    UnknownRole {
        /// What was written.
        found: String,
    },

    /// A transmitter id that is not one.
    #[error("`{entry}`: `{found}` is not a transmitter id (1-8)")]
    BadTxid {
        /// The whole entry, so the operator can find it in the string.
        entry: String,
        /// What was written where the id goes.
        found: String,
    },

    /// A role that is on a transmitter, without one.
    #[error("`{entry}`: the role `{role}` is on a transmitter and names none")]
    MissingTxid {
        /// The whole entry.
        entry: String,
        /// The role.
        role: SensorRole,
    },

    /// A role that is the unit's own, with a transmitter id.
    #[error("`{entry}`: the role `{role}` is the unit's own and cannot name a transmitter")]
    UnexpectedTxid {
        /// The whole entry.
        entry: String,
        /// The role.
        role: SensorRole,
    },

    /// A channel number that is not one, or is out of range for the role.
    #[error("`{entry}`: `{found}` is not a port for `{role}` (1-{max})")]
    BadPort {
        /// The whole entry.
        entry: String,
        /// What was written where the port goes.
        found: String,
        /// The role.
        role: SensorRole,
        /// How many channels the role has.
        max: u8,
    },

    /// A role that names a channel, without one.
    #[error(
        "`{entry}`: the role `{role}` names one channel of the leaf/soil station and gives no port"
    )]
    MissingPort {
        /// The whole entry.
        entry: String,
        /// The role.
        role: SensorRole,
    },

    /// An option nobody reads.
    #[error("`{entry}`: `{found}` is not an option; the only option is `appTemp`")]
    UnknownOption {
        /// The whole entry.
        entry: String,
        /// What was written.
        found: String,
    },

    /// The same sensor twice, which would write the same metric twice.
    #[error("`{entry}` appears twice")]
    Duplicate {
        /// The whole entry.
        entry: String,
    },

    /// An option on a role that has none.
    #[error("`{entry}`: the role `{role}` takes no options")]
    UnexpectedOption {
        /// The whole entry.
        entry: String,
        /// The role.
        role: SensorRole,
    },
}

fn roles() -> String {
    SensorRole::ALL
        .iter()
        .map(|r| r.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

impl SensorMapping {
    /// Parse a mapping string.
    pub fn parse(text: &str) -> Result<Self, MappingError> {
        let mut entries: Vec<SensorEntry> = Vec::new();
        for part in text.split(',') {
            let entry = part.trim();
            if entry.is_empty() {
                // A trailing comma, or `a,,b`. Tolerated, as the Go parser tolerates it:
                // this is a hand-edited string and an empty element carries no intent.
                continue;
            }
            let parsed = Self::entry(entry)?;
            let same = |e: &SensorEntry| {
                e.role == parsed.role && e.txid == parsed.txid && e.port == parsed.port
            };
            if entries.iter().any(same) {
                return Err(MappingError::Duplicate {
                    entry: entry.to_string(),
                });
            }
            entries.push(parsed);
        }
        if entries.is_empty() {
            return Err(MappingError::Empty);
        }
        Ok(Self { entries })
    }

    fn entry(entry: &str) -> Result<SensorEntry, MappingError> {
        let mut parts = entry.split(':').map(str::trim);
        // `split` always yields one element and the caller rejected the empty entry.
        let name = parts.next().unwrap_or_default();
        let role = SensorRole::parse(name).ok_or_else(|| MappingError::UnknownRole {
            found: name.to_string(),
        })?;

        let txid = match parts.next().filter(|t| !t.is_empty()) {
            Some(text) => Some(
                text.parse::<u8>()
                    .ok()
                    .filter(|n| (1..=8).contains(n))
                    .ok_or(MappingError::BadTxid {
                        entry: entry.to_string(),
                        found: text.to_string(),
                    })?,
            ),
            None => None,
        };
        match (role.needs_txid(), txid) {
            (true, None) => {
                return Err(MappingError::MissingTxid {
                    entry: entry.to_string(),
                    role,
                })
            }
            (false, Some(_)) => {
                return Err(MappingError::UnexpectedTxid {
                    entry: entry.to_string(),
                    role,
                })
            }
            _ => {}
        }

        let rest: Vec<&str> = parts.filter(|p| !p.is_empty()).collect();
        let mut port = None;
        let mut app_temp = false;
        if role.needs_port() {
            let max = role.ports();
            let text = rest.first().copied().ok_or(MappingError::MissingPort {
                entry: entry.to_string(),
                role,
            })?;
            port = Some(
                text.parse::<u8>()
                    .ok()
                    .filter(|n| (1..=max).contains(n))
                    .ok_or_else(|| MappingError::BadPort {
                        entry: entry.to_string(),
                        found: text.to_string(),
                        role,
                        max,
                    })?,
            );
            if let Some(extra) = rest.get(1) {
                return Err(MappingError::UnknownOption {
                    entry: entry.to_string(),
                    found: (*extra).to_string(),
                });
            }
        } else {
            for option in rest {
                if option != APP_TEMP {
                    return Err(MappingError::UnknownOption {
                        entry: entry.to_string(),
                        found: option.to_string(),
                    });
                }
                // `appTemp` on anything but the two indices that compute an apparent
                // temperature is a value with nowhere to go, so it is refused rather than
                // ignored.
                if !matches!(role, SensorRole::Thw | SensorRole::Thsw) {
                    return Err(MappingError::UnexpectedOption {
                        entry: entry.to_string(),
                        role,
                    });
                }
                app_temp = true;
            }
        }

        Ok(SensorEntry {
            role,
            txid,
            port,
            app_temp,
        })
    }

    /// The entries, in the order they were written.
    pub fn entries(&self) -> &[SensorEntry] {
        &self.entries
    }

    /// How many entries the mapping has.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the mapping has no entries. It cannot: [`SensorMapping::parse`] rejects an
    /// empty one, and the only other constructor is `Default`, which no driver uses.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl fmt::Display for SensorMapping {
    /// Round-trips through [`SensorMapping::parse`], which is what the test below asserts
    /// — so a mapping can be rendered back into the column it came from without a second
    /// serialiser that could disagree with the parser.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (n, e) in self.entries.iter().enumerate() {
            if n > 0 {
                f.write_str(", ")?;
            }
            f.write_str(e.role.as_str())?;
            if let Some(txid) = e.txid {
                write!(f, ":{txid}")?;
            }
            if let Some(port) = e.port {
                write!(f, ":{port}")?;
            }
            if e.app_temp {
                write!(f, ":{APP_TEMP}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Go tree's `vp2_plus` template, which is the mapping most deployments have.
    const VP2_PLUS: &str = "th:1, rain:1, wind:1, uv:1, solar:1, windchill:1, thw:1, \
                            thsw:1:appTemp, th_indoor, baro, battery:1";

    #[test]
    fn the_shipped_templates_parse() {
        // All four of `templates.go`, verbatim.
        for template in [
            "th:1, rain:1, wind:1, windchill:1, thw:1:appTemp, th_indoor, baro, battery:1",
            VP2_PLUS,
            "th:1, rain:1, wind:2, uv:1, solar:1, windchill:1, thw:1, thsw:1:appTemp, \
             th_indoor, baro, battery:1, battery:2",
            "th:1, rain:1, wind:1, uv:1, solar:1, windchill:1, thw:1, thsw:1:appTemp, \
             soil_temp:2:1, soil_temp:2:2, soil_temp:2:3, soil_temp:2:4, soil_moist:2:1, \
             soil_moist:2:2, soil_moist:2:3, soil_moist:2:4, leaf_wet:2:1, leaf_wet:2:2, \
             th_indoor, baro, battery:1, battery:2",
        ] {
            let parsed = SensorMapping::parse(template)
                .unwrap_or_else(|e| panic!("`{template}` should parse: {e}"));
            assert!(parsed.len() >= 8, "{template}");
        }
    }

    #[test]
    fn a_mapping_renders_back_into_itself() {
        let parsed = SensorMapping::parse(VP2_PLUS).expect("parses");
        let rendered = parsed.to_string();
        assert_eq!(rendered, VP2_PLUS, "the rendering is not the input");
        assert_eq!(SensorMapping::parse(&rendered).expect("re-parses"), parsed);
    }

    #[test]
    fn the_order_of_th_entries_is_preserved() {
        let m = SensorMapping::parse("th:2, th:1, th:3").expect("parses");
        let ids: Vec<_> = m.entries().iter().map(|e| e.txid).collect();
        assert_eq!(ids, vec![Some(2), Some(1), Some(3)]);
    }

    /// The four the Go parser accepts and the transformer then cannot use.
    #[test]
    fn the_four_silent_configuration_mistakes_are_refused() {
        assert!(matches!(
            SensorMapping::parse("wnid:1"),
            Err(MappingError::UnknownRole { .. })
        ));
        assert!(matches!(
            SensorMapping::parse("leaf_wet:2:3"),
            Err(MappingError::BadPort { max: 2, .. })
        ));
        assert!(matches!(
            SensorMapping::parse("thsw:1:apptemp"),
            Err(MappingError::UnknownOption { .. })
        ));
        assert!(matches!(
            SensorMapping::parse("wind:1, wind:1"),
            Err(MappingError::Duplicate { .. })
        ));
    }

    #[test]
    fn a_transmitter_is_required_exactly_where_there_is_one() {
        assert!(matches!(
            SensorMapping::parse("wind"),
            Err(MappingError::MissingTxid { .. })
        ));
        assert!(matches!(
            SensorMapping::parse("baro:1"),
            Err(MappingError::UnexpectedTxid { .. })
        ));
        assert!(matches!(
            SensorMapping::parse("th_indoor:1"),
            Err(MappingError::UnexpectedTxid { .. })
        ));
        assert!(matches!(
            SensorMapping::parse("wind:0"),
            Err(MappingError::BadTxid { .. })
        ));
        assert!(matches!(
            SensorMapping::parse("wind:9"),
            Err(MappingError::BadTxid { .. })
        ));
        assert!(matches!(
            SensorMapping::parse("wind:x"),
            Err(MappingError::BadTxid { .. })
        ));
    }

    #[test]
    fn a_port_is_required_where_the_role_names_a_channel() {
        assert!(matches!(
            SensorMapping::parse("soil_temp:2"),
            Err(MappingError::MissingPort { .. })
        ));
        assert_eq!(
            SensorMapping::parse("soil_temp:2:4")
                .expect("parses")
                .entries()[0]
                .port,
            Some(4)
        );
    }

    #[test]
    fn app_temp_belongs_only_to_the_two_indices_that_have_one() {
        assert!(SensorMapping::parse("thw:1:appTemp").is_ok());
        assert!(SensorMapping::parse("thsw:1:appTemp").is_ok());
        assert!(matches!(
            SensorMapping::parse("th:1:appTemp"),
            Err(MappingError::UnexpectedOption { .. })
        ));
    }

    #[test]
    fn an_empty_mapping_is_refused_rather_than_decoding_to_nothing() {
        assert_eq!(SensorMapping::parse(""), Err(MappingError::Empty));
        assert_eq!(SensorMapping::parse("  ,, "), Err(MappingError::Empty));
        // But a trailing comma is a hand-edit, not an intent.
        assert_eq!(SensorMapping::parse("baro,").expect("parses").len(), 1);
    }

    #[test]
    fn every_role_declares_where_its_record_comes_from() {
        for role in SensorRole::ALL {
            assert!((1..=4).contains(&role.structure()), "{role}");
            assert!(!role.as_str().is_empty());
        }
        assert_eq!(SensorRole::Baro.structure(), 3);
        assert_eq!(SensorRole::ThIndoor.structure(), 4);
        assert_eq!(SensorRole::SoilTemp.structure(), 2);
        assert_eq!(SensorRole::Th.structure(), 1);
    }
}
