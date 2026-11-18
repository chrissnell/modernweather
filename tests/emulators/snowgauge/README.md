# The snow gauge emulator

A `snowgauge-rs` gauge on an HTTP port: `GET /data`, `GET /health`, and nothing else.

`main.go` and `go.mod` are **ours**, and `go.mod` exists for the same reason the AirGradient
one does — so that `go build .` resolves nothing and needs no network.

## What this emulator used to be, and what changed

Until the firmware was replaced, the gauge spoke gRPC
(`snowgauge.SnowGaugeService/StreamReading`) and this directory held:

- the reference tree's **generated protobuf stubs**, vendored byte-for-byte at the pinned
  commit, with their SHA-256 in `../manifest.toml`;
- a `go.sum` and a `require` block for `google.golang.org/grpc`, which made this one of the
  two emulators here that `go build` could not produce offline;
- a `replace` directive so the vendored stubs resolved under the import path they were
  generated for.

All three are gone. The new firmware's reading surface is two JSON endpoints over plain
HTTP, so `net/http` and `encoding/json` are the whole of it.

**That is a real loss and it is worth naming.** The vendored stubs were the half of this
oracle that was *independently defined*: `mw_codec::SnowGaugeProto` declared two fields of a
four-field protobuf message by hand, and the stubs were what checked that declaration
against the schema a real gauge had been generated from. There is no equivalent artefact for
a JSON API — the firmware ships no schema file, so the field names and types in `main.go`
and in `mw_codec::snowgauge` both come from reading the same page of
`chrissnell/snowgauge-rs`'s `docs/integration.md`, and a misreading shared by the two would
pass. `../manifest.toml` records that as `origin = "this-repository"` with no
`[emulator.files]`, and the honest fix is the same one it names for the WeatherLink Live: an
emulator in the firmware's own tree, written by somebody who is not writing the driver.

What the gauge *does* have that no other device here does is a published API document with
an error table in it, and every flag below is a row of that table.

## Flags

| Flag | What |
|---|---|
| `-port <n>` | the port; `0` lets the kernel choose and it is printed as `ready http=<n>` |
| `-base <mm>` | the distance with no snow under it, which the daemon also has configured |
| `-trim-after <n>` | how many sensor frames before `trim_mean` stops being `null`. **30 on a real gauge**, which is about a minute after boot; a test that wants the trimmed-mean regime in five polls passes `2` |
| `-sensor all\|dead\|stalled` | `dead` is a sensor that never speaks — `status` stays `no_data`, `readings` stays `0`, `/health.bytes_rx` stays `0`. `stalled` is the UART link dying mid-run: `readings` freezes while `uptime_ms` keeps growing, which is the firmware documentation's own liveness test |
| `-stall-after <n>` | how many frames `-sensor stalled` produces before it stops |
| `-fault <n>` | every nth response reports a distance **greater** than `-base`, which is the fault **DEV-9** is about: stored, it is a negative snow depth in a column with `min`/`mean`/`max` aggregates over it |
| `-reboot <n>` | reboot every nth response: `uptime_ms` drops and `readings` restarts at zero, which is the state a driver that tested the sensor before the uptime would call a dead sensor |
| `-firmware current\|old` | `old` is a gauge that predates `/data`: `404` there, and a `fw_version` from before the endpoint existed |
| `-uptime-step <ms>` | how much `uptime_ms` advances per response, default `5000` |

## Why `uptime_ms` is not the wall clock

A real gauge is polled seconds apart; this one answers in microseconds. Read off the wall
clock, several consecutive responses in a test would carry the **same** millisecond — and a
reboot, which the driver detects as `uptime_ms` *decreasing*, would be invisible between two
responses that both read zero. So the emulator advances it one `-uptime-step` per response,
defaulting to the driver's own poll period, and what the driver sees is what it would see
from a device it was actually polling.

## Re-deriving the captured bodies

`crates/mw-codec/tests/frames/snowgauge-*.json` came off this emulator.
`../README.md` has the recipe.
