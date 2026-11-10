//! **Two implementations of Go's float digits, asserted to agree.**
//!
//! `encoding/json` formats a float with `strconv.AppendFloat(b, f, fmt, -1, bits)`, which is
//! ryu and breaks a shortest-representation tie **round-half-to-even**; Rust's `Display` is
//! Grisu/Dragon and breaks the same tie **round-half-up**. Two surfaces have to reproduce
//! Go's bytes:
//!
//! | Crate | Surface | What a wrong digit costs |
//! |---|---|---|
//! | `mw_api_legacy::dto` | the frozen 133-key DTO | a byte a deployed iOS client already has |
//! | `mw_web::gojs` | the rendered page's `radar-config` block | the byte diff against W1's recording |
//!
//! Both are in the `surface` layer, so neither may name the other (`ci/layers.toml`) -- and
//! the binary is the one crate that may name both (§1.3 rule 1), which is why this test is
//! here and not in either of them.
//!
//! `ci/layers.toml`'s `go-number-digits` rule is the other half: it names exactly these two
//! crates as the ones that may depend on `ryu`, and its reason points at this file.
//!
//! **What this does not assert** is that either implementation matches Go. That is each
//! crate's own job and each one does it against output produced by running Go:
//! `mw_api_legacy::dto::tests::the_digits_are_gos_digits_over_every_f32_bit_pattern_in_a_sweep`
//! and `crates/mw-web/tests/escaping.rs`. What is left is drift between the two, which no
//! single-crate test can see.

use mw_api_legacy::dto::go_float64;
use mw_web::gojs::json_f64;

/// A deterministic 64-bit sweep: xorshift64*, which is three shifts and a multiply and
/// needs no dependency.
///
/// A fixed seed rather than a random one, for the reason every property test in this
/// workspace uses one: a failure that cannot be reproduced is a failure nobody fixes.
fn sweep(seed: u64, count: usize) -> impl Iterator<Item = u64> {
    let mut x = seed;
    std::iter::from_fn(move || {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        Some(x.wrapping_mul(0x2545_F491_4F6C_DD1D))
    })
    .take(count)
}

#[test]
fn the_two_go_float_implementations_agree_over_a_bit_pattern_sweep() {
    let mut finite = 0usize;
    for bits in sweep(0x4D57_3433_5F47_4F4E, 200_000) {
        let value = f64::from_bits(bits);
        if !value.is_finite() {
            // Neither surface can be reached with one: `zf` maps it to zero before the DTO
            // is built (**DEV-11**), and a latitude is read from a `REAL` column. The two
            // crates answer differently on purpose -- `go_float64` is documented as
            // unreachable for one, `json_f64` renders `0` rather than panicking -- so
            // comparing them here would assert a case neither serves.
            continue;
        }
        assert_eq!(
            go_float64(value),
            json_f64(value),
            "{value:?} (bits {bits:#018x}): the frozen surface and the website disagree \
             about Go's digits"
        );
        finite += 1;
    }
    assert!(
        finite > 150_000,
        "only {finite} finite patterns were compared; the sweep is not exercising anything"
    );
}

/// The sweep above is dominated by enormous and tiny magnitudes, which exercise the
/// exponent path and almost never the `'f'` one. These are the values a configuration
/// actually holds, plus the three the `dto` module documents as the tie cases where
/// `Display` and `ryu` differ.
#[test]
fn the_two_agree_on_the_values_a_configuration_holds() {
    let mut values: Vec<f64> = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        39.9,
        -105.5,
        39.900_000_000_000_006,
        180.0,
        -180.0,
        90.0,
        -90.0,
        // The three `mw_api_legacy::dto` names: `Display` writes a different last digit.
        46.632_812_5,
        -41.382_812_5,
        2_956_007.25,
        // Go's two format thresholds, from both sides.
        1e-7,
        1e-6,
        1e21,
        9.999_999_999_999_999e20,
        f64::MIN_POSITIVE,
        f64::MAX,
        f64::EPSILON,
    ];
    // Every latitude and longitude at a tenth of a degree, which is the grid an operator
    // types coordinates on.
    for tenths in -1800..=1800 {
        values.push(f64::from(tenths) / 10.0);
    }
    for value in values {
        assert_eq!(
            go_float64(value),
            json_f64(value),
            "{value:?}: the frozen surface and the website disagree about Go's digits"
        );
    }
}

/// **The negative control.** The comparison above is only worth running if the two
/// implementations *can* disagree with `Display` -- so this asserts that neither of them
/// **is** `Display`, which is what they would both have to be for the test to be vacuous.
///
/// The property used here is Go's **format choice**, not its tie rule. `Display` never uses
/// an exponent at any magnitude and Go uses one outside `[1e-6, 1e21)`, which is reachable
/// from an `f64` literal. The tie rule -- ryu's round-half-to-even against `Display`'s
/// round-half-up -- is an `f32` property at the three values `mw_api_legacy::dto` documents
/// (`46.632812` against `46.632813`), and that crate's own sweep against Go pins it; the
/// `f64` spelling of those literals is exactly representable and therefore not a tie.
#[test]
fn neither_implementation_is_rusts_display() {
    for (value, go) in [(1e21_f64, "1e+21"), (1e-7_f64, "1e-7")] {
        assert_eq!(go_float64(value), go);
        assert_eq!(json_f64(value), go);
        assert_ne!(
            format!("{value}"),
            go,
            "`Display` would have been accepted by both sides of this file"
        );
    }
    // And inside the interval, where Go uses `'f'`, both agree with each other and with
    // `Display` -- which is why the exponent is the case that distinguishes them.
    assert_eq!(go_float64(39.9), "39.9");
    assert_eq!(json_f64(39.9), "39.9");
}
