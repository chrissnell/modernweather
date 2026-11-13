#!/usr/bin/env bash
# Run GRA-787's serial-bridge proof and assert that every one of its tests ran.
#
# `crates/mw-station/tests/serial_bridge.rs` is the assertion that a `ser2net` fan-out
# broadcasts every byte to every connected client, rather than handing bytes to whichever
# client reads first. M5-2's mechanism (b) on the Campbell station rests on that, and a
# multiplexer that split the stream would leave both daemons with gap-ridden data that still
# parses -- so the property is asserted byte for byte, and it has two negative controls.
#
# Why a runner rather than `cargo test`. The tests are `#[ignore]`d, and a `--ignored` filter
# that matches nothing exits zero: a gate that reports green while running nothing is
# docs/reconciliation.md G-1. So the count is declared here and checked, which is the same
# rule `cargo xtask emulator-tests` applies to the emulator gate.
#
# Requires: a Rust toolchain, `socat`, and `ser2net` 4.x. None of them is optional and none
# is faked -- the binary under test is the one a deployment actually runs, which is the whole
# point. Set `SER2NET` if it is not on PATH; the Debian package puts it in
# /usr/sbin, which a non-root PATH does not include. Measured against 4.6.4 (Debian trixie)
# and 4.6.8 (Arch extra), because the two differ and a fan-out is not a thing to assume.
#
# Arms itself when crates/mw-station/tests/serial_bridge.rs exists (see ci/gates.toml).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# The number of tests in crates/mw-station/tests/serial_bridge.rs. Adding one without
# raising this is a test that runs and is not counted; raising it without adding one turns
# the gate red, which is the direction that matters.
EXPECTED=7

ser2net=${SER2NET:-}
if [ -n "$ser2net" ] && ! [ -x "$ser2net" ]; then
  echo "serial-bridge: SER2NET=$ser2net is not an executable" >&2
  exit 1
fi
if [ -z "$ser2net" ]; then
  for candidate in ser2net /usr/sbin/ser2net /usr/local/sbin/ser2net; do
    if command -v "$candidate" >/dev/null 2>&1; then ser2net=$candidate; break; fi
  done
fi
[ -n "$ser2net" ] || {
  echo "serial-bridge: ser2net is not installed, and this gate does not skip." >&2
  echo "  Debian/Ubuntu: apt-get install ser2net   Arch: pacman -S ser2net" >&2
  echo "  Already installed elsewhere: SER2NET=/path/to/ser2net $0" >&2
  exit 1
}
command -v socat >/dev/null || { echo "serial-bridge: socat is not installed" >&2; exit 1; }

# Both versions in the log, because the two that were measured by hand are not the same
# release and a future divergence should be visible rather than assumed away.
echo "serial-bridge: $("$ser2net" -v), socat $(socat -V | sed -n 's/^socat version \([^ ]*\).*/\1/p')"

out=$(mktemp)
trap 'rm -f "$out"' EXIT
set +e
SER2NET="$ser2net" cargo test -p mw-station --test serial_bridge -- --ignored 2>&1 | tee "$out"
status=${PIPESTATUS[0]}
set -e
[ "$status" -eq 0 ] || { echo "serial-bridge: the proof failed" >&2; exit 1; }

# `test result: ok. N passed; 0 failed; ...` -- N is what has to be EXPECTED. A filter that
# matched nothing also prints `ok`, with N of 0.
passed=$(sed -n 's/^test result: ok\. \([0-9]\+\) passed.*/\1/p' "$out" | tail -1)
[ -n "$passed" ] || { echo "serial-bridge: no test result line in the output" >&2; exit 1; }
if [ "$passed" -ne "$EXPECTED" ]; then
  echo "serial-bridge: $passed test(s) passed, expected $EXPECTED." >&2
  echo "  A filter that matches nothing exits zero, so this is the check that the gate ran." >&2
  exit 1
fi
echo "serial-bridge: $passed of $EXPECTED tests passed"
