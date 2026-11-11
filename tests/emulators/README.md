# The emulators, and how to re-derive a captured frame

`manifest.toml` is the authority: it declares which emulators exist, which tests each one
must run, where each one came from and — for the vendored ones — the SHA-256 of every file.
`cargo xtask emulator-tests` reads it and nothing else, and `ci/gates.toml` declares that as
the `driver-emulators` gate.

| Directory | Device | `origin` | Standard library only |
|---|---|---|---|
| `wll/` | Davis WeatherLink Live | `this-repository` | yes |
| `davis/` | Davis Vantage Pro/Pro2 | `reference-vendored` | yes |
| `campbell/` | Campbell Scientific datalogger | `reference-vendored` | yes |
| `airgradient/` | AirGradient ONE | `this-repository` | yes |
| `ambient/` | Ambient Weather console (a **client**) | `this-repository` | yes |
| `snowgauge/` | the snow gauge | `this-repository`, stubs vendored | **no** — gRPC |
| `grpcrx/` | a Go-era satellite (a **client**) | `this-repository`, stubs vendored | **no** — gRPC |

Each directory has its own `README.md` with what the emulator does, its flags, and — for a
vendored one — the `git show` command that reproduces every byte of it from the pinned
commit.

## Re-deriving a captured frame

`crates/mw-codec/tests/frames/` holds the level-1 golden inputs, and five of them were
captured from the emulators here rather than written by hand. The recipes are below so that
a reader can check a fixture against the device, and so that replacing one after a pin bump
is a command rather than an investigation.

Every recipe assumes the emulator has been built: `go build -o /tmp/<name> .` inside its
directory.

**`davis-loop-{1,2,3}.bin`** — the protocol has to be spoken, because the console sends
nothing until it is asked:

```sh
/tmp/davis -port 24711 &
python3 - <<'PY'
import socket
s = socket.create_connection(("127.0.0.1", 24711)); s.settimeout(20)
s.sendall(b"\n")                                   # wake
while b"\n\r" not in s.recv(64): pass
s.sendall(b"LOOP 3\n")                             # ask for a run of three
buf = b""
while len(buf) < 1 + 3 * 99: buf += s.recv(4096)   # <ACK> then 3 x 99 bytes
assert buf[0] == 0x06
for n in range(3):
    open(f"crates/mw-codec/tests/frames/davis-loop-{n+1}.bin", "wb").write(
        buf[1 + n*99 : 1 + (n+1)*99])
PY
```

**`campbell-packets.jsonl`** — the datalogger writes unprompted, so the recipe is a read:

```sh
/tmp/campbell -port 24712 -interval 50ms &
timeout 5 socat -u TCP:127.0.0.1:24712 - | head -4 \
    > crates/mw-codec/tests/frames/campbell-packets.jsonl
```

**`airgradient-*.json`** — the device is a server, so the recipe is a `GET`. The port is on
the emulator's own `ready http=<port>` line:

```sh
/tmp/ag -port 0 &                                  # and `-firmware old`, `-sensor none`
curl -s "http://127.0.0.1:<port>/measures/current" \
    > crates/mw-codec/tests/frames/airgradient-current.json
```

**`ambient-upload.txt`** — the console is a **client**, so the recipe is a listener. The
fixture is the query string of the request line, percent-encoding and all:

```sh
socat -u TCP-LISTEN:24713,reuseaddr - | head -1 &   # the request line
/tmp/amb -url http://127.0.0.1:24713/ingest/ambient -id roof -password hunter2 -count 1
# then take everything after the `?` and before the space
```

**`snowgauge-reading.bin`** — through the reference tree's own generated marshaller, so that
the fixture is a real gauge's field numbers rather than a hand-written encoder's. A six-line
Go program in a scratch module that `replace`s `github.com/chrissnell/remoteweather` at
`snowgauge/remoteweather`, marshals a `snowgauge.Reading` and writes it to stdout; the one
committed here carries `stationName: "roof-gauge"`, `distance: 1352` and both uptimes at
42 s.

## Why there is no recipe for the WeatherLink Live frames

There is nothing to run them against. `wll/`'s emulator was written here *from* those
fixtures, so capturing them from it would be circular —
`crates/mw-codec/tests/frames/README.md` states their provenance and its limit, which is
that they are the published local API's documented payloads rather than a physical unit's
output.
