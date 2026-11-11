# The satellite emulator

A Go-era satellite streaming at a collector: `WeatherV1.SendWeatherReadings`.

| This tree | Reference tree, at `c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf` |
|---|---|
| `remoteweather/protocols/remoteweather/remoteweather.pb.go` | `protocols/remoteweather/remoteweather.pb.go` |
| `remoteweather/protocols/remoteweather/remoteweather_grpc.pb.go` | `protocols/remoteweather/remoteweather_grpc.pb.go` |

Both are **byte-for-byte copies**, digest-verified by `cargo xtask emulator-tests`.
`../davis/README.md` has why the reference's files are vendored and what the digests are
for; `../snowgauge/README.md` has the one cost these two gRPC emulators carry that the other
four do not — they are not standard-library-only.

`main.go` and the two `go.mod` files are ours.

## Why this one is worth 3,453 lines of vendored generated Go

Because of what it checks. `metrics/registry.toml` opens with

> Bootstrapped from the Go implementation's own definition (215 columns in
> `internal/types/weather.go` and `internal/storage/timescaledb/sql.go` at remoteweather
> `c3ec9f7`).

and **D-21** is the decision that a field number is allocated once and never reused, because
*"a reused number silently reinterprets historical data on every client that has not been
rebuilt"*. Every protobuf field number in this programme rests on that bootstrap being
faithful — and until this emulator, nothing had checked it **on the wire**.

`remoteweather.pb.go` is the reference's own `protoc` output for `WeatherReading`. This
emulator encodes with it; `mw_grpc::grpcrx` decodes the bytes as
`modernweather.v1.Reading`, which is generated from the registry. A field number that
drifted during the bootstrap is a reading whose barometer arrives in the humidity column, and
`crates/modernweather/tests/emulator_grpcrx.rs` asserts one value per field the satellite
sets.

That is not a test a fixture can make: both sides have to be generated from their own
schema, by their own generator, and meet over a socket.

## Flags

| Flag | What |
|---|---|
| `-url <host:port>` | the collector's gRPC address |
| `-id <key>` | the `station_id` to present — the key the station was registered under |
| `-name <name>` | the `stationName` to present; a Go-era sender sets only this |
| `-count <n>` | how many readings to stream |
| `-interval <d>` | between readings |

It prints `sent n=<i>` per reading and then either `accepted` or `refused status=<code>`,
which is the status the collector chose. A `Send` that fails is `send-failed n=<i>` and is
**not** an error: grpc-go's own rule is that a failed `Send` means the stream is already
over and the real error is the status `CloseAndRecv` returns.
