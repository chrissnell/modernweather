# `davis-emulator`, vendored from the pinned reference commit

`main.go` and `remoteweather/pkg/crc16/crc16.go` are **byte-for-byte copies** of

| This tree | Reference tree, at `c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf` |
|---|---|
| `main.go` | `cmd/davis-emulator/main.go` |
| `remoteweather/pkg/crc16/crc16.go` | `pkg/crc16/crc16.go` |

Re-derive either of them with

```sh
git -C <a checkout of chrissnell/remoteweather> show \
    c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf:cmd/davis-emulator/main.go
```

The two `go.mod` files are **ours** and are the only thing that is: they exist so that
`main.go`'s own import of `github.com/chrissnell/remoteweather/pkg/crc16` resolves against
the vendored copy without the file being edited. A `replace` directive is what buys that.

## Why it is vendored rather than checked out

**DESIGN.md D-40** keeps the emulators in Go deliberately — *"a driver tested against a
simulator written in the same language from the same understanding tests that understanding
against itself"* — and **A-W8-4** adds that they are consumed at a **pinned commit** of the
reference repository. A test that fetched that commit would need the network at test time,
which is a test that cannot run on a machine with no credentials for a private repository
and a test that fails for a reason unrelated to the driver. So the source is here, at the
pin, and the pin is recorded in three places: `tests/emulators/manifest.toml`, the table
above, and the `git show` command that reproduces it.

**What vendoring keeps.** Everything D-40 asks for. This emulator was written years before
this programme, by somebody who was not writing this driver, against the real hardware's
protocol — which is the independent-authorship property the WeatherLink Live emulator in
this tree explicitly does **not** have (see `../manifest.toml`).

**What vendoring costs, and what is done about it.** A local copy can be edited, and the
realistic way an emulator test gets "fixed" is by editing the oracle until it passes.
`tests/emulators/manifest.toml` records a **SHA-256 of every vendored file** and
`cargo xtask emulator-tests` refuses to run an emulator whose files do not match — so an
edit is a red gate rather than a green one. What that cannot catch is the reference tree
moving, and it does not need to: the commit is pinned, and bumping it is a reviewed change
to the manifest *and* to these digests in the same diff.

## What it does

A Davis Vantage console on a TCP port. `-port` chooses it; the `-flaky` family of flags
simulates dropped bytes, corrupted bytes, bad CRCs, truncated packets, disconnections,
hangs, slow responses and ignored commands, which is what makes it a better oracle than
anything written beside the driver.

It answers a bare newline with `\n\r`, a `LOOP n` command with `<ACK>` and then `n`
99-byte LOOP records at 1.5-second intervals, and anything else with `<NAK>`.

**Two things it does that the specification does not**, both of which
`crates/mw-codec/src/davis/wire.rs` records rather than works around: it writes the 3-hour
barometer trend at byte 95 and a zero at byte 96, where the specification has `\n` and
`\r`; and its `ConsBatteryVoltage` of `4050` is commented as "13.5V * 300 + 0.5", which is
a different scale from the one the Go *driver* divides by. Both are properties of the pair
rather than of this decoder, and the Rust tests assert what the Go driver produces from the
Go emulator's bytes.

**The serial path.** This emulator speaks TCP and a Davis console on a serial line speaks
the same bytes, so the serial half of the driver is exercised by bridging a pty to this
port with `socat` (`mw_station::testing::PtyBridge`). That is W3 **D-83**'s "`socat` pty
pair" with the pinned oracle on the other end of it.
