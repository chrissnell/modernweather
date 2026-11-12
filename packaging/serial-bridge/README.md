# A serial fan-out for a stream station

A serial line has exactly one reader. This makes it several, so that more than one process
can have the same datalogger at the same time.

Two reasons to want it:

- **Running two daemons against one station.** Diffing a new implementation against an old
  one *by value on real data* means both of them seeing the same readings. A station that
  broadcasts over UDP — a WeatherLink Live, say — gives that away for free. A station on a
  serial line does not: there is one reader, so without a multiplexer there is no
  comparison to make. This is what `docs/implementation-plan.md` M5-1a exists for.
- **A station whose cable and whose daemon are on different machines.** The bridge owns the
  line; the daemon configures Campbell or Davis as `transport: tcp_out` and connects over
  the network. Both drivers already support it — `crates/mw-station/src/stream.rs` is
  written around exactly this — and so does the Go implementation, which dials TCP whenever
  its `serial_device` is empty.

| | |
|---|---|
| Configuration | [`ser2net.yaml.example`](ser2net.yaml.example) → `/etc/ser2net/<station>.yaml` |
| Unit | [`campbell-serial-bridge.service`](campbell-serial-bridge.service) → `/etc/systemd/system/` |
| Moving an existing `remoteweather` onto it | [`cutover.sh`](cutover.sh), which writes its own rollback first |
| Proof | `crates/mw-station/tests/serial_bridge.rs`, run by `ci/serial-bridge/prove.sh` |

**Everything here is an example, and no file in this directory describes a real
installation.** The device path, the line rate and the listen address are site-specific;
`ser2net.yaml.example` leaves them as placeholders that are deliberately not valid, because
each of them fails *quietly* when guessed wrong and a plausible-looking default is worse
than a file that refuses to load.

## What was measured

Everything in this table is asserted by `crates/mw-station/tests/serial_bridge.rs` against a
real `ser2net` over a real character device, with a `socat` pty pair standing in for the
station — so it is reproducible on any machine and needs no hardware. Run on **two**
versions, because distributions do not agree: **4.6.4** (Debian trixie) and **4.6.8** (Arch
`extra`).

| | Measured |
|---|---|
| **It broadcasts** | two clients connected at once receive the identical byte stream, asserted byte for byte over 200 records — not a shared prefix, the whole stream. This is the question the whole exercise turned on: a multiplexer that handed bytes to whichever client read first would leave *both* daemons with gap-ridden data that still parses, which nothing downstream could tell from weather |
| **The control bites** | the same apparatus with `max-connections: 1` fails that assertion, so it is sensitive to the configuration it is proving |
| **An extra client cannot displace a reader** | over the limit, a client is answered `Port already in use` and closed; the clients already reading do not notice. `kickolduser` would change that and is deliberately not set |
| **One reader wedging does not starve the others** | a client that stops reading entirely costs the others nothing across 6,000 records — several hours at a typical station's output period |
| **The bridge owns the line** | `ser2net` sets `TIOCEXCL`, so a stray serial reader gets `EBUSY` and retries loudly rather than silently taking half the stream |
| **Two readers of one device do split it** | the negative control for the above, and the reason the cutover order below is what it is |
| **The driver reads it** | the real `StreamStation` + `CampbellAscii` driver admits one reading per record through the bridge while a second client reads the same bytes |
| **An absent device is not a failure to start** | `ser2net` starts, listens, and opens the device when a client arrives. With the cable out, a client is answered `Device open failure: …`; when the device returns, the next client works with no restart. A replug is a non-event |

## The one new hazard

**The port is writable.** `ser2net` is bidirectional: bytes a client sends are forwarded to
the datalogger. Neither daemon writes to a Campbell line — `remoteweather`'s driver contains
no `.Write(` at all, and `mw-station`'s session carries
`mw_station::lines::tests::the_session_writes_nothing` as its assertion — so the fan-out
cannot make two readers fight over the port. But before the bridge, the only thing that
*could* write was a file descriptor one daemon held; after it, anything that can reach the
TCP port can.

The accepter is `tcp` rather than `telnet`, so there is no RFC 2217 negotiation and a client
cannot change the line rate. Beyond that it is a host firewall question. `ser2net`'s own
`remaddr` option cannot help much: it takes an `<address>,<port>` pair and no CIDR, which was
tried. **Bind the loopback unless a reader is genuinely on another machine.**

## Cutover, for a station an existing daemon is already reading

The moment the bridge takes the device, the daemon has to have let go of it. One process
owns a serial port, so there is a brief window where the daemon is reconnecting.

**The order below is not caution, it is a measured requirement.** `remoteweather`'s serial
library (`github.com/tarm/goserial`) sets neither `TIOCEXCL` nor `O_EXCL` nor a `flock`, so
if the bridge starts while the daemon still holds the device, the bridge's `open(2)`
**succeeds** and the kernel then hands each byte to whichever of them reads first. There is
no error anywhere to notice. This order makes it impossible, because by the time the bridge
opens the device nothing else has it.

Have the rollback in hand before stopping anything — step 2 writes it.

```sh
# 1. Install and stage. Nothing is live yet; the daemon still owns the port.
#    (ser2net is packaged by Debian/Ubuntu and by Arch; both were measured.)
useradd --system --no-create-home --shell "$(command -v nologin)" ser2net   # if absent
ls -l "$DEVICE"                        # note the owning group: uucp or dialout,
                                       # and keep the matching SupplementaryGroups line
install -d /etc/ser2net
cp ser2net.yaml.example /etc/ser2net/campbell.yaml
$EDITOR /etc/ser2net/campbell.yaml     # the placeholders: device, baud, address
install -m 0644 campbell-serial-bridge.service /etc/systemd/system/
systemctl daemon-reload

# 2. Write the rollback, then move the station onto TCP. Still nothing restarted.
./cutover.sh plan                      # read this
./cutover.sh apply                     # writes ./rollback.sql from the row as it was

# 3. THE GAP STARTS HERE. The daemon lets go of the serial port and starts dialling.
systemctl restart remoteweather
#    It will log `could not connect to …` and retry every 5 s. Expected.

# 4. THE GAP ENDS HERE.
systemctl enable --now campbell-serial-bridge
```

The gap is bounded by the daemon's own retry — `connectToNetworkStation` loops with a 5 s
sleep — so it costs a few readings and not a restart. **Do it once**, not repeatedly while
iterating: iterate against a `socat` pty and `ci/serial-bridge/prove.sh`, which needs no
hardware and no station.

## Verify, by value and not by liveness

```sh
systemctl status campbell-serial-bridge
journalctl -u remoteweather -n 20      # expect the TCP connect, and nothing after it

# A reading landed recently. This is the check that matters: a process that is up and a
# station that is reporting are different claims.
psql -c "SELECT max(time), now() - max(time) AS age FROM weather;"
```

## Rollback

```sh
systemctl disable --now campbell-serial-bridge   # the bridge must let go FIRST
sqlite3 <config.db> < rollback.sql
systemctl restart remoteweather
```

Same rule in reverse, and for the same reason: if the daemon goes back to the serial port
while the bridge still has it, they both read it. Stopping the bridge first makes that
impossible.

**Never leave the station with no reader.** If the bridge will not start, the rollback above
is three commands and takes seconds; run it rather than debugging with the station dark.

## If the bridge starts but delivers nothing

In order of likelihood:

1. **Wrong line rate.** Every line fails to parse and the station looks connected and
   silent. `stty -F "$DEVICE"` shows what the bridge set; the station's existing
   configuration shows what it should be.
2. **A stale uucp lock**, if the bridge was ever killed with `SIGKILL`. gensio takes two
   locks per device — one keyed by the path, one by its `major.minor` — and only removes
   them on a clean exit. The symptom is a client answered `Device open failure: Object was
   already in use` by a process that looks healthy. `ls /run/lock/LCK..*`, and remove the
   ones naming this device once nothing is running. This is why the unit stops with
   `SIGTERM`.
3. **Something else has the device.** `fuser -v "$DEVICE"`. If it is the old daemon, its
   `serial_device` column is not empty — `./cutover.sh plan` shows the row.
4. **`ser2net` is listening but every connector is broken**, which looks like a healthy
   process answering `Unknown gensio type: serialdev,…`. gensio loads `serialdev` as a
   dynamic module; this happens when those modules are not where the library expects them.
   `GENSIO_LIBRARY_PATH` is the override, and on a distribution package it should never be
   needed.
