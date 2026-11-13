//! The M5-2 diff harness: the Go daemon is the oracle, and every difference is predicted.
//!
//! `docs/implementation-plan.md` §5.0 describes three mechanisms for running the two
//! implementations side by side. (a) is the conformance suite, which has run since M1 and is
//! deliberately value-blind -- it diffs the *contract*, and values change every two seconds.
//! This crate is (b) and (c), which is where a wrong *number* shows up.
//!
//! | | What it diffs | Module |
//! |---|---|---|
//! | **(b)** | same station, both daemons, diffed **readings** | [`readings`] |
//! | **(c)** | same database, both read paths, diffed **responses** | [`responses`] |
//!
//! # Why the expected-diff set is a file and not a list of `if`s
//!
//! M5-2's own wording: "with its expected-diff set encoded as data rather than prose, so an
//! unexpected diff is a failure rather than a judgement call." [`expected`] reads
//! `expected.toml`, and the only two outcomes for a difference are "matched a rule, which
//! cites a decision" and "unexpected, which is red". There is no third outcome and no
//! threshold to argue about at the end of a run.
//!
//! The rules are predicates, not key lists. `float4_accumulated` is an arithmetic bound that
//! fails if the two sides differ by more than the Go accumulator's own worst case;
//! `mean_to_total` asserts the new value is the old one multiplied by the bucket's reading
//! count. Both of them can fail on the keys they name, which is the difference between a
//! prediction and an exemption.
//!
//! # Why every input is pluggable
//!
//! W1 **D-10**: a differ that cannot be made to differ proves nothing. Each side of each
//! mechanism is a [`source::Source`] that is either a live system or a recorded directory,
//! so the negative control -- inject a 1% error into one metric and assert the harness
//! reports it -- runs end to end through the real differ over committed inputs, with no
//! database, no network and no production. `ci/diff/self-test.sh` is that control, and it
//! asserts both halves: clean on the recorded pair, red on the injected one.

pub mod census;
pub mod clock;
pub mod expected;
pub mod inject;
pub mod numeric;
pub mod psql;
pub mod readings;
pub mod report;
pub mod responses;
pub mod source;
pub mod value;
