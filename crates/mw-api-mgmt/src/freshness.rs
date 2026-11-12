//! `Freshness`, rendered — "Forecast last updated 3 hours ago." / "Forecast has never been
//! fetched."
//!
//! **W2a Decision 20**, and the second half of implementation-plan.md **M6-3**'s
//! done-criterion. The decision is one sentence and the whole of this module is a
//! consequence of it:
//!
//! > *Capability availability is a lifecycle concern; data presence is a response concern.
//! > `/forecast` depending on Aeris having fetched is a **freshness** declaration consumed
//! > by W5 and `/test/*`, **never by the scheduler**.*
//!
//! # Why this is a separate rendering from a component's state, and not a badge on it
//!
//! `docs/management-ui-design.md` §0.4 renders every W2a lifecycle state as a sentence —
//! "Running.", "Retrying in 30 seconds.", "Stopped because of an error: …" — and then says,
//! in its own section and not as a footnote:
//!
//! > *Freshness (W2a Decision 20) and staleness (W3 §2a.6) are rendered **separately from
//! > state**, because an available-but-stale thing is not a failure.*
//!
//! The pair that makes that concrete is the pair this module exists for: a forecast
//! component that is **`Running`** and whose last successful fetch was three hours ago. Its
//! state sentence is "Running. Checks every 6 hours." and there is nothing wrong with it;
//! what an operator needs to know is that the *forecast* is three hours old. Conflating the
//! two gives one of two wrong answers — a green badge beside a stale panel, or a red one
//! beside a component that is working.
//!
//! So [`FreshnessView`] carries **no state**, by construction. It cannot be rendered as a
//! badge colour derived from a lifecycle state, because it does not have one; and
//! [`render`] takes the declaration and an observed instant and nothing else. W5 §1544's
//! "last forecast and last alert fetch times (`Freshness`, §0.4)" is this type.
//!
//! # The observed instant is **data**, not the component's `last_run`
//!
//! The one thing that could quietly turn this back into a state rendering is reading the
//! component's own last-run time instead of the data's. They differ exactly when it matters:
//! a component that has ticked four times and failed every time has a recent `last_run` and
//! a forecast from yesterday. So the instant is read from the **store** — `forecasts.updated_at`
//! for `ForecastStore` and `alerts.updated_at` for `AlertStore` — which is why [`render`]
//! takes an `Option<DateTime<Utc>>` rather than a `ComponentStatus`.
//!
//! # The vocabulary rule, which is **D-36**
//!
//! `mw-component`'s internal names must not reach a screen: *"Component `aeris:backyard` is
//! Waiting on Capability ForecastStore" tells an operator nothing; W5 §0 owns the
//! translation.* This is that translation for the freshness half, so [`Subject`] maps a
//! `Capability` to an English noun and the rendered sentence contains neither a component id
//! nor a capability name. A unit test asserts that, because it is the kind of rule that is
//! true when written and false two refactors later.

use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_component::{Capability, Freshness};

/// What a freshness declaration is **about**, in words an operator reads.
///
/// The translation **D-36** requires: `Capability::ForecastStore` is an internal name and
/// "Forecast" is what it is. A closed mapping rather than a `Display`, because a capability
/// whose contents nobody expects to be recent has no sentence — see [`Subject::of`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subject {
    /// The noun, capitalised, as a sentence starts with it: "Forecast", "Weather alerts".
    noun: &'static str,
    /// The verb, for the never case: "fetched", "calculated".
    verb: &'static str,
    /// Whether the noun is plural, so the never sentence agrees with it.
    ///
    /// "Weather alerts **has** never been fetched" is the detail that makes a page look
    /// machine-generated, and §0.5's standard for every string on this surface is that it
    /// reads as prose to somebody who has just bought a weather station.
    plural: bool,
}

impl Subject {
    /// The subject for a capability whose *contents* a component populates.
    ///
    /// `None` for a capability no [`Freshness`] is declared over. Four of the twelve
    /// capabilities have contents that go stale and the rest are a connection, a route
    /// table or a control surface — asking when `HttpRoutes` was last updated is not a
    /// question, and answering it with a sentence this module invented would be worse than
    /// not answering.
    pub const fn of(capability: Capability) -> Option<Self> {
        match capability {
            Capability::ForecastStore => Some(Self {
                noun: "Forecast",
                verb: "fetched",
                plural: false,
            }),
            Capability::AlertStore => Some(Self {
                noun: "Weather alerts",
                verb: "fetched",
                plural: true,
            }),
            Capability::SnowCache => Some(Self {
                noun: "Snow totals",
                verb: "calculated",
                plural: true,
            }),
            Capability::LatestSnapshot => Some(Self {
                noun: "Current conditions",
                verb: "updated",
                plural: true,
            }),
            _ => None,
        }
    }

    /// The noun the sentence starts with.
    pub const fn noun(self) -> &'static str {
        self.noun
    }
}

/// One freshness declaration, rendered — and carrying no lifecycle state.
///
/// The absence of a state field is the design and not an omission; the module documentation
/// has why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshnessView {
    /// The sentence, for the panel: "Forecast last updated 3 hours ago." or "Forecast has
    /// never been fetched."
    pub sentence: String,
    /// Whether the contents are older than the declaration's `expected_max_age`.
    ///
    /// **Not a failure**, which is the whole point of rendering this separately: the thing
    /// is available and its contents are old. W5's own rule is that colour is never the only
    /// signal (§1602), so this is what selects an *emphasis* and the sentence is what says
    /// the fact.
    pub stale: bool,
    /// Whether anything has ever populated it.
    ///
    /// Distinct from `stale`, and the distinction is the one an operator acts on
    /// differently: never-fetched is usually a credential or a pairing, and stale is usually
    /// a network.
    pub never: bool,
    /// How old the contents are, for a caller that wants a number rather than a sentence —
    /// the API serialises this beside `sentence` so a browser can count up without
    /// re-parsing English.
    pub age: Option<Duration>,
}

/// Render one [`Freshness`] declaration against the instant its contents were last written.
///
/// `observed` is `None` when nothing has ever populated the capability, which is the
/// "never" case and is **not** an error: a node whose Aeris credential was entered this
/// morning has never fetched a forecast and nothing is wrong.
///
/// `now` is passed in rather than read, because a sentence that says "3 hours ago" is a
/// function of an instant and a rendering that could not be tested at a chosen one would be
/// a rendering nobody checked.
pub fn render(
    declaration: &Freshness,
    observed: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> FreshnessView {
    // A capability with no [`Subject`] is a declaration that should not have been made --
    // `plan()` has no check for it, so this is the place that notices. "Data" is a
    // deliberately dull fallback: it says the true thing without inventing a noun for a
    // capability this module has not been taught.
    let subject = Subject::of(declaration.capability).unwrap_or(Subject {
        noun: "Data",
        verb: "updated",
        plural: false,
    });
    let Some(observed) = observed else {
        return FreshnessView {
            sentence: format!(
                "{} {} never been {}.",
                subject.noun,
                if subject.plural { "have" } else { "has" },
                subject.verb
            ),
            // **Never is not stale**, and the ordering matters: a brand-new installation
            // would otherwise light up as a problem in the first minute. An operator who
            // has configured nothing is not looking at a stale forecast.
            stale: false,
            never: true,
            age: None,
        };
    };
    // A negative age is a clock that has gone backwards -- an NTP step, which W2a's
    // `clock_step_total` counts -- or a row written by a database whose clock is ahead.
    // Rendered as "just now" rather than as a negative duration, because "updated -3
    // seconds ago" is the kind of sentence that makes an operator distrust the whole page.
    let age = (now - observed).to_std().unwrap_or(Duration::ZERO);
    FreshnessView {
        sentence: format!("{} last updated {}.", subject.noun, humanise(age)),
        stale: age > declaration.expected_max_age,
        never: false,
        age: Some(age),
    }
}

/// An age as the phrase §0.4's examples use: "3 hours ago", "just now", "2 days ago".
///
/// Whole units with no decimals and no compound phrases, which is the standard §0.5 sets
/// for every string on this surface: *"written for someone who has just bought a weather
/// station"*. "3 hours ago" is what the design document's own example says; "3 h 14 min ago"
/// is a number nobody needs about a forecast that refreshes every six hours.
///
/// Singular and plural are both written, because "1 hours ago" is the detail that makes a
/// page look unfinished.
fn humanise(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        // Under a minute is "just now": the exact number of seconds since a forecast
        // refreshed is not a fact anybody acts on.
        0..=59 => "just now".to_string(),
        60..=5399 => plural(seconds / 60, "minute"),
        // From an hour and a half, round to the nearest hour rather than truncating: a
        // forecast fetched 115 minutes ago is "2 hours ago" and not "1 hour ago".
        5400..=86_399 => plural((seconds + 1800) / 3600, "hour"),
        _ => plural((seconds + 43_200) / 86_400, "day"),
    }
}

/// `1 hour ago`, `3 hours ago`.
fn plural(count: u64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{count} {unit}s ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_component::ComponentId;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_763_000_000 + seconds, 0).expect("a valid instant")
    }

    fn forecast(expected_max_age: Duration) -> Freshness {
        Freshness {
            capability: Capability::ForecastStore,
            populated_by: ComponentId::new("aeris:forecast:backyard:240").expect("an id"),
            expected_max_age,
        }
    }

    /// **The done-criterion, as one assertion.** The two sentences
    /// `docs/management-ui-design.md` §0.4 writes, from the declaration and an instant.
    #[test]
    fn the_two_sentences_are_the_design_documents_own() {
        let declaration = forecast(Duration::from_secs(12 * 3600));
        let three_hours_ago = render(&declaration, Some(at(0)), at(3 * 3600));
        assert_eq!(
            three_hours_ago.sentence,
            "Forecast last updated 3 hours ago."
        );
        let never = render(&declaration, None, at(0));
        assert_eq!(never.sentence, "Forecast has never been fetched.");
    }

    /// **And it is distinct from component state.** The type carries none, so it cannot be
    /// rendered as one: a `Running` component with a three-hour-old forecast has a correct
    /// state sentence and a stale freshness sentence, and these are two fields on a page
    /// rather than one badge.
    #[test]
    fn the_view_carries_no_lifecycle_state() {
        let view = render(
            &forecast(Duration::from_secs(60)),
            Some(at(0)),
            at(3 * 3600),
        );
        assert!(view.stale, "the contents are old");
        assert!(!view.never);
        // The assertion that keeps the separation honest: nothing about a lifecycle state
        // is reachable from this value. If a `state` field is ever added, this fails to
        // compile and whoever adds it has to read Decision 20.
        let FreshnessView {
            sentence,
            stale,
            never,
            age,
        } = view;
        assert!(!sentence.is_empty());
        assert!(stale && !never && age.is_some());
    }

    /// **D-36**: no component id and no capability name reaches the sentence.
    #[test]
    fn the_sentence_carries_no_internal_vocabulary() {
        for observed in [None, Some(at(0))] {
            let view = render(&forecast(Duration::from_secs(3600)), observed, at(7200));
            for internal in [
                "ForecastStore",
                "aeris:forecast",
                "Capability",
                "Running",
                "Waiting",
                "Freshness",
            ] {
                assert!(
                    !view.sentence.contains(internal),
                    "`{internal}` leaked into `{}`",
                    view.sentence
                );
            }
        }
    }

    /// `stale` is the declaration's own `expected_max_age` and nothing else.
    #[test]
    fn stale_is_the_declared_age_and_not_a_guess() {
        let declaration = forecast(Duration::from_secs(3600));
        assert!(!render(&declaration, Some(at(0)), at(3599)).stale);
        assert!(
            !render(&declaration, Some(at(0)), at(3600)).stale,
            "at the edge"
        );
        assert!(render(&declaration, Some(at(0)), at(3601)).stale);
    }

    /// Never-fetched is **not** stale, so a fresh installation does not open with a warning.
    #[test]
    fn never_fetched_is_not_reported_as_stale() {
        let view = render(&forecast(Duration::ZERO), None, at(0));
        assert!(view.never);
        assert!(!view.stale);
        assert_eq!(view.age, None);
    }

    #[test]
    fn the_phrases_are_whole_units_with_no_decimals() {
        assert_eq!(humanise(Duration::from_secs(0)), "just now");
        assert_eq!(humanise(Duration::from_secs(59)), "just now");
        assert_eq!(humanise(Duration::from_secs(60)), "1 minute ago");
        assert_eq!(humanise(Duration::from_secs(119)), "1 minute ago");
        assert_eq!(humanise(Duration::from_secs(120)), "2 minutes ago");
        assert_eq!(humanise(Duration::from_secs(5399)), "89 minutes ago");
        assert_eq!(
            humanise(Duration::from_secs(5400)),
            "2 hours ago",
            "rounded"
        );
        assert_eq!(humanise(Duration::from_secs(3 * 3600)), "3 hours ago");
        assert_eq!(humanise(Duration::from_secs(86_399)), "24 hours ago");
        assert_eq!(humanise(Duration::from_secs(86_400)), "1 day ago");
        assert_eq!(humanise(Duration::from_secs(3 * 86_400)), "3 days ago");
    }

    /// A clock that went backwards renders as "just now" rather than as a negative age.
    #[test]
    fn a_future_instant_renders_as_just_now() {
        let view = render(&forecast(Duration::from_secs(3600)), Some(at(600)), at(0));
        assert_eq!(view.sentence, "Forecast last updated just now.");
        assert!(!view.stale);
        assert_eq!(view.age, Some(Duration::ZERO));
    }

    /// The four capabilities whose contents go stale have a noun; the rest do not, and a
    /// declaration over one of them renders the dull fallback rather than an invented word.
    #[test]
    fn only_the_capabilities_with_contents_have_a_subject() {
        let with: Vec<Capability> = Capability::ALL
            .iter()
            .copied()
            .filter(|c| Subject::of(*c).is_some())
            .collect();
        assert_eq!(
            with,
            vec![
                Capability::ForecastStore,
                Capability::AlertStore,
                Capability::SnowCache,
                Capability::LatestSnapshot,
            ]
        );
        let odd = Freshness {
            capability: Capability::HttpRoutes,
            populated_by: ComponentId::new("http:listener").expect("an id"),
            expected_max_age: Duration::from_secs(60),
        };
        assert_eq!(
            render(&odd, Some(at(0)), at(30)).sentence,
            "Data last updated just now."
        );
    }

    /// The alerts and snow subjects, which are the other two M6-3 declares or will.
    #[test]
    fn each_subject_reads_as_the_thing_it_is() {
        let sentence = |capability| {
            render(
                &Freshness {
                    capability,
                    populated_by: ComponentId::new("x:y").expect("an id"),
                    expected_max_age: Duration::from_secs(900),
                },
                None,
                at(0),
            )
            .sentence
        };
        assert_eq!(
            sentence(Capability::AlertStore),
            "Weather alerts have never been fetched.",
            "the plural agrees"
        );
        assert_eq!(
            sentence(Capability::SnowCache),
            "Snow totals have never been calculated."
        );
    }
}
