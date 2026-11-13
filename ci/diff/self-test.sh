#!/usr/bin/env bash
# The diff harness's own negative control, and the `diff-harness` gate's command.
#
# implementation-plan.md M5-2's test strategy: "inject a 1% error into one metric and assert
# the harness reports it. Same principle as W1 D-10: a differ that cannot be made to differ
# proves nothing." This is that, and it is the half of M5-2 that can run in CI: mechanisms (b)
# and (c) need two running systems, but the DIFFER needs only inputs, and `recorded/` is a
# pair of them.
#
# Three things it establishes, in this order:
#
#   1. the rule set and the predicates behind it            `cargo test`
#   2. a clean run over the recorded pair is GREEN          exit 0, both mechanisms
#   3. the same run with a 1% error is RED, and names it    exit 1, and the metric in the output
#
# Step 2 is what makes step 3 mean anything: a harness that reported red on everything would
# pass step 3 and be useless. Step 3 is what makes step 2 mean anything. Neither is a control
# on its own, which is why this is one script and not two.
#
# `recorded/` is a capture of the Go side and a DERIVED Rust side -- `recorded/build.py`
# applies exactly the deviations expected.toml predicts -- so a clean run is green by
# construction and a red one is attributable to the injection. The derivation is checked
# here too: a corpus edited by hand would make the control pass for a reason nobody chose.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

# A filter that matches no tests exits zero, and a gate that reports green while running
# nothing is docs/reconciliation.md G-1. Same rule, and the same mechanism, as
# `cargo xtask emulator-tests` and `cargo xtask db-tests`: the declared number is the gate.
UNIT_TESTS=8
CONTROL_TESTS=26

BIN="$HERE/target/debug/mw-diff"
GO_ROWS="tsv:recorded/readings-go.tsv"
RUST_ROWS="tsv:recorded/readings-rust.tsv"
# The recorded readings are six consecutive readings on 2026-11-13; the window is wide enough
# to contain them and is fixed rather than relative, so the control does not stop working
# tomorrow.
WINDOW=(--since 2026-01-01T00:00:00Z --until 2027-01-01T00:00:00Z)
CENSUS=(--census tsv:recorded/census.tsv --census-rust tsv:recorded/census-rust.tsv)

fail() { printf '\nSELF-TEST FAILED: %s\n' "$*" >&2; exit 1; }
step() { printf '\n--- %s\n' "$*"; }

# Run the harness and assert its exit code, because the exit code is what a script reads.
# `set -e` is suspended for the call: a red run is the expected outcome of half of these.
expect_exit() {
    local want="$1" what="$2"; shift 2
    local out status
    set +e
    out="$("$@" 2>&1)"; status=$?
    set -e
    printf '%s\n' "$out" > "$HERE/target/self-test-last.txt"
    [ "$status" = "$want" ] || {
        printf '%s\n' "$out" >&2
        fail "$what: exit $status, expected $want"
    }
    printf '%s\n' "$out"
}

step "the corpus is the one build.py derives"
if command -v python3 >/dev/null 2>&1; then
    python3 recorded/build.py --check \
        || fail "recorded/ does not match recorded/build.py. Regenerate and commit it"
else
    printf 'python3 is absent: the corpus derivation was NOT checked\n'
fi

step "the rule set, and the predicates behind it"
unit=$(cargo test --locked --lib 2>&1 | tee /dev/stderr | grep -c '^test .* ok$' || true)
[ "$unit" = "$UNIT_TESTS" ] || fail "$unit unit tests passed, and $UNIT_TESTS are declared"
control=$(cargo test --locked --test control 2>&1 | tee /dev/stderr | grep -c '^test .* ok$' || true)
[ "$control" = "$CONTROL_TESTS" ] \
    || fail "$control control tests passed, and $CONTROL_TESTS are declared"

cargo build --locked >/dev/null

step "(b) a clean run over the recorded pair is green"
out=$(expect_exit 0 "clean readings" "$BIN" readings \
    --go "$GO_ROWS" --rust "$RUST_ROWS" \
    --go-station CSI --rust-station csi "${WINDOW[@]}")
printf '%s\n' "$out" | grep -q 'VERDICT (b): PASS' || fail "clean readings did not report PASS"
printf '%s\n' "$out" | grep -q 'paired 6' || fail "clean readings paired nothing"

step "(c) a clean run over the recorded pair is green"
out=$(expect_exit 0 "clean responses" "$BIN" responses \
    --go dir:recorded/go --rust dir:recorded/rust --station CSI "${CENSUS[@]}")
printf '%s\n' "$out" | grep -q 'VERDICT (c): PASS' || fail "clean responses did not report PASS"
# Every rule the corpus manufactures a difference for has to have fired. A green run in which
# no rule fired would be a green run over two identical inputs, which proves nothing about
# the rules.
for rule in DEV-4 DEV-7 DEV-12; do
    printf '%s\n' "$out" | grep -q "$rule" || fail "clean responses never exercised $rule"
done

step "(b) a 1% error on the Rust side is reported"
out=$(expect_exit 1 "injected readings (rust)" "$BIN" readings \
    --go "$GO_ROWS" --rust "$RUST_ROWS" \
    --go-station CSI --rust-station csi "${WINDOW[@]}" --inject outtemp:1:rust)
printf '%s\n' "$out" | grep -q 'VERDICT (b): FAIL' || fail "the injection was not reported"
printf '%s\n' "$out" | grep -q 'outtemp' || fail "the report does not name the injected metric"
printf '%s\n' "$out" | grep -q 'value(s) moved' || fail "the report does not say what it moved"

step "(b) a 1% error on the GO side is reported too"
out=$(expect_exit 1 "injected readings (go)" "$BIN" readings \
    --go "$GO_ROWS" --rust "$RUST_ROWS" \
    --go-station CSI --rust-station csi "${WINDOW[@]}" --inject barometer:1:go)
printf '%s\n' "$out" | grep -q 'barometer' || fail "the report does not name the injected metric"

step "(c) a 1% error in a statistic DEV-4 bounds is reported"
# The sharpest of the controls: `windd` is one of the two keys DEV-4 excuses, so this asserts
# the bound cannot absorb an error four orders of magnitude outside it. If DEV-4 were ever
# widened to a blanket exemption for its two keys, this is the step that would go red.
#
# `windd` rather than `period_rain`, which is the other key the rule names: it did not rain on
# the captured days, so `period_rain` is a hair above zero and a 1% relative error on a hair
# is still a hair. A relative injection is only a control on a key with a value, which is the
# reason the step below exists.
out=$(expect_exit 1 "injected responses (windd)" "$BIN" responses \
    --go dir:recorded/go --rust dir:recorded/rust --station CSI "${CENSUS[@]}" \
    --inject windd:1:rust)
printf '%s\n' "$out" | grep -q 'VERDICT (c): FAIL' || fail "the injection was not reported"
printf '%s\n' "$out" | grep -q 'windd' || fail "the report does not name the injected key"

step "an injection that moves nothing is refused rather than passed"
# The control's own control, and the reason the harness counts what it moved. `extratemp2` is
# zero in every captured bucket -- the station has no second extra temperature sensor -- so
# scaling it by 1% changes no value. A harness that reported a clean run there would be
# handing back a green verdict for a test that did not happen, which is the same failure as
# a gate that passes by running nothing.
out=$(expect_exit 2 "a no-op injection" "$BIN" responses \
    --go dir:recorded/go --rust dir:recorded/rust --station CSI "${CENSUS[@]}" \
    --inject extratemp2:1:rust)
printf '%s\n' "$out" | grep -q 'not a control' || fail "a no-op injection was not refused"

step "(c) a 1% error in DEV-7's relationship is reported"
out=$(expect_exit 1 "injected responses (solarjoules)" "$BIN" responses \
    --go dir:recorded/go --rust dir:recorded/rust --station CSI "${CENSUS[@]}" \
    --inject solarjoules:1:rust)
printf '%s\n' "$out" | grep -q 'solarjoules' || fail "the report does not name the injected key"

step "(c) a 1% error on the GO side is reported too"
out=$(expect_exit 1 "injected responses (go)" "$BIN" responses \
    --go dir:recorded/go --rust dir:recorded/rust --station CSI "${CENSUS[@]}" \
    --inject otemp:1:go)
printf '%s\n' "$out" | grep -q 'otemp' || fail "the report does not name the injected key"

step "(c) a missing census is reported rather than assumed away"
# The other direction of fail-loud: without n, DEV-4's and DEV-7's bounds do not exist, and a
# harness that guessed one would report a tolerance it did not have.
out=$(expect_exit 1 "responses with no census" "$BIN" responses \
    --go dir:recorded/go --rust dir:recorded/rust --station CSI)
printf '%s\n' "$out" | grep -q 'no_census' || fail "a missing census was not reported"

printf '\nSELF-TEST PASSED: the differ is green on the recorded pair, red on every injection,\n'
printf 'and red when the bound it needs has no n.\n'
