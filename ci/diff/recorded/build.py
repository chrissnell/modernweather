#!/usr/bin/env python3
"""Derive the Rust side of the negative control's corpus from the captured Go side.

The Go half of this directory is a CAPTURE: `go/*.json` are four `/span` bands, `/latest`,
`/snow` and `/almanac` as the live Go server answered them for station CSI on 2026-11-13, and
`readings-go.tsv` is six consecutive readings of the same station out of the Go hypertable,
in exactly the shape `mw-diff`'s own query selects.

The Rust half is DERIVED, by applying the deviations `expected.toml` predicts and nothing
else. That is the whole point: a clean run over this corpus is green *by construction*, so
`self-test.sh` can inject a 1% error and know that the red run it gets back came from the
injection rather than from the corpus. A corpus captured from two live systems could not do
that -- the two deployments differ in their configuration, and the harness correctly reports
those differences, which would make every control red before it started.

So this file is the readable statement of what each rule predicts, in the other direction
from `src/expected.rs`: there a rule decides whether a difference is allowed, here it
MANUFACTURES exactly that difference. A rule that drifted away from its prediction would
make `self-test.sh` red, because the manufactured difference would stop matching.

  ci/diff/recorded/build.py          rewrite rust/ and the two censuses
  ci/diff/recorded/build.py --check  rewrite nothing, exit 1 if the tree differs

What it applies, and nothing else:

  D-26    readings: a metric the Go side wrote as exactly 0.0 becomes NULL
  DEV-9   readings: windchill and heatindex become NULL where the Go side wrote outtemp
  DEV-4   /span: period_rain and windd move by 90% of the float4 accumulation bound
  DEV-7   /span: solarjoules becomes the bucket total, which is the mean times the count
  DEV-12  /span: one extra bucket at the leading edge
  pairing readings are stamped 3 ms later and /latest 2 ms later, which is the measured
          delay between the two daemons receiving one frame
"""

import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

# 2^-24, half an ulp of float4's mantissa. The same constant as src/numeric.rs and as
# w9.rollup_report; if they ever disagree, self-test.sh is what says so.
FLOAT4_HALF_ULP = 5.960464477539063e-08

# The four bands, their bucket width and the readings in one bucket at the station's
# five-second output interval. The censuses this writes are the same on both sides, so the
# corpus has no unevenly populated bucket -- that case is exercised by a dedicated run in
# self-test.sh rather than by every run.
BANDS = [("1m", "3h", 60), ("5m", "24h", 300), ("1h", "168h", 3600), ("1d", "720h", 86400)]
CADENCE_SECONDS = 5

# The two statistics the Go views both accumulate and store in `real` (DEV-4), and the one
# whose statistic changed from the mean to the total (DEV-7).
FLOAT4_ACCUMULATED = ("period_rain", "windd")
MEAN_TO_TOTAL = ("solarjoules",)


def bound(n, a, b):
    """src/numeric.rs's float4_accumulated_tolerance, restated."""
    return max(n, 1) * FLOAT4_HALF_ULP * max(abs(a), abs(b), 1.0)


def derive_span(band_id, span, bucket_seconds, go):
    n = bucket_seconds // CADENCE_SECONDS
    elements = list(go["json"])
    # DEV-12: the aligned, inclusive lower bound includes exactly one bucket Go's unaligned
    # `>` excludes. Older data rather than different data, so a copy of the oldest bucket at
    # one bucket earlier is a faithful stand-in.
    leading = dict(elements[0])
    leading["ts"] = elements[0]["ts"] - bucket_seconds * 1000
    out = []
    for element in [leading] + elements:
        e = dict(element)
        for key in FLOAT4_ACCUMULATED:
            if isinstance(e.get(key), (int, float)):
                # Inside the bound by a margin, so a rule whose constant was wrong by a
                # factor of two still passes and only a rule that lost its n fails. The
                # boundary itself is asserted in tests/control.rs, which can place a value
                # either side of it exactly.
                e[key] = e[key] + 0.9 * bound(n, e[key], e[key])
        for key in MEAN_TO_TOTAL:
            if isinstance(e.get(key), (int, float)):
                e[key] = e[key] * n
        out.append(e)
    return {"status": go["status"], "json": out}, n


def derive_readings(lines):
    header = lines[0].rstrip("\n").split("\t")
    columns = header[1:]
    at = {c: i + 1 for i, c in enumerate(columns)}
    out = [lines[0].rstrip("\n")]
    for line in lines[1:]:
        if not line.strip():
            continue
        f = line.rstrip("\n").split("\t")
        assert len(f) == len(header), f"{len(f)} fields against {len(header)} columns"
        # The pairing offset: each daemon stamps its own receipt of the frame.
        f[0] = str(int(f[0]) + 3000)
        outtemp = f[at["outtemp"]]
        for c in columns:
            i = at[c]
            if f[i] == r"\N":
                continue
            # DEV-9 part (1) before D-26: the Go drivers store the air temperature where the
            # formula does not apply, and that is a different prediction from zero-filling.
            if c in ("windchill", "heatindex") and f[i] == outtemp:
                f[i] = r"\N"
                continue
            if float(f[i]) == 0.0:
                f[i] = r"\N"
        out.append("\t".join(f))
    return "\n".join(out) + "\n"


def main():
    check = "--check" in sys.argv[1:]
    written = {}

    census = []
    for band_id, span, bucket_seconds in BANDS:
        go = json.load(open(os.path.join(HERE, "go", f"span-{span}.json")))
        rust, n = derive_span(band_id, span, bucket_seconds, go)
        written[os.path.join("rust", f"span-{span}.json")] = (
            json.dumps(rust, indent=1) + "\n"
        )
        for element in rust["json"]:
            census.append(f"{band_id}\t{element['ts']}\t{n}")

    # /latest: the same reading, stamped 2 ms later. Nothing else differs, so a clean run
    # has to report this endpoint green -- which is what makes the injected run's red
    # attributable.
    latest = json.load(open(os.path.join(HERE, "go", "latest.json")))
    latest["json"] = dict(latest["json"])
    latest["json"]["ts"] = latest["json"]["ts"] + 2
    written["rust/latest.json"] = json.dumps(latest, indent=1) + "\n"

    for name in ("snow", "almanac"):
        same = open(os.path.join(HERE, "go", f"{name}.json")).read()
        written[f"rust/{name}.json"] = same

    with open(os.path.join(HERE, "readings-go.tsv")) as fh:
        written["readings-rust.tsv"] = derive_readings(fh.readlines())

    body = (
        "# Readings per bucket, per band: band<TAB>bucket_ts_ms<TAB>n. Written by build.py.\n"
        + "\n".join(census)
        + "\n"
    )
    written["census.tsv"] = body
    written["census-rust.tsv"] = body

    drift = []
    for rel, content in written.items():
        path = os.path.join(HERE, rel)
        current = open(path).read() if os.path.exists(path) else None
        if current == content:
            continue
        drift.append(rel)
        if not check:
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as fh:
                fh.write(content)

    if check and drift:
        print("the derived corpus does not match build.py:")
        for rel in drift:
            print(f"  {rel}")
        print("run ci/diff/recorded/build.py and commit the result")
        return 1
    print(f"{'checked' if check else 'wrote'} {len(written)} files, {len(drift)} differed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
