# `campbell-emulator`, vendored from the pinned reference commit

`main.go` is a **byte-for-byte copy** of `cmd/campbell-emulator/main.go` at
`c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf`. Re-derive it with

```sh
git -C <a checkout of chrissnell/remoteweather> show \
    c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf:cmd/campbell-emulator/main.go
```

`go.mod` is ours, and is the only thing that is: this emulator imports only the standard
library, so it needs nothing but a module path. `../davis/README.md` has why these are
vendored rather than checked out, what that keeps of **D-40** and **A-W8-4**, and what the
SHA-256 digests in `../manifest.toml` are for.

## What it does

A Campbell Scientific datalogger on a TCP port: `-port` chooses it, `-interval` sets the
output period, and every connection gets one JSON object immediately and then one per
interval, newline-terminated, forever. Ten keys — `batt_volt`, `airtemp_f`, `rh`, `baro`,
`baro_temp_f`, `slr_w`, `slr_mj`, `rain_in`, `wind_s`, `wind_d` — which is exactly the
struct `internal/weatherstations/campbell/station.go` decodes and exactly what
`mw_codec::CampbellAscii` maps.

**It is a `json.Encoder` on a socket**, which is the property the `Lines` session exists
for: `Encoder.Encode` writes the object and a newline in whatever TCP segments the kernel
chooses, so one read is not one record and one record is not one read.

**The serial path**, as for Davis: a pty bridged to this port with `socat`
(`mw_station::testing::PtyBridge`), which is the transport the owner's own station is on
(**N-20**).
