//! `ConfigAdmin` — the one way configuration changes at run time, as the trait **this crate
//! needs from the binary**.
//!
//! # Why the trait is declared here and implemented there
//!
//! `ci/layers.toml` forbids this crate from naming `mw-store` ("it uses `ConfigAdmin`") and
//! `mw-pipeline`, and `mw-supervisor` may not name the configuration type at all because
//! `plan()` is generic over it. So a committed write needs three things that live in three
//! crates — SQL, `plan()` and the `Config` type — and the one place all three may be named
//! is the binary (docs/service-architecture.md §1.3 rule 1). `mw_http::ListenerConfig` and
//! `mw_station::StationConfig` are the same shape for the same reason: the consumer declares
//! the trait, the binary implements it.
//!
//! # There is no `reload`
//!
//! W2a §2.5: a write through this trait bumps the configuration generation, the binary
//! re-plans, and the supervisor applies the **plan diff**. `AppReloader::ReloadConfiguration`
//! has no replacement because nothing calls it, and §1.10 drops `POST /api/config/reload`
//! for the same reason — "keeping a Reload button would re-teach the operator that saving
//! does not take effect, which is precisely the lie §2 exists to remove."
//!
//! # Preview and save are one call with one flag
//!
//! [`ConfigAdmin::apply`] takes [`Commit`], and that is the only difference between
//! `POST /api/v1/config/preview` and a `PUT`. The transaction, the re-read, `plan()` and the
//! diff are identical; a preview rolls back and a save commits. That is what
//! implementation-plan.md **M7-1**'s done-criterion is stated over.

use mw_component::{BoxFuture, Generation};

use crate::change::Change;
use crate::error::{ApiError, FieldError};
use crate::view::{ConfigView, Named};

/// Whether a change is being tried or made.
///
/// A two-variant enum rather than a `bool`, because `apply(&change, true)` at a call site
/// says nothing and this is the flag that decides whether the operator's configuration
/// changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Commit {
    /// Apply it, read the result back, plan it, and **roll back**.
    Preview,
    /// Apply it, read the result back, plan it, and **commit** — then hand the plan to the
    /// supervisor.
    Save,
}

impl Commit {
    /// Whether this is the real thing.
    pub const fn is_save(self) -> bool {
        matches!(self, Commit::Save)
    }
}

/// What a change does to the running component set.
///
/// §2.4's `diff`, with one difference in naming that is worth being explicit about:
/// `PlanDiff`'s words are `added`/`removed`/`changed`, and this resource's are
/// `start`/`stop`/`restart`, because those are what the operator is being asked to agree to.
/// `unchanged` is a **count** rather than a list for the same reason — twenty-seven things
/// not happening is one fact.
#[derive(
    Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
pub struct PlanOutline {
    /// Things that do not exist yet and will.
    pub start: Vec<Named>,
    /// Things that exist and will not.
    pub stop: Vec<Named>,
    /// Things whose configuration changed, which are stopped and started again.
    pub restart: Vec<Named>,
    /// How many are left exactly as they are. **Not touched** — which is what makes "saving
    /// a website no longer disturbs the APRS schedule" observable rather than asserted.
    pub unchanged: usize,
}

impl PlanOutline {
    /// Whether this change does nothing to the running set.
    pub fn is_empty(&self) -> bool {
        self.start.is_empty() && self.stop.is_empty() && self.restart.is_empty()
    }

    /// The ids this change restarts, sorted — the list M7-1's done-criterion compares
    /// against what actually restarted.
    pub fn restart_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.restart.iter().map(|n| n.id.clone()).collect();
        v.sort();
        v
    }
}

/// What a preview or a save answers with.
///
/// One type for both, because the client renders the same three things either way: whether
/// it is acceptable, what it does, and what it is worth knowing. §2.4's `{ "valid": true,
/// "diff": {...}, "warnings": [...] }` and §2.2's `{ "generation": 42, "affects": [...] }`
/// are the same object seen twice.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Applied {
    /// Whether the configuration is acceptable. Always `true` on a `200`; a refusal is a
    /// `422` with [`crate::error::ErrorBody`], which carries the field paths.
    pub valid: bool,
    /// The configuration generation. On a save this is the **new** one; on a preview it is
    /// the generation the preview was taken against, so a client can tell that the ground
    /// moved under it.
    pub generation: u64,
    /// Whether the configuration was committed, or only tried.
    pub committed: bool,
    /// What it does to the running component set.
    pub diff: PlanOutline,
    /// The ids a client watches `applied_generation` on to decide when the change is live
    /// (§2.2). The union of `start` and `restart`: a stopped component never reports an
    /// applied generation again, so waiting for one would wait forever.
    pub affects: Vec<String>,
    /// Consequences that do not block (§8.5).
    pub warnings: Vec<String>,
}

impl Applied {
    /// Assemble the response from a diff.
    pub fn new(
        generation: Generation,
        commit: Commit,
        diff: PlanOutline,
        warnings: Vec<String>,
    ) -> Self {
        let mut affects: Vec<String> = diff
            .start
            .iter()
            .chain(diff.restart.iter())
            .map(|n| n.id.clone())
            .collect();
        affects.sort();
        Self {
            valid: true,
            generation: generation.get(),
            committed: commit.is_save(),
            diff,
            affects,
            warnings,
        }
    }
}

/// What the newest committed change is still waiting for.
///
/// **The set matters as much as the integer**, and getting that wrong is the one way W5
/// §2.4's `ApplyBanner` becomes a spinner that never stops. §2.2 states the derived fact as
/// "a change is live when every **affected** component's `applied_generation >= generation`"
/// -- affected, not every component. An unchanged component is *deliberately not touched* by
/// a plan diff (W2a Decision 25), so it keeps the generation it was built at and is not
/// waiting for anything; counting it as pending would mean every save after the first one
/// reported progress for ever, because the APRS beacon a website edit did not disturb would
/// never report the website's generation.
///
/// Empty before the first write of this process, which is the honest answer: nothing has
/// been asked for, so nothing is outstanding.
/// **Not on the wire**, which is why it carries no `ToSchema`: the client is told what to
/// watch by `Applied::affects` when it saves, and by `ComponentView::pending` and
/// `SummaryView::pending` when it reads -- so a third representation of the same fact would
/// be a schema entry no operation returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Awaiting {
    /// The generation the components below are expected to reach.
    pub generation: u64,
    /// The ids the newest committed change starts or restarts, sorted.
    pub ids: Vec<String>,
}

impl Awaiting {
    /// Whether this component is one of the ones still being waited for.
    pub fn wants(&self, id: &str) -> bool {
        self.ids
            .binary_search_by(|probe| probe.as_str().cmp(id))
            .is_ok()
    }
}

/// Validated configuration mutation, and the read every resource is a projection of.
///
/// Exactly one component requires it (`mw_component::Capability::ConfigAdmin`'s own words),
/// and this crate is that component.
pub trait ConfigAdmin: Send + Sync + 'static {
    /// The whole configuration, with every secret omitted (§1.10, **D-67**).
    ///
    /// One read rather than one per resource: W5 §2.2's generation has to be the same
    /// integer on everything a screen shows at once, and five independent reads could
    /// straddle a write and report two.
    fn read(&self) -> BoxFuture<'_, Result<ConfigView, ApiError>>;

    /// The newest committed generation (**D-53**), without a read.
    fn generation(&self) -> Generation;

    /// What the newest committed change is still waiting for (§2.2, §2.4).
    fn awaiting(&self) -> Awaiting;

    /// Try or make one change.
    ///
    /// The whole of it, in order: open a transaction, apply the change's statements, read the
    /// configuration **back out of that transaction**, run `plan()` on it, diff against the
    /// live plan, then commit or roll back per [`Commit`]. A `plan()` refusal is an
    /// [`ApiError::Invalid`] with the field paths §7.2 requires, and nothing has been stopped
    /// when it is returned — which is the property that makes a dry run free (W2a §2.5).
    fn apply(&self, change: Change, commit: Commit) -> BoxFuture<'_, Result<Applied, ApiError>>;

    /// Put the previous committed configuration back — §2.4's **Undo save**.
    ///
    /// One step, and only the step this process committed: "because every write has a
    /// generation, the server keeps the previous committed configuration for the current
    /// session". Offered only on the failure banner. Full configuration history is
    /// **OQ-W5-5** and is deliberately not this.
    fn revert(&self, to_generation: u64) -> BoxFuture<'_, Result<Applied, ApiError>>;
}

/// Turn a `plan()` refusal into the field it belongs on.
///
/// §7.2: "`plan()` errors are validation errors", and `PlanError::RouteConflict { scope,
/// pattern, a, b }` is rendered on the field that caused it. The mapping is by the error's
/// rendered text rather than by its variant, because `PlanError` is `mw-supervisor`'s and
/// this crate may not name it — so the binary hands the rendered chain over and this decides
/// where it lands.
///
/// Unmatched refusals land on the whole form rather than on a guess: an error attached to the
/// wrong input is worse than one attached to none, because the operator edits the wrong field
/// and the message does not change.
pub fn plan_refusal_as_field_errors(rendered: &str) -> Vec<FieldError> {
    let lower = rendered.to_lowercase();
    // A duplicate route is two websites claiming one hostname, which is the hostname field.
    if lower.contains("route") && (lower.contains("conflict") || lower.contains("already")) {
        return vec![FieldError::new(
            "hostname",
            "Another website already uses that hostname.",
        )];
    }
    if lower.contains("time zone") || lower.contains("timezone") || lower.contains("zone") {
        return vec![FieldError::new(
            "timezone",
            "Enter an IANA time-zone name, such as America/Denver.",
        )];
    }
    if lower.contains("uploader") && lower.contains("arity") {
        return vec![FieldError::new(
            "stations",
            "This service uploads for one station. Add a second account to share another.",
        )];
    }
    if lower.contains("duplicate") && lower.contains("id") {
        return vec![FieldError::new(
            "name",
            "Something else already uses that name.",
        )];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::Section;

    fn named(id: &str) -> Named {
        Named {
            id: id.into(),
            display: id.into(),
            section: Section::System,
            href: "/system".into(),
        }
    }

    /// `affects` is what a client watches to decide a change is live (§2.2), so a **stopped**
    /// component must not be in it: a component that is gone never reports an applied
    /// generation again, and the banner would spin forever.
    #[test]
    fn a_stopped_component_is_not_something_to_wait_for() {
        let diff = PlanOutline {
            start: vec![named("wu:backyard")],
            stop: vec![named("pws:backyard")],
            restart: vec![named("api:legacy")],
            unchanged: 27,
        };
        let a = Applied::new(Generation::new(42), Commit::Save, diff, Vec::new());
        assert_eq!(a.affects, ["api:legacy", "wu:backyard"]);
        assert!(a.committed);
        assert_eq!(a.generation, 42);
    }

    /// A preview says so, so that a client cannot render "Saved." off the back of one.
    #[test]
    fn a_preview_reports_that_it_committed_nothing() {
        let a = Applied::new(
            Generation::new(7),
            Commit::Preview,
            PlanOutline::default(),
            Vec::new(),
        );
        assert!(!a.committed);
        assert!(a.valid);
        assert!(a.diff.is_empty());
    }

    /// A route conflict is the hostname field, because that is the input the operator
    /// changed — not a banner, which §8.5 rules out.
    #[test]
    fn a_route_conflict_lands_on_the_hostname() {
        let e = plan_refusal_as_field_errors(
            "two components claim route `/` on site scope: `web:site:1` and `web:site:2`; \
             a duplicate path conflict",
        );
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].field, "hostname");
    }

    /// An unrecognised refusal gets **no** field, rather than a guessed one: an error on the
    /// wrong input is worse than one on none, because editing that input does not clear it.
    #[test]
    fn an_unrecognised_refusal_claims_no_field() {
        assert!(plan_refusal_as_field_errors("the spool is not open").is_empty());
    }
}
