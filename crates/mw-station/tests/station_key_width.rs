//! [`mw_station::MAX_KEY_LEN`] — the widest station key every driver can still name a
//! component for, checked against the mint functions rather than against the arithmetic.
//!
//! The number also lives in SQL. `w9.station_key_problem`, emitted by
//! `xtask/src/legacy.rs` into the migration's staging schema, refuses a migrated station
//! id longer than this, because a station id *becomes* a station key and a migration that
//! minted one the plan then rejects is M5-1b: three stations imported under ids
//! (`CSI`, `Suncrest Air Quality`) that no configuration could address.
//!
//! So the bound is pinned from both ends for the wrappers this crate owns: a new driver
//! here with a prefix longer than `station:airgradient:` fails the first test below, which
//! is the only warning the migration's copy of the limit can get. It does NOT cover
//! `mw_output::aeris`'s `aeris:forecast:<key>:<span_hours>`, which is one byte less tight
//! and which `ci/layers.toml` keeps out of reach of this crate — see `MAX_KEY_LEN`.

use mw_station::MAX_KEY_LEN;

/// Every component id a station key is embedded in, as the plan mints them.
fn ids(key: &str) -> Vec<(&'static str, bool)> {
    vec![
        ("campbell", mw_station::campbell::station_id(key).is_ok()),
        ("davis", mw_station::davis::station_id(key).is_ok()),
        (
            "airgradient",
            mw_station::airgradient::station_id(key).is_ok(),
        ),
        ("snowgauge", mw_station::snowgauge::station_id(key).is_ok()),
        ("wll poll", mw_station::wll::poll_id(key).is_ok()),
        ("wll rearm", mw_station::wll::rearm_id(key).is_ok()),
    ]
}

#[test]
fn a_key_of_the_declared_width_names_a_component_for_every_driver() {
    let key = "k".repeat(MAX_KEY_LEN);
    let refused: Vec<&str> = ids(&key)
        .into_iter()
        .filter(|(_, ok)| !ok)
        .map(|(d, _)| d)
        .collect();
    assert!(
        refused.is_empty(),
        "MAX_KEY_LEN is {MAX_KEY_LEN} and these drivers cannot use a key that long: \
         {refused:?} — lower it here and in w9.station_key_problem"
    );
}

#[test]
fn one_byte_more_is_refused_by_at_least_one_driver() {
    // The bound has to be tight as well as safe: a limit lower than necessary would make
    // the migration refuse a station id the daemon would have accepted, and this is the
    // half of that which a reader of either copy cannot check by inspection.
    let key = "k".repeat(MAX_KEY_LEN + 1);
    let accepted_by_all = ids(&key).into_iter().all(|(_, ok)| ok);
    assert!(
        !accepted_by_all,
        "every driver accepts a key of {} bytes, so MAX_KEY_LEN is lower than it needs \
         to be",
        MAX_KEY_LEN + 1
    );
}

#[test]
fn the_characters_the_migration_derives_are_characters_a_key_may_contain() {
    // w9.station_key() emits lower-case ASCII, digits, `-`, `_` and `.` and nothing else.
    // This is the set, as one key, through the longest wrapper.
    let key = "abcdefghijklmnopqrstuvwxyz0123456789-_.";
    assert!(mw_station::airgradient::station_id(key).is_ok());
    // And the three shapes it exists to prevent, for contrast.
    for bad in ["CSI", "Suncrest Air Quality", ""] {
        assert!(
            mw_station::airgradient::station_id(bad).is_err(),
            "{bad:?} should not name a component"
        );
    }
}
