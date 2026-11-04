# Captured WeatherLink Live frames

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
| `real-time.json` | `GET /v1/real_time?duration=3600`'s answer. |
| `device-error.json` | The unit refusing, with `200` on the status line and the refusal in the body. |
| `broadcast-metric-collector.json` | A broadcast datagram from a unit with a 0.1 mm collector, which is the case the Go driver's raw tip counts get wrong by a factor of 254. |
