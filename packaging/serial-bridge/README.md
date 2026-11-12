# A serial multiplexer for the Campbell station

One serial line has one reader. This directory makes it several, so that `remoteweather` and
`modernweather` can read the same datalogger **at the same time** — which is what
[`docs/implementation-plan.md`](../../docs/implementation-plan.md) M5-2's mechanism (b) needs
on this station and cannot get any other way. For the WeatherLink Live the same thing is
free: it broadcasts over UDP and both daemons can listen. For a Campbell on a serial port
there is exactly one reader, so without this there is no (b) for it at all.

It is also `docs/reconciliation.md` **N-20**'s production path, arriving a milestone early:
the bridge owns the line and both daemons speak TCP, which is what the Rust driver's
`crates/mw-station/src/stream.rs` was designed around and what the Go driver already does
when `serial_device` is empty.

| | |
|---|---|
| Runs on | pitcairn (`10.50.0.35`), Arch, `extra/ser2net` |
| Configuration | [`ser2net.yaml`](ser2net.yaml) → `/etc/ser2net/campbell.yaml` |
| Unit | [`campbell-serial-bridge.service`](campbell-serial-bridge.service) → `/etc/systemd/system/` |
| `remoteweather` change | [`cutover.sh`](cutover.sh), which writes its own rollback first |
| Proof | `crates/mw-station/tests/serial_bridge.rs`, run by `ci/serial-bridge/prove.sh` |

**This directory is the only place pitcairn's bridge configuration is tracked.** pitcairn is
a hand-maintained box and nothing on it is under version control; `remoteweather`'s own
repository packages the daemon and not the machine. So these files live here, with the
programme that needs them, rather than only on the box. If a `kube-cluster-helm-values`
manifest ever fronts this — it would not, since the bridge terminates a physical cable — that
half belongs there and this half still belongs here.

## What was measured, and what is still assumed

Everything in this table is asserted by `crates/mw-station/tests/serial_bridge.rs` against a
real `ser2net` over a real character device, with a `socat` pty pair standing in for the
station. **Two versions**: 4.6.4 (Debian trixie) and 4.6.8, which is what Arch ships and
therefore what pitcairn will run.

| | Measured |
|---|---|
| **It broadcasts** | two clients connected at once receive the identical byte stream, asserted byte for byte over 200 records — not a shared prefix, the whole stream. This is the question the item existed to answer: a multiplexer that handed bytes to whichever client read first would leave *both* daemons with gap-ridden data that still parses, which nothing downstream could tell from weather |
| **The control bites** | the same apparatus with `max-connections: 1` fails that assertion, so it is sensitive to the configuration it is proving |
| **A fourth client cannot displace a daemon** | over the limit, a client is answered `Port already in use` and closed; the clients already reading do not notice. `kickolduser` would change that and is deliberately not set |
| **One daemon wedging does not starve the other** | a client that stops reading entirely costs the other nothing across 6,000 records — over three hours at this station's output period |
| **The bridge owns the line** | `ser2net` sets `TIOCEXCL`, so once it holds the device a stray serial reader gets `EBUSY` and retries loudly rather than silently taking half the stream |
| **Two readers of one device do split it** | the negative control for the above, and the reason the cutover order below is what it is |
| **`modernweather` reads it today** | the real `StreamStation` + `CampbellAscii` driver admits one reading per record through the bridge while a second client reads the same bytes |
| **An absent device is not a failure to start** | `ser2net` starts, listens, and opens the device when a client arrives. With the cable out, a client is answered `Device open failure: …`; when the device returns, the next client works with no restart. A replug is a non-event |

Still assumed, because the box is the only thing that can answer it:

- **which device, and at what rate** — see the block at the top of `ser2net.yaml`. The issue
  says `/dev/ttyS0`; `crates/mw-station/src/stream.rs` records the same station as USB
  serial. One of those is wrong. The rate is the `baud` column of the station's row in
  `remoteweather`'s configuration database, and a wrong rate does **not** produce silence —
  the bytes arrive framed wrong and every line fails to parse.
- **what else is on `10.50.0.35:7001`** — nothing is expected to be, and it has not been
  checked from the box.

## The one new hazard

`ser2net` is bidirectional: bytes a client sends are forwarded to the datalogger. **Neither
daemon writes to this line** — `remoteweather`'s Campbell driver contains no `.Write(` at
all, and `mw-station`'s session carries
`mw_station::lines::tests::the_session_writes_nothing` as its assertion — so the fan-out
cannot make the two fight over the port. But before this change the only thing that could write to the station
was a file descriptor `remoteweather` held; after it, anything that can reach the TCP port
can. The accepter is `tcp` rather than `telnet`, so there is no RFC 2217 negotiation and a
client cannot change the line rate. The rest is pitcairn's firewall. `ser2net`'s own
`remaddr` option cannot express it: it takes an `<address>,<port>` pair and no CIDR, which
was tried.

## Cutover

**The live site must keep serving throughout, and it will — with one short gap.** The moment
the bridge takes the device, `remoteweather` has to have let go of it. One process owns a
serial port.

**The order below is not caution, it is a measured requirement.** `remoteweather`'s serial
library (`github.com/tarm/goserial`) sets neither `TIOCEXCL` nor `O_EXCL` nor a `flock`, so
if the bridge is started while `remoteweather` still holds `/dev/ttyS0`, the bridge's
`open(2)` **succeeds** and the kernel then hands each byte to whichever of them reads first.
There is no error anywhere to notice. Doing it in this order makes that impossible, because
by the time the bridge opens the device nothing else has it.

Have the rollback in hand before stopping anything — step 2 writes it.

```sh
# 1. Install and stage. Nothing is live yet; `remoteweather` still owns the port.
pacman -S ser2net
useradd --system --no-create-home --shell /usr/bin/nologin ser2net     # if absent
ls -l /dev/ttyS0                                                      # which group owns it:
                                                                      # Arch says uucp, Debian dialout.
                                                                      # Drop the other from the unit.
install -d /etc/ser2net
install -m 0644 ser2net.yaml /etc/ser2net/campbell.yaml               # device + baud CONFIRMED first
install -m 0644 campbell-serial-bridge.service /etc/systemd/system/
systemctl daemon-reload

# 2. Write the rollback, then move the station onto TCP. Still nothing restarted.
./cutover.sh plan                     # read this
./cutover.sh apply                    # writes ./rollback.sql from the row as it was

# 3. THE GAP STARTS HERE. remoteweather lets go of the serial port and starts dialling.
systemctl restart remoteweather
#    It will log `could not connect to 10.50.0.35:7001` and retry every 5 s. Expected.

# 4. THE GAP ENDS HERE.
systemctl enable --now campbell-serial-bridge
```

The gap is bounded by `remoteweather`'s own retry — `connectToNetworkStation` loops with a
5 s sleep — so it costs a few readings and not a restart. **Do this once**, not repeatedly
while iterating: iterate against a `socat` pty and `ci/serial-bridge/prove.sh`, which needs
no hardware.

## Verify, by value and not by liveness

```sh
systemctl status campbell-serial-bridge
journalctl -u remoteweather -n 20          # expect `connecting to 10.50.0.35:7001` and nothing after it

# A reading landed in the last few minutes. This is the check that matters: a process that
# is up and a station that is reporting are different claims.
psql -c "SELECT max(time), now() - max(time) AS age FROM weather;"

# And the served value, which is what a visitor sees.
curl -s https://<the site>/latest | python3 -m json.tool | head
```

Then, and only then, point `modernweather` at the same `10.50.0.35:7001` with Campbell's
transport as `tcp_out` and watch both read it at once.

## Rollback

```sh
systemctl disable --now campbell-serial-bridge   # the bridge must let go FIRST
sqlite3 /var/lib/remoteweather/config.db < rollback.sql
systemctl restart remoteweather
```

Same rule in reverse, and for the same reason: if `remoteweather` goes back to the serial
port while the bridge still has it, they both read it. Stopping the bridge first makes that
impossible.

**Never leave the station with no reader.** If the bridge will not start, the rollback above
is three commands and takes seconds; run it rather than debugging with the station dark.

## If the bridge starts but delivers nothing

In order of likelihood:

1. **Wrong line rate.** Every line fails to parse and the station looks connected and
   silent. `stty -F /dev/ttyS0` shows what the bridge set; the station's `baud` column shows
   what it should be.
2. **A stale uucp lock**, if the bridge was ever killed with `SIGKILL`. gensio takes two
   locks per device — one keyed by the path, one by its `major.minor` — and only removes
   them on a clean exit. The symptom is a client answered `Device open failure: Object was
   already in use` by a process that looks healthy. `ls /run/lock/LCK..*`, and remove the
   ones naming this device once nothing is running. This is why the unit stops with
   `SIGTERM`.
3. **Something else has the device.** `fuser -v /dev/ttyS0`. If it is `remoteweather`, its
   `serial_device` column is not empty — `./cutover.sh plan` shows the row.
4. **`ser2net` is listening but every connector is broken**, which looks like a healthy
   process answering `Unknown gensio type: serialdev,…`. gensio loads `serialdev` as a
   dynamic module; this happens when those modules are not where the library expects them.
   `GENSIO_LIBRARY_PATH` is the override, and on a distribution package it should never be
   needed.
