# Captured frames

## WeatherLink Live

The golden inputs for `tests/wll_golden.rs`. Each file is one frame exactly as a
WeatherLink Live produces it, including the fields this codec does not read — `lsid`,
`rx_state`, `bar_trend`, the 1- and 2-minute wind averages, the 15/60-minute and 24-hour
rain accumulators — because a fixture trimmed to what the decoder wants cannot catch a
decoder that stops tolerating an unknown field.

**Provenance, stated exactly.** These are the payloads of the Davis WeatherLink Live local
API v1 as its published documentation specifies them, field for field and with its example
values, cross-checked against the field set
`internal/weatherstations/weatherlinklive/types.go` declares in the Go reference tree at
the pinned commit. They were **not** captured from a physical unit: no WeatherLink Live is
reachable from the environment this work item was implemented in. That is a real limit on
what these fixtures prove — they pin the decoder against the documented protocol and
against the Go driver's understanding of it, not against a particular firmware's output.
Replacing any of them with a datagram captured from the owner's own unit is strictly
better and needs no code change.

| File | What it is |
|---|---|
| `broadcast.json` | A UDP broadcast datagram: one ISS record carrying only the high-rate wind and rain fields. Note that it is **not** wrapped in `data`. |
| `current-conditions.json` | `GET /v1/current_conditions` on a Vantage Pro2 Plus: ISS, the unit's own temp/humidity, and the unit's barometer. |
| `current-conditions-agricultural.json` | The same, with a leaf/soil station on transmitter 2. |
| `current-conditions-two-transmitters.json` | Three temperature/humidity transmitters, the middle one **present with null fields** (`rx_state: 2`) — a configured transmitter the unit has not heard from. This is the fixture for the `th` ordering rule, because a dropout must not renumber the columns after it. |
| `real-time.json` | `GET /v1/real_time?duration=3600`'s answer. |
| `device-error.json` | The unit refusing, with `200` on the status line and the refusal in the body. |
| `broadcast-metric-collector.json` | A broadcast datagram from a unit with a 0.1 mm collector, which is the case the Go driver's raw tip counts get wrong by a factor of 254. |

## Davis, Campbell, AirGradient, Ambient and the snow gauge

The golden inputs for `tests/davis_golden.rs`, `tests/campbell_golden.rs`,
`tests/airgradient_golden.rs`, `tests/ambient_golden.rs` and `tests/snowgauge_golden.rs`.

**Provenance, stated exactly.** Unlike the WeatherLink Live frames above, every one of
these was **captured from a running emulator** — the process `cargo xtask emulator-tests`
drives — by `tests/emulators/README.md`'s own instructions. Three of the five therefore come
from the reference tree's own code at the pinned commit:

| File | Captured from | What that makes it |
|---|---|---|
| `davis-loop-1.bin`, `-2`, `-3` | `cmd/davis-emulator`, vendored verbatim | the reference's own `LOOP` writer, including its CRC-16 |
| `campbell-packets.jsonl` | `cmd/campbell-emulator`, vendored verbatim | the reference's own `json.Encoder` output |
| `snowgauge-reading.bin` | `protocols/snowgauge`'s generated marshaller, vendored verbatim | a real gauge's wire bytes, field numbers and all |
| `airgradient-*.json` | `tests/emulators/airgradient`, written here | the device's published `/measures/current` shape |
| `ambient-upload.txt` | `tests/emulators/ambient`, written here | the vendor's `updateraw` vocabulary |

So the first three pin a decoder against **the reference implementation's own encoder**,
which is the strongest thing a fixture in this repository can be: a byte offset or a field
number that drifted is a failing test with no hardware and no Go toolchain involved. The
last two pin it against an emulator written beside the driver, which cannot catch a
misunderstanding the two share — `tests/emulators/manifest.toml` records which is which and
the gate prints it on every run.

**Why these are files and not literals in the test.** A 99-byte binary packet written as a
Rust array is unreadable and unreviewable; a captured file can be re-derived, and
`od -An -tx1` on it is a thing a reviewer can do. The WLL fixtures' reason for being
`include_str!`'d rather than read at run time applies here too, through `include_bytes!`:
this crate may not name the filesystem.

| File | What it is |
|---|---|
| `davis-loop-1.bin` | One 99-byte `LOOP` record, header to CRC. The three are consecutive and carry different values, which is what makes "the decoder reads the packet rather than a constant" assertable. |
| `davis-loop-2.bin` | The second of the run. |
| `davis-loop-3.bin` | The third. |
| `campbell-packets.jsonl` | Four consecutive outputs from the datalogger's program, newline-terminated as it writes them — including the `omitempty` behaviour that makes a zero-valued key **absent**. |
| `airgradient-current.json` | `GET /measures/current` on current firmware: the `*Compensated` fields present. |
| `airgradient-old-firmware.json` | The same device on older firmware: no `*Compensated` fields, which is the body that makes the Go driver store 32 °F. |
| `airgradient-no-sensor.json` | A monitor whose temperature/humidity module is not reporting: no `atmp` and no `rhum` at all. |
| `ambient-upload.txt` | One `updateraw` query string, **percent-encoded exactly as the console sends it** — which is what makes the `+`-is-a-space and `%3A`-is-a-colon handling testable rather than assumed. |
| `snowgauge-reading.bin` | One `snowgauge.Reading`, including the two `google.protobuf.Duration` fields the codec declares no field for — so "`prost` discards what is not declared" is asserted rather than assumed. |
