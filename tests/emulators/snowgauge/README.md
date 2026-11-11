# The snow gauge emulator

A snow gauge on a gRPC port: `snowgauge.SnowGaugeService/StreamReading`, and nothing else.

| This tree | Reference tree, at `c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf` |
|---|---|
| `remoteweather/protocols/snowgauge/snowgauge.pb.go` | `protocols/snowgauge/snowgauge.pb.go` |
| `remoteweather/protocols/snowgauge/snowgauge_grpc.pb.go` | `protocols/snowgauge/snowgauge_grpc.pb.go` |

Both are **byte-for-byte copies**, with their SHA-256 in `../manifest.toml` and verified by
`cargo xtask emulator-tests`. `../davis/README.md` has the whole of why the reference's files
are vendored rather than checked out, and what the digests are for.

`main.go` and the two `go.mod` files are **ours**. `main.go` is the device's behaviour — the
values, the cadence, the flags — and the `go.mod` files exist so that the vendored stubs
resolve under the import path they were generated for.

## What the split buys, and what it does not

The reference tree has **no snow gauge emulator** at the pinned commit: `cmd/` has four and
none of them is one. What it does have is the **generated stubs**, which is the half that
matters most here. `mw_codec::SnowGaugeProto` declares **two fields of a four-field message
by hand** — `ci/layers.toml` puts `mw-proto` in `mw-codec`'s own layer, so the codec cannot
use a generated type — and nothing in the programme had checked that hand-written
declaration against the schema a real gauge was generated from.

So:

- the **protocol** is independently defined: the service name, the method name, the field
  numbers and the wire types come from the reference's own `protoc` output;
- the **behaviour** is not: the numbers this emulator sends were written here, beside the
  driver, which is what `origin = "this-repository"` in the manifest records.

## The one thing this emulator costs

**It is the only emulator in this tree that is not standard-library-only.** It speaks gRPC,
so it needs `google.golang.org/grpc` and `google.golang.org/protobuf`, and `go build`
therefore needs a populated module cache or the network. `go.sum` is committed, so the build
is reproducible; it is not *offline* on a machine that has never fetched those modules.

The alternative was hand-rolling HTTP/2 and HPACK from the standard library, which is a
worse emulator and a worse test: the thing under test is a gRPC client, and an oracle that
did not use a real gRPC server would not exercise it.

## Flags

| Flag | What |
|---|---|
| `-port <n>` | the port; `0` lets the kernel choose and the port is printed as `ready grpc=<n>` |
| `-interval <d>` | between readings |
| `-count <n>` | how many to send; `0` is forever |
| `-base <mm>` | the distance with no snow under it, which the daemon also has configured |
| `-fault <n>` | every nth reading is a distance **greater** than `-base`, which is the fault **DEV-9** is about |
| `-hangup` | close the stream after `-count` readings instead of holding it open |
