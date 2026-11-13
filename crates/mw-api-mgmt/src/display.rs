//! **D-36 as code**: the component model's vocabulary must not reach a screen, and this is
//! the one place the translation happens.
//!
//! W5 §0.2 is the table and W5 §0.4 is the state and trigger rendering. Both are implemented
//! **here, once, on the server** — which is §2.3's first deliberate choice and worth
//! restating, because the obvious alternative is a mapping in the client:
//!
//! > If translation lived in the UI, every new component would need a UI change to be
//! > nameable, and the §6.6 lint could not see the mapping.
//!
//! So `display.name`, `display.qualifier`, `display.section` and `display.href` come from
//! the server and the UI renders what it is given. The internal `id` travels beside them for
//! keying, deep links and support, and is rendered raw only inside `System → Advanced`.
//!
//! # Why a [`Vocabulary`] rather than a free function
//!
//! Because the qualifier is a *name*, not an id. §2.3's worked example renders `wu:backyard`
//! as "Weather Underground" / "Backyard" — and "Backyard" is the station's `name` column,
//! which an id cannot supply: the component's suffix is the **uploader's** name. A free
//! function over the id alone would have to render `wu:backyard` as "backyard", lower-case
//! and wrong, on the one screen the operator reads during an incident. So the translation
//! takes the configuration with it.
//!
//! # Three places plain language is the wrong call, and they are not fixed later
//!
//! §0.3: **APRS / CWOP** keeps its name because the person configuring it holds a licence;
//! **serial port** and **baud rate** keep theirs because the person configuring them holds a
//! cable; **hostname**, **port** and **TLS certificate** keep theirs because the person
//! configuring them owns the domain. Those are decisions, recorded here so that a later
//! reader does not "improve" them.

use std::collections::BTreeMap;
use std::time::Duration;

use mw_component::{Capability, ComponentId, ComponentStatus, State, WaitingOn};

use crate::view::ConfigView;

/// Which part of the UI owns a thing.
///
/// §2.3's list exactly. A closed enum rather than a string because the client switches on
/// it to choose a navigation group, and a typo would be a silently unreachable screen.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Section {
    /// The overview, where §2.5's root-cause panel lives.
    Home,
    /// "My weather stations".
    Stations,
    /// "Share my data".
    Share,
    /// "My website".
    Website,
    /// "Data storage".
    Storage,
    /// "System".
    System,
}

impl Section {
    /// The section's own page, which is the fallback `href` for anything in it.
    pub const fn href(self) -> &'static str {
        match self {
            Section::Home => "/",
            Section::Stations => "/stations",
            Section::Share => "/share",
            Section::Website => "/website",
            Section::Storage => "/data-storage",
            Section::System => "/system",
        }
    }
}

/// What a component is called on screen.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Display {
    /// What it is: "Weather Underground", "Saving readings", "your Davis station".
    pub name: String,
    /// Which station or website it belongs to, when it belongs to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualifier: Option<String>,
    /// Which part of the UI owns it.
    pub section: Section,
    /// The page that owns it.
    pub href: String,
}

impl Display {
    /// `name`, or `name (qualifier)` — the one-line form §2.5's `blocking` list uses.
    pub fn one_line(&self) -> String {
        match &self.qualifier {
            Some(q) => format!("{} ({q})", self.name),
            None => self.name.clone(),
        }
    }
}

/// The §0.2 table, plus the configuration it needs to render a qualifier.
///
/// Built per response from one [`ConfigView`], which is the same read every other resource
/// is a projection of — so a component's qualifier and the station page's title cannot
/// disagree about a station's name.
#[derive(Debug, Clone, Default)]
pub struct Vocabulary {
    /// `station_key` → the operator's name for it.
    stations: BTreeMap<String, String>,
    /// `website.id` → the operator's name for it.
    websites: BTreeMap<i64, String>,
    /// An upload account's `name` → the one station it uploads for, as that station's name.
    ///
    /// One station, because W7 **D-38** makes the one-station arity a property of the
    /// `wu`/`pws`/`aprs` *kind*. A second pairing is `PlanError::UploaderArity` and there is
    /// no component to name.
    uploads_for: BTreeMap<String, String>,
}

impl Vocabulary {
    /// The vocabulary a response renders with.
    pub fn new(cfg: &ConfigView) -> Self {
        let stations: BTreeMap<String, String> = cfg
            .stations
            .iter()
            .map(|s| (s.key.clone(), s.name.clone()))
            .collect();
        let mut uploads_for = BTreeMap::new();
        for service in &cfg.services {
            if let Some(first) = service.stations.first() {
                let name = stations
                    .get(&first.station_key)
                    .cloned()
                    .unwrap_or_else(|| first.station_key.clone());
                uploads_for.insert(service.name.clone(), name);
            }
        }
        Self {
            stations,
            websites: cfg
                .websites
                .iter()
                .map(|w| (w.id, w.name.clone()))
                .collect(),
            uploads_for,
        }
    }

    /// A station's name, or its key when the configuration no longer has it.
    ///
    /// The fallback is reachable and is not a defect: a component whose station was deleted
    /// in this same generation is still `Stopping` while the plan diff is applied, and
    /// "Backyard" having become `backyard` for two seconds is better than a blank.
    fn station_name(&self, key: &str) -> String {
        self.stations
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }

    /// What this component is called on screen.
    ///
    /// **Total.** Every id gets a rendering, including one this function has never seen: an
    /// unknown family renders as its own first segment with the `System` section, which is
    /// the Advanced view's "here is a component you have not been given a page for". The
    /// alternative — an `Option` — would mean a new component family could make a screen
    /// render a blank, and `Running` with no name beside it is worse than a technical name.
    pub fn describe(&self, id: &ComponentId) -> Display {
        let s = id.as_str();
        let (family, rest) = match s.split_once(':') {
            Some((f, r)) => (f, r),
            None => (s, ""),
        };
        match family {
            "api" | "http" => Display {
                name: "Website & API".into(),
                qualifier: None,
                section: Section::Website,
                href: "/website".into(),
            },
            "web" => {
                // `web:site:<id>`.
                let site = rest.strip_prefix("site:").unwrap_or(rest);
                let qualifier = site
                    .parse::<i64>()
                    .ok()
                    .and_then(|n| self.websites.get(&n).cloned());
                Display {
                    name: "your website".into(),
                    qualifier,
                    section: Section::Website,
                    href: "/website".into(),
                }
            }
            "db" => Display {
                name: "the weather database".into(),
                qualifier: None,
                section: Section::Storage,
                href: "/data-storage".into(),
            },
            "writer" => Display {
                name: "Saving readings".into(),
                qualifier: None,
                section: Section::Storage,
                href: "/data-storage".into(),
            },
            "window" => Display {
                name: "Recent-conditions calculations".into(),
                qualifier: None,
                section: Section::System,
                href: "/system/diagnostics".into(),
            },
            "rain" => Display {
                name: "Rainfall day boundary".into(),
                qualifier: rest
                    .rsplit(':')
                    .next()
                    .filter(|k| !k.is_empty())
                    .map(|k| self.station_name(k)),
                section: Section::System,
                href: "/system/diagnostics".into(),
            },
            "snapshot" => Display {
                name: "Current conditions".into(),
                qualifier: None,
                section: Section::System,
                href: "/system/diagnostics".into(),
            },
            "snow" => Display {
                name: "Snow calculations".into(),
                qualifier: None,
                section: Section::System,
                href: "/system/diagnostics".into(),
            },
            "aeris" => Display {
                name: "Forecasts & alerts".into(),
                qualifier: None,
                section: Section::Share,
                href: "/share/forecasts".into(),
            },
            "wu" => self.upload("Weather Underground", "/share/weather-underground", rest),
            "pws" => self.upload("PWS Weather", "/share/pws-weather", rest),
            // Kept verbatim on purpose (§0.3): the audience holds a callsign.
            "aprs" => self.upload("APRS / CWOP", "/share/aprs", rest),
            "station" => self.station(rest),
            "mgmt" => Display {
                name: "Admin".into(),
                qualifier: None,
                section: Section::System,
                href: "/system".into(),
            },
            other => Display {
                name: other.to_string(),
                qualifier: (!rest.is_empty()).then(|| rest.to_string()),
                section: Section::System,
                href: "/system/advanced".into(),
            },
        }
    }

    fn upload(&self, name: &str, href: &str, account: &str) -> Display {
        Display {
            name: name.into(),
            qualifier: self.uploads_for.get(account).cloned(),
            section: Section::Share,
            href: href.into(),
        }
    }

    /// `station:<driver>:<key>`, `station:ambient`, `station:grpcrx`, `station:wll:udp:<port>`.
    fn station(&self, rest: &str) -> Display {
        let (driver, key) = match rest.split_once(':') {
            Some((d, k)) => (d, k),
            None => (rest, ""),
        };
        // §0.2 names each device type. The two that are not a driver at all are named by
        // what they do, because that is what the operator configured.
        let (name, section, href) = match driver {
            "davis" => ("your Davis station", Section::Stations, None),
            "campbell" => ("your Campbell Scientific station", Section::Stations, None),
            "airgradient" => ("AirGradient air-quality sensor", Section::Stations, None),
            "snowgauge" => ("Snow depth sensor", Section::Stations, None),
            "wll" => ("your WeatherLink Live station", Section::Stations, None),
            "ambient" => (
                "Ambient Weather station (sends to us)",
                Section::Stations,
                None,
            ),
            "grpcrx" => (
                "Receive readings from another RemoteWeather",
                Section::Storage,
                Some("/data-storage"),
            ),
            _ => ("your weather station", Section::Stations, None),
        };
        // `station:wll:udp:<port>` is one broadcast listener for every WeatherLink unit on
        // the segment rather than a station's own component, so it has no station to
        // qualify with and must not borrow a port number as one.
        let key = key.strip_prefix("udp:").map_or(key, |_| "");
        Display {
            name: name.into(),
            qualifier: (!key.is_empty()).then(|| self.station_name(key)),
            section,
            href: href.map_or_else(
                || {
                    if key.is_empty() {
                        section.href().to_string()
                    } else {
                        format!("/stations/{key}")
                    }
                },
                str::to_string,
            ),
        }
    }
}

/// What a capability is, in the words of the thing it actually is.
///
/// W2a §3.1 added the provider to `WaitingOn` **for this document**, and §2.3 states why:
/// "waiting on `ReadingStore`" is unactionable, "waiting for the weather database" is a
/// sentence an operator can act on. This is the half that turns the capability into the
/// thing.
pub const fn capability_as_a_thing(cap: Capability) -> &'static str {
    match cap {
        Capability::ReadingStore => "the weather database",
        Capability::ForecastStore | Capability::AlertStore => "forecasts and alerts",
        Capability::SnowCache => "snow calculations",
        Capability::WindowedView => "recent-conditions calculations",
        Capability::LatestSnapshot => "current conditions",
        Capability::ConfigStore | Capability::ConfigAdmin => "the configuration",
        Capability::StationRegistry => "the station directory",
        Capability::HttpRoutes => "the website and API",
        Capability::ReadingStream => "incoming readings",
        Capability::ComponentControl => "the supervisor",
        // `Capability` is `#[non_exhaustive]`, so a capability added in another crate still
        // renders rather than failing to compile a screen. It reads as a thing, not as a
        // name: "something this needs" is honest where the enum variant would be jargon.
        _ => "something this needs",
    }
}

/// The badge a state gets, as §0.4's table names it.
///
/// Note that the badge is **not** the state's own word: `Failed` is badged **Stopped** and
/// `Stopped` is badged **Off**, because the only way an operator deliberately reaches
/// `Stopped` is by turning something off — and a red "Failed" on something they switched off
/// themselves is the UI lying in the other direction.
/// `String` rather than `&'static str`, even though every value is a literal: the badge is
/// deserialised by the generated TypeScript client's Rust-side counterpart in this
/// repository's own tests, and `Deserialize` cannot produce a `&'static str`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Badge {
    /// The badge's text.
    pub label: String,
    /// Its tone: `info`, `success`, `warning`, `danger` or `neutral`.
    pub tone: String,
}

fn badge(label: &str, tone: &str) -> Badge {
    Badge {
        label: label.to_string(),
        tone: tone.to_string(),
    }
}

/// §0.4's badge for one state.
pub fn badge_for(state: &State) -> Badge {
    match state {
        // "A planned-but-not-started component is invisible; the operator sees the first
        // real state. Appears only in the Advanced view."
        State::Planned => badge("Planned", "neutral"),
        State::Waiting { .. } => badge("Waiting", "info"),
        State::Starting { attempt } if *attempt > 1 => badge("Starting", "warning"),
        State::Starting { .. } => badge("Starting", "info"),
        State::Running { degraded } if !degraded.is_empty() => badge("Running, limited", "warning"),
        State::Running { .. } => badge("Running", "success"),
        State::Failed { .. } => badge("Stopped", "danger"),
        State::Backoff { .. } => badge("Retrying", "warning"),
        State::Finished { .. } => badge("Done", "neutral"),
        State::Stopping { .. } => badge("Stopping", "neutral"),
        State::Stopped => badge("Off", "neutral"),
    }
}

/// §0.4's sentence for one state, in the operator's words.
///
/// `now` is passed rather than read, because §3.3 keeps the clock out of everything that is
/// not a component tick — and the countdown in "Retrying in 12 seconds" is rendered from a
/// **server-supplied absolute timestamp** on the client (W5 **D-46**), so this sentence is
/// the prose and `Component::retry_at` is the number the client counts down.
pub fn sentence_for(
    state: &State,
    vocab: &Vocabulary,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    match state {
        State::Planned => "Not started yet.".to_string(),
        State::Waiting { on } => {
            let things = waiting_things(on, vocab);
            format!("Can't start yet — waiting for {things}.")
        }
        State::Starting { attempt } if *attempt > 1 => {
            format!("Starting — attempt {attempt}.")
        }
        State::Starting { .. } => "Starting…".to_string(),
        State::Running { degraded } if !degraded.is_empty() => {
            let missing = join_plainly(
                &degraded
                    .iter()
                    .map(|c| capability_as_a_thing(*c).to_string())
                    .collect::<Vec<_>>(),
            );
            format!("Running, but {missing} isn't available.")
        }
        State::Running { .. } => "Running.".to_string(),
        State::Failed { error, .. } => format!("Stopped because of an error: {error}"),
        State::Backoff { until, attempt } => {
            let secs = (*until - now).num_seconds().max(0);
            if *attempt > 5 {
                format!("Retrying in {secs} seconds — {attempt} attempts so far.")
            } else {
                format!("Retrying in {secs} seconds.")
            }
        }
        State::Finished { at } => format!("Finished at {}.", at.to_rfc3339()),
        State::Stopping { since } => {
            if (now - *since) > chrono::Duration::seconds(10) {
                "Stopping — waiting for it to finish.".to_string()
            } else {
                "Stopping…".to_string()
            }
        }
        State::Stopped => "Turned off.".to_string(),
    }
}

/// "the weather database" / "the weather database and snow calculations".
///
/// Named by the **provider** where the plan has one and by the capability where it does not,
/// which is the difference W2a §3.1 added `WaitingOn::provider` for. A provider the plan
/// names is a thing with a page; a capability with no provider is a configuration that does
/// not contain one, and saying "the weather database" when none is configured would send the
/// operator to a page about a row that is not there.
fn waiting_things(on: &[WaitingOn], vocab: &Vocabulary) -> String {
    let mut things: Vec<String> = Vec::new();
    for w in on {
        let named = w.provider.as_ref().map(|id| vocab.describe(id).one_line());
        let thing = named.unwrap_or_else(|| capability_as_a_thing(w.capability).to_string());
        if !things.contains(&thing) {
            things.push(thing);
        }
    }
    if things.is_empty() {
        return "something it needs".to_string();
    }
    join_plainly(&things)
}

/// "a", "a and b", "a, b and c" — the Oxford-free list §0.4's examples use.
fn join_plainly(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// §0.4's schedule sentence for one trigger.
///
/// Takes the trigger's `&'static str` kind and its period rather than the `Trigger` itself:
/// `ComponentStatus` carries exactly those two, which is deliberate — the status is what
/// crosses the capability boundary and a whole `Trigger` would drag `Builder` with it.
///
/// `OnDemand` has **no sentence**, which is §0.4's own answer: it is a button.
pub fn schedule_sentence(trigger: &str, period: Option<Duration>) -> Option<String> {
    match trigger {
        "Every" => period.map(|p| format!("Runs {}.", every(p))),
        "Calendar" => Some("Runs on a schedule.".to_string()),
        "OnReading" => Some("Runs every time a new reading arrives.".to_string()),
        "OnStartup" => Some("Runs once when the daemon starts.".to_string()),
        "Service" => Some("Runs continuously.".to_string()),
        // "no sentence — it is a button"
        "OnDemand" => None,
        _ => None,
    }
}

/// "every minute", "every 10 minutes", "every 90 seconds" — a period as an operator says it.
fn every(p: Duration) -> String {
    let s = p.as_secs();
    match s {
        0 => "continuously".to_string(),
        1 => "every second".to_string(),
        60 => "every minute".to_string(),
        3600 => "every hour".to_string(),
        s if s % 3600 == 0 => format!("every {} hours", s / 3600),
        s if s % 60 == 0 => format!("every {} minutes", s / 60),
        s => format!("every {s} seconds"),
    }
}

/// §0.4's counter sentences, each shown only when its own condition holds.
///
/// The conditions are the table's "Shown when" column and are applied here rather than in
/// the client, so that "shown when n > 2" cannot become "shown when n > 0" in one screen and
/// not another.
pub fn counter_notes(status: &ComponentStatus) -> Vec<String> {
    let c = &status.counters;
    let mut out = Vec::new();
    if c.skipped_total > 0 {
        out.push(format!(
            "{} runs skipped because the previous one was still going.",
            c.skipped_total
        ));
    }
    if c.deadline_exceeded_total > 0 {
        out.push(format!("{} runs gave up early.", c.deadline_exceeded_total));
    }
    if c.restart_total > 2 {
        out.push(format!(
            "Restarted {} times since the daemon started.",
            c.restart_total
        ));
    }
    if c.misfired_total > 0 {
        out.push(format!(
            "{} scheduled runs were missed while the daemon was not running.",
            c.misfired_total
        ));
    }
    if c.stop_timeout_total > 0 {
        out.push(format!(
            "{} items had to be forced to stop.",
            c.stop_timeout_total
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{ListenerView, PairingView, ServiceView, StationView, TransportView};

    fn station(key: &str, name: &str) -> StationView {
        StationView {
            key: key.into(),
            id: 1,
            name: name.into(),
            kind: "davis".into(),
            kind_display: "Your Davis station".into(),
            collecting: true,
            identity_kind: "fixed".into(),
            presented_key: None,
            protocol_secret: crate::view::Credential::NONE,
            latitude: None,
            longitude: None,
            altitude_m: None,
            timezone: None,
            aprs_callsign: None,
            wind_dir_correction_degrees: 0,
            snow_base_distance_mm: None,
            transport: TransportView::default(),
            sensor_mapping: None,
            poll_interval_seconds: None,
            shared_with: Vec::new(),
        }
    }

    fn cfg() -> ConfigView {
        ConfigView {
            generation: 42,
            node: None,
            listener: ListenerView {
                listen_addr: "0.0.0.0".into(),
                http_port: 8080,
                https_port: None,
                grpc_port: 50051,
                management_listen_addr: "127.0.0.1".into(),
                management_bound_addr: "127.0.0.1".into(),
                management_port: 8081,
                management_tls_cert_path: None,
                management_tls_key_path: None,
            },
            stations: vec![station("backyard", "Backyard")],
            websites: Vec::new(),
            storage: None,
            services: vec![ServiceView {
                service: "wunderground".into(),
                display: "Weather Underground".into(),
                name: "backyard".into(),
                enabled: true,
                interval_seconds: None,
                api_endpoint: None,
                api_key: crate::view::Credential::NONE,
                passcode: crate::view::Credential::NONE,
                aprs_symbol_table: None,
                aprs_symbol_code: None,
                aprs_comment: None,
                aprs_server: None,
                aprs_transport: None,
                aprs_kiss_serial_device: None,
                aprs_kiss_serial_baud: None,
                aprs_kiss_tcp_address: None,
                aprs_kiss_path: None,
                aprs_kiss_destination: None,
                stations: vec![PairingView {
                    station_key: "backyard".into(),
                    remote_station_id: Some("KSTATION1".into()),
                }],
            }],
            forecasts: None,
        }
    }

    fn id(s: &str) -> ComponentId {
        ComponentId::new(s).expect("a literal id")
    }

    /// §2.3's worked example, exactly: `wu:backyard` renders as "Weather Underground" /
    /// "Backyard" / `share` / `/share/weather-underground`. The qualifier is the
    /// **station's name**, which is the whole reason this takes the configuration.
    #[test]
    fn the_design_s_worked_example_renders_as_the_design_writes_it() {
        let v = Vocabulary::new(&cfg());
        let d = v.describe(&id("wu:backyard"));
        assert_eq!(d.name, "Weather Underground");
        assert_eq!(d.qualifier.as_deref(), Some("Backyard"));
        assert_eq!(d.section, Section::Share);
        assert_eq!(d.href, "/share/weather-underground");
        assert_eq!(d.one_line(), "Weather Underground (Backyard)");
    }

    /// **No internal word reaches a rendering.** The table's left column is what must never
    /// be shown, so the assertion is over the output rather than over a mapping: no
    /// rendering of any id this program mints may contain `controller`, `component`,
    /// `capability`, `envelope`, `spool` or `trigger`.
    #[test]
    fn no_rendering_contains_an_internal_word() {
        let v = Vocabulary::new(&cfg());
        let forbidden = [
            "controller",
            "component",
            "capability",
            "envelope",
            "spool",
            "trigger",
            "sink",
            "timescale",
        ];
        for raw in [
            "api:legacy",
            "http:listener",
            "web:site:1",
            "db:postgres",
            "writer:timescale",
            "window:rollup",
            "rain:rollover:backyard",
            "snapshot:latest",
            "snow:depth",
            "aeris:forecast:backyard:240",
            "wu:backyard",
            "pws:backyard",
            "aprs:backyard",
            "station:davis:backyard",
            "station:snowgauge:summit",
            "station:ambient",
            "station:grpcrx",
            "station:wll:udp:22222",
            "mgmt:api",
        ] {
            let d = v.describe(&id(raw));
            let rendered = d.one_line().to_lowercase();
            for word in forbidden {
                assert!(
                    !rendered.contains(word),
                    "`{raw}` rendered as `{rendered}`, which contains `{word}`"
                );
            }
            assert!(!d.name.is_empty(), "`{raw}` rendered no name");
        }
    }

    /// `station:wll:udp:<port>` is one listener for every unit on the segment, so it has no
    /// station — and must not render the **port** as the station's name, which is what a
    /// naive "last segment is the key" rule does.
    #[test]
    fn the_broadcast_listener_has_no_station_to_qualify_with() {
        let v = Vocabulary::new(&cfg());
        let d = v.describe(&id("station:wll:udp:22222"));
        assert_eq!(d.qualifier, None);
        assert_eq!(d.href, "/stations");
    }

    /// An id from a component family that does not exist yet still renders, because a blank
    /// beside `Running` is worse than a technical name in the Advanced view.
    #[test]
    fn an_unknown_family_still_renders() {
        let v = Vocabulary::new(&cfg());
        let d = v.describe(&id("check:aprs"));
        assert_eq!(d.name, "check");
        assert_eq!(d.section, Section::System);
    }

    /// §0.4: `Failed` is badged **Stopped** and `Stopped` is badged **Off** — the badge is
    /// not the state's own word, and this is the pair most likely to be "corrected" later.
    #[test]
    fn a_failure_is_badged_stopped_and_a_stop_is_badged_off() {
        let now = chrono::Utc::now();
        assert_eq!(
            badge_for(&State::Failed {
                error: "x".into(),
                at: now,
                attempt: 1
            })
            .label,
            "Stopped"
        );
        assert_eq!(badge_for(&State::Stopped).label, "Off");
        assert_eq!(badge_for(&State::Stopped).tone, "neutral");
    }

    /// The sentence names the **provider**, not the capability: that is what W2a §3.1 put
    /// the provider on `WaitingOn` for.
    #[test]
    fn waiting_names_the_thing_rather_than_the_capability() {
        let v = Vocabulary::new(&cfg());
        let on = vec![WaitingOn {
            capability: Capability::ReadingStore,
            provider: Some(id("db:postgres")),
        }];
        let s = sentence_for(&State::Waiting { on }, &v, chrono::Utc::now());
        assert_eq!(s, "Can't start yet — waiting for the weather database.");
        assert!(!s.contains("ReadingStore"));
    }

    /// Two providers read as a sentence rather than as a list of two: §0.4's "waiting for
    /// the weather database and snow calculations".
    #[test]
    fn two_things_are_joined_with_and() {
        let v = Vocabulary::new(&cfg());
        let on = vec![
            WaitingOn {
                capability: Capability::ReadingStore,
                provider: None,
            },
            WaitingOn {
                capability: Capability::SnowCache,
                provider: None,
            },
        ];
        let s = sentence_for(&State::Waiting { on }, &v, chrono::Utc::now());
        assert_eq!(
            s,
            "Can't start yet — waiting for the weather database and snow calculations."
        );
    }

    /// `OnDemand` has no schedule sentence: it is a button, and a sentence would be a
    /// promise that it runs by itself.
    #[test]
    fn on_demand_gets_no_schedule_sentence() {
        assert_eq!(schedule_sentence("OnDemand", None), None);
        assert_eq!(
            schedule_sentence("Every", Some(Duration::from_secs(600))),
            Some("Runs every 10 minutes.".to_string())
        );
        assert_eq!(
            schedule_sentence("Every", Some(Duration::from_secs(90))),
            Some("Runs every 90 seconds.".to_string())
        );
        assert_eq!(
            schedule_sentence("Service", None),
            Some("Runs continuously.".to_string())
        );
    }
}
