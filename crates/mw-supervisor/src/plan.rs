//! `plan()` — the pure function, and every structural check W2a Decision 2 requires.
//!
//! ```text
//! plan(config, registrars)
//!   -> specs: Vec<ComponentSpec>                      (pure; each crate's plan.rs)
//!   -> validate arity, floors, deadlines, identities   (pure)
//!   -> build the hard-edge digraph: requires(Hard) -> provides
//!   -> toposort                                        (pure)
//!   -> Err(PlanError::Cycle { path }) on failure
//! ```
//!
//! **Cycles are detected here and are fatal here**, before any component is constructed —
//! not at startup, and not at compile time, because the graph is a function of
//! configuration. That is what makes cycle detection a unit test over fixture
//! configurations rather than a hang, and it is why every error below names the component
//! and, where there are two, both of them.
//!
//! The topological order is an **optimisation, not a correctness requirement**: because a
//! hard requirement means `Waiting` rather than failure, a component started too early
//! simply waits. A supervisor that started everything in random order would converge to
//! the same state, which is the property that makes this robust to the one case the Go
//! version cannot survive — a dependency that goes away and comes back.
//!
//! # Why `plan` is generic over the configuration type
//!
//! docs/service-architecture.md §1.3 rule 2 writes the signature as
//! `plan(&Config, &Registrars)`, and rule 2's whole point is that the supervisor does not
//! know its components. The configuration *tree* has the same problem one level down: it
//! describes stations and websites, so a crate that named its type would know that a
//! station exists — and `mw-supervisor` may not depend on `mw-station` or `mw-store` at
//! all. Being generic over `C` keeps the supervisor ignorant of both, and the binary — the
//! one place a consumer and a provider meet — chooses the type.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use mw_component::{
    Arity, BodyKind, Capability, ComponentId, ComponentSpec, Generation, PlanError, ProviderClass,
};

/// Which root capabilities this process provides.
///
/// A root is provided by the runtime spine before any component exists, so whether one is
/// present is a fact about how the binary was assembled rather than about configuration.
/// That is what makes [`PlanError::UnknownCapability`] distinct from
/// [`PlanError::Unprovided`]: the first means no configuration change can help.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootSet {
    capabilities: BTreeSet<Capability>,
}

impl RootSet {
    /// An empty root set — a process with neither a reading stream nor component control.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The roots W2a §2.1 names: the reading fan-out and the supervisor's own control
    /// surface. What `main` provides in an ordinary boot.
    pub fn full() -> Self {
        Self::new([Capability::ReadingStream, Capability::ComponentControl])
    }

    /// A root set from an explicit list. A capability that is not root-class is ignored:
    /// the runtime spine cannot provide `ReadingStore`, and claiming so here would make
    /// `Unprovided` unreachable for it.
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            capabilities: capabilities
                .into_iter()
                .filter(|c| c.provider_class() == ProviderClass::Root)
                .collect(),
        }
    }

    /// Whether the spine provides this capability.
    pub fn contains(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

/// What `plan()` needs beyond the configuration itself.
#[derive(Debug, Clone)]
pub struct PlanContext {
    /// The root capabilities this process provides.
    pub roots: RootSet,
    /// The configuration generation these specs were derived from. Every component the
    /// supervisor starts from this plan records it as `applied_generation` (**D-53**).
    pub generation: Generation,
}

impl PlanContext {
    /// A context with the ordinary root set and a generation.
    pub fn new(generation: Generation) -> Self {
        Self {
            roots: RootSet::full(),
            generation,
        }
    }

    /// Replace the root set.
    #[must_use]
    pub fn with_roots(mut self, roots: RootSet) -> Self {
        self.roots = roots;
        self
    }
}

/// One component crate's contribution to the plan.
///
/// `fn register(cfg, out)` and nothing else, which is rule 2 of
/// docs/service-architecture.md §1.3: adding a component crate never edits this one, and
/// the supervisor cannot grow a `match` arm per component kind — the shape
/// `managers/controller.go` took, down to its `if con.Type == "management" { continue }`.
pub trait Registrar<C: ?Sized> {
    /// The registrar's name, for the error message when it refuses.
    fn name(&self) -> &'static str;

    /// Append this crate's specs. Pure: no I/O, no sockets, no database.
    fn register(&self, cfg: &C, out: &mut Vec<ComponentSpec>) -> Result<(), RegisterError>;
}

/// Why a registrar could not derive specs from a configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RegisterError(String);

impl RegisterError {
    /// Refuse, with a reason an operator can act on.
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

/// A validated, ordered plan.
#[derive(Debug)]
pub struct Plan {
    generation: Generation,
    /// In topological order: every provider before its consumers.
    specs: Vec<ComponentSpec>,
    /// Start-concurrency groups. Within a rank there is no reason to serialise siblings;
    /// shutdown walks the ranks in reverse, so a provider outlives its consumers.
    ranks: Vec<Vec<ComponentId>>,
    providers: BTreeMap<Capability, Vec<ComponentId>>,
}

impl Plan {
    /// The configuration generation this plan was derived from.
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// The specs, in topological order.
    pub fn specs(&self) -> &[ComponentSpec] {
        &self.specs
    }

    /// Take the specs, for a supervisor that is about to own them.
    pub fn into_specs(self) -> Vec<ComponentSpec> {
        self.specs
    }

    /// The start-concurrency groups, lowest first.
    pub fn ranks(&self) -> &[Vec<ComponentId>] {
        &self.ranks
    }

    /// Which components provide a capability. Empty for a root.
    pub fn providers_of(&self, capability: Capability) -> &[ComponentId] {
        self.providers
            .get(&capability)
            .map_or(&[], |v| v.as_slice())
    }

    /// How many components are planned.
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// Whether the plan is empty, which an unconfigured deployment is.
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// The ids, in topological order.
    pub fn ids(&self) -> impl Iterator<Item = &ComponentId> {
        self.specs.iter().map(|s| &s.id)
    }
}

/// What changed between two plans.
///
/// **Reconfiguration is a plan diff, not a restart** (W2a Decision 25). Unchanged
/// components are not touched, so saving a website configuration no longer disturbs the
/// APRS schedule — and because `plan()` is pure, an invalid configuration is rejected
/// *before* anything is stopped. The current behaviour is the opposite: the Go REST
/// controller replaces its website list, device list, device-name map, devices-by-website
/// map and TLS configuration wholesale, as plain fields with no mutex, while in-flight
/// requests read them (**D-28**).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanDiff {
    /// In the new plan and not the old.
    pub added: Vec<ComponentId>,
    /// In the old plan and not the new.
    pub removed: Vec<ComponentId>,
    /// In both, with a different fingerprint.
    pub changed: Vec<ComponentId>,
    /// In both, identical. Not touched.
    pub unchanged: Vec<ComponentId>,
}

impl PlanDiff {
    /// Whether anything at all changed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Derive, validate and order the plan — the pure function of W2a Decision 2.
pub fn plan<C: ?Sized>(
    cfg: &C,
    registrars: &[&dyn Registrar<C>],
    ctx: &PlanContext,
) -> Result<Plan, PlanError> {
    let mut specs = Vec::new();
    for registrar in registrars {
        registrar
            .register(cfg, &mut specs)
            .map_err(|e| PlanError::Registrar {
                registrar: registrar.name(),
                reason: e.to_string(),
            })?;
    }
    validate(specs, ctx)
}

/// Validate and order specs that are already derived.
///
/// Split from [`plan`] so that a test can hand it specs directly, which is what makes
/// every structural error below a unit test rather than a fixture-configuration exercise.
///
/// **The order of the checks is deliberate**, because a spec can fail more than one and the
/// first failure is what an operator reads: per-component declarations first (they are
/// about one component and name it), then provisions, then requirements, then routes, then
/// the graph. A cycle is reported last because a plan with a bad deadline *and* a cycle has
/// two problems, and the one inside a single component is the cheaper one to fix.
pub fn validate(specs: Vec<ComponentSpec>, ctx: &PlanContext) -> Result<Plan, PlanError> {
    check_unique_ids(&specs)?;
    for spec in &specs {
        check_declarations(spec)?;
    }
    let providers = check_provisions(&specs)?;
    check_requirements(&specs, &providers, &ctx.roots)?;
    check_routes(&specs)?;
    let (specs, ranks) = order(specs, &providers)?;
    Ok(Plan {
        generation: ctx.generation,
        specs,
        ranks,
        providers,
    })
}

/// Compare two plans by id and fingerprint.
pub fn diff(old: &Plan, new: &Plan) -> PlanDiff {
    let before: BTreeMap<&ComponentId, _> =
        old.specs.iter().map(|s| (&s.id, s.fingerprint())).collect();
    let mut out = PlanDiff::default();
    for spec in &new.specs {
        match before.get(&spec.id) {
            None => out.added.push(spec.id.clone()),
            Some(fingerprint) if *fingerprint == spec.fingerprint() => {
                out.unchanged.push(spec.id.clone());
            }
            Some(_) => out.changed.push(spec.id.clone()),
        }
    }
    let after: BTreeSet<&ComponentId> = new.specs.iter().map(|s| &s.id).collect();
    for spec in &old.specs {
        if !after.contains(&spec.id) {
            out.removed.push(spec.id.clone());
        }
    }
    out
}

fn check_unique_ids(specs: &[ComponentSpec]) -> Result<(), PlanError> {
    let mut seen = BTreeSet::new();
    for spec in specs {
        if !seen.insert(&spec.id) {
            return Err(PlanError::DuplicateComponentId {
                id: spec.id.clone(),
            });
        }
    }
    Ok(())
}

/// Everything about one component that can be checked without looking at any other.
fn check_declarations(spec: &ComponentSpec) -> Result<(), PlanError> {
    use mw_component::Trigger;

    if spec.trigger.body_kind() != spec.build.kind() {
        return Err(PlanError::TriggerBodyMismatch {
            id: spec.id.clone(),
            trigger: spec.trigger.kind(),
        });
    }

    if let Some(deadline) = spec.trigger.deadline() {
        if deadline.is_zero() {
            return Err(PlanError::BadDeadline {
                id: spec.id.clone(),
                deadline,
            });
        }
    }

    match &spec.trigger {
        Trigger::Every {
            period,
            deadline,
            floor,
            ..
        } => {
            // The floor before the deadline: a component configured below its floor has a
            // period that should not exist, so reporting a deadline against it would name
            // the wrong number.
            if period < floor {
                return Err(PlanError::IntervalBelowFloor {
                    id: spec.id.clone(),
                    configured: *period,
                    floor: *floor,
                });
            }
            if deadline > period {
                return Err(PlanError::DeadlineExceedsPeriod {
                    id: spec.id.clone(),
                    deadline: *deadline,
                    period: *period,
                });
            }
        }
        Trigger::Calendar { spec: cron, tz, .. } => {
            if let Some(problem) = cron.problem() {
                return Err(PlanError::BadCalendar {
                    id: spec.id.clone(),
                    problem,
                });
            }
            if tz.zone().is_none() {
                return Err(PlanError::MissingTimeZone {
                    id: spec.id.clone(),
                    station: tz
                        .station()
                        .map_or_else(|| "the deployment default".to_string(), |s| s.to_string()),
                });
            }
        }
        Trigger::OnReading { .. }
        | Trigger::OnStartup { .. }
        | Trigger::OnDemand { .. }
        | Trigger::Service => {}
    }

    if let Some(identity) = &spec.identity {
        if let Some(problem) = identity.problem(&spec.trigger) {
            return Err(PlanError::ImpossibleIdentity {
                id: spec.id.clone(),
                problem,
            });
        }
    }

    Ok(())
}

/// Arity, root-provision and duplicate-provider checks, producing the provider index.
fn check_provisions(
    specs: &[ComponentSpec],
) -> Result<BTreeMap<Capability, Vec<ComponentId>>, PlanError> {
    let mut providers: BTreeMap<Capability, Vec<ComponentId>> = BTreeMap::new();
    for spec in specs {
        for provision in &spec.provides {
            let capability = provision.capability;
            if capability.provider_class() == ProviderClass::Root {
                return Err(PlanError::NotComponentProvidable {
                    capability,
                    provider: spec.id.clone(),
                });
            }
            if provision.arity != capability.arity() {
                return Err(PlanError::ArityMismatch {
                    capability,
                    provider: spec.id.clone(),
                    declared: provision.arity,
                    expected: capability.arity(),
                });
            }
            let entry = providers.entry(capability).or_default();
            if provision.arity == Arity::Exclusive {
                if let Some(first) = entry.first() {
                    return Err(PlanError::DuplicateExclusive {
                        capability,
                        first: first.clone(),
                        second: spec.id.clone(),
                    });
                }
            }
            entry.push(spec.id.clone());
        }
    }
    Ok(providers)
}

fn check_requirements(
    specs: &[ComponentSpec],
    providers: &BTreeMap<Capability, Vec<ComponentId>>,
    roots: &RootSet,
) -> Result<(), PlanError> {
    for spec in specs {
        // A `Trigger::OnReading` component does not declare `ReadingStream` as a need --
        // the supervisor delivers and the component holds no receiver (W2a §5) -- so the
        // requirement is implied by the trigger and checked here. Without it, an
        // OnReading component in a process with no stream would start and then never be
        // delivered anything, which is the quietest possible failure.
        if spec.trigger.body_kind() == BodyKind::OnReading
            && !roots.contains(Capability::ReadingStream)
        {
            return Err(PlanError::UnknownCapability {
                capability: Capability::ReadingStream,
                needed_by: spec.id.clone(),
            });
        }

        for need in spec.hard_needs() {
            let capability = need.capability;
            if roots.contains(capability) || providers.contains_key(&capability) {
                continue;
            }
            return Err(match capability.provider_class() {
                ProviderClass::Root => PlanError::UnknownCapability {
                    capability,
                    needed_by: spec.id.clone(),
                },
                ProviderClass::Component => PlanError::Unprovided {
                    capability,
                    needed_by: spec.id.clone(),
                },
            });
        }
    }
    Ok(())
}

/// **D-51**: the union of every component's declared paths, checked for conflicts.
fn check_routes(specs: &[ComponentSpec]) -> Result<(), PlanError> {
    use mw_component::ProvisionDetail;

    let mut claimed: Vec<(&ComponentId, &mw_component::RouteManifest)> = Vec::new();
    for spec in specs {
        for provision in &spec.provides {
            let ProvisionDetail::Routes(manifest) = &provision.detail else {
                continue;
            };
            for (owner, existing) in &claimed {
                if existing.scope != manifest.scope {
                    continue;
                }
                for a in &existing.entries {
                    for b in &manifest.entries {
                        if a.conflicts_with(b) {
                            return Err(PlanError::RouteConflict {
                                scope: manifest.scope,
                                pattern: b.pattern.clone(),
                                methods: a.methods,
                                a: (*owner).clone(),
                                b: spec.id.clone(),
                            });
                        }
                    }
                }
            }
            // Within one manifest too: a component that declares `/latest` twice would
            // otherwise register two handlers and keep whichever `axum` resolved last.
            for (i, a) in manifest.entries.iter().enumerate() {
                for b in &manifest.entries[i + 1..] {
                    if a.conflicts_with(b) {
                        return Err(PlanError::RouteConflict {
                            scope: manifest.scope,
                            pattern: b.pattern.clone(),
                            methods: a.methods,
                            a: spec.id.clone(),
                            b: spec.id.clone(),
                        });
                    }
                }
            }
            claimed.push((&spec.id, manifest));
        }
    }
    Ok(())
}

/// Kahn's algorithm over the hard edges, in plan order so the result is deterministic.
fn order(
    specs: Vec<ComponentSpec>,
    providers: &BTreeMap<Capability, Vec<ComponentId>>,
) -> Result<(Vec<ComponentSpec>, Vec<Vec<ComponentId>>), PlanError> {
    let index: BTreeMap<&ComponentId, usize> =
        specs.iter().enumerate().map(|(i, s)| (&s.id, i)).collect();

    // edges[provider] = consumers, and in_degree counts each consumer's providers.
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); specs.len()];
    let mut in_degree = vec![0usize; specs.len()];
    for (consumer, spec) in specs.iter().enumerate() {
        for need in spec.hard_needs() {
            for provider in providers
                .get(&need.capability)
                .map_or(&[][..], |v| v.as_slice())
            {
                let Some(&p) = index.get(provider) else {
                    continue;
                };
                if p == consumer {
                    // A component that provides what it requires is its own cycle, and
                    // `snapshot:latest` legitimately requires soft capabilities it does
                    // not provide -- so only a HARD self-edge is a cycle.
                    return Err(PlanError::Cycle {
                        path: vec![spec.id.clone(), spec.id.clone()],
                    });
                }
                edges[p].push(consumer);
                in_degree[consumer] += 1;
            }
        }
    }

    let mut ranks: Vec<Vec<usize>> = Vec::new();
    let mut ready: VecDeque<usize> = (0..specs.len()).filter(|i| in_degree[*i] == 0).collect();
    let mut emitted = 0usize;
    while !ready.is_empty() {
        let rank: Vec<usize> = ready.drain(..).collect();
        emitted += rank.len();
        let mut next: Vec<usize> = Vec::new();
        for &node in &rank {
            for &consumer in &edges[node] {
                in_degree[consumer] -= 1;
                if in_degree[consumer] == 0 {
                    next.push(consumer);
                }
            }
        }
        next.sort_unstable();
        ready.extend(next);
        ranks.push(rank);
    }

    if emitted != specs.len() {
        let remaining: Vec<usize> = (0..specs.len()).filter(|i| in_degree[*i] > 0).collect();
        return Err(PlanError::Cycle {
            path: find_cycle(&specs, &edges, &remaining),
        });
    }

    // Reorder the specs into topological order, and translate the ranks into ids.
    let flat: Vec<usize> = ranks.iter().flatten().copied().collect();
    let id_ranks = ranks
        .iter()
        .map(|rank| rank.iter().map(|&i| specs[i].id.clone()).collect())
        .collect();
    let mut slots: Vec<Option<ComponentSpec>> = specs.into_iter().map(Some).collect();
    let ordered = flat
        .into_iter()
        .filter_map(|i| slots[i].take())
        .collect::<Vec<_>>();
    Ok((ordered, id_ranks))
}

/// Walk the cyclic remainder to produce a path a reader can follow.
fn find_cycle(
    specs: &[ComponentSpec],
    edges: &[Vec<usize>],
    remaining: &[usize],
) -> Vec<ComponentId> {
    let in_remaining = |n: usize| remaining.contains(&n);
    for &start in remaining {
        let mut path = vec![start];
        let mut seen = BTreeSet::new();
        seen.insert(start);
        let mut node = start;
        // Follow any edge that stays inside the cyclic remainder. Every node there has an
        // incoming edge from it, so this terminates in a repeat rather than a dead end.
        while let Some(&next) = edges[node].iter().find(|&&n| in_remaining(n)) {
            path.push(next);
            if next == start {
                return path.iter().map(|&i| specs[i].id.clone()).collect();
            }
            if !seen.insert(next) {
                // Reached a cycle that `start` leads into but is not part of; report the
                // cycle itself rather than the tail that found it.
                let from = path.iter().position(|&n| n == next).unwrap_or(0);
                let mut cycle: Vec<ComponentId> =
                    path[from..].iter().map(|&i| specs[i].id.clone()).collect();
                cycle.push(specs[next].id.clone());
                return cycle;
            }
            node = next;
        }
    }
    remaining.iter().map(|&i| specs[i].id.clone()).collect()
}
