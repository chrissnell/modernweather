# The weather database: TimescaleDB on `lil-bulky`

`implementation-plan.md` M0-1. A single-replica `StatefulSet` of
`timescale/timescaledb-ha:pg17` on a `local` PersistentVolume, on a Talos worker tainted so
nothing else lands there. `docs/reconciliation.md` §7 has the reasoning; this file records
what was built, what was measured on the real hardware, and the decisions that are visible
in the manifests but not explained by them.

These manifests are the definition of the instance. It was stood up by applying them, not
by hand, and a rebuild is the same two commands.

## Apply

```sh
# Once, out of band: the one input the manifests do not carry.
kubectl apply -f deploy/timescaledb/00-namespace.yaml
kubectl -n modernweather create secret generic modernweather-db-superuser \
  --from-literal=password="$(openssl rand -base64 36 | tr -d '\n')"

kubectl apply -k deploy/timescaledb
```

Clients connect to `modernweather-db-rw.modernweather.svc.cluster.local:5432`, database
`modernweather`. The `modernweather-db` service is headless and exists to give the set its
stable pod name; it is not the one to point an application at.

## The node

| | |
|---|---|
| Node | `lil-bulky`, `10.50.0.57`, bare metal, `Ready` |
| Talos / kubelet | v1.12.4 / v1.35.0 — every other worker is v1.13.4 / v1.36.1 |
| CPU / RAM | 12 cores / 32 GiB |
| Taint / label | `workload-class=database:NoSchedule`, `workload-class=database`, both Talos-owned |
| Data volume | `/var/mnt/modernweather-db` — `/dev/sda7`, `xfs`, 350 GiB, mounted by Talos |
| Backing disk | Samsung SSD 850, SATA, `rotational=0` |

The version skew is inside the supported window but it is a real difference from the rest
of the cluster, and it is the first thing to suspect if this node behaves unlike the others.
Nothing in this item hit it.

## fsync latency, which is the specification that matters

§7.3 records why: a durable aggregate refresh is dominated by WAL fsync (A-W4-3), so disk
*latency* is what consumes W4 D-26's 10-second refresh budget, and capacity is nearly
irrelevant — a decade of history is ~36 GB compressed against 350 GiB of volume.

`pg_test_fsync -s 3`, run inside the pod against the mounted volume (one 8 kB write):

| Method | Before the StatefulSet, via a bare `local` PV | On the running instance |
|---|---|---|
| `open_datasync` | 2207 µs/op | **807 µs/op** |
| `fdatasync` (Linux default, and the server's `wal_sync_method`) | 2767 µs/op | **795 µs/op** |
| `fsync` | 5725 µs/op | **1014 µs/op** |
| `open_sync` | 5542 µs/op | **1003 µs/op** |
| non-sync'ed 8 kB write | 1 µs/op | 1 µs/op |

Same device, same filesystem, same mount. The second column is reproducible — two
consecutive runs agreed to within 1% (807/795 then 811/803 µs). The first column was taken
before this StatefulSet existed and is left in because the difference is real and
unexplained, not because it is reconciled. **This is the baseline M5-1 will judge W4 D-26's
refresh deadline against. Nothing here is tuned off it** — see the next section for the one
place the server's configuration is not the image's default, and it is not a choice made
here.

## Decisions a reader of the YAML would otherwise have to guess at

**PodSecurity `restricted`, set explicitly.** The cluster default is `baseline` enforce /
`restricted` warn, so an unlabelled namespace is admitted and warned at forever — which is
the state `mw-ci` is in. This workload needs nothing `baseline` grants: a `local` PV is
mounted through a PVC, so PSA does not see a `hostPath`, and the only privileged-looking
thing wanted is a writable data directory, which an `fsGroup` provides. `restricted` is
therefore free, and it is cheaper to pick it now than to discover later that something was
relying on `baseline`.

**`fsGroup`, not an `initContainer`.** Talos mounts the volume root `root:root 0755` and the
image runs as `postgres` (uid 1000), so a container writing straight to the mount gets
`EACCES`. `fsGroup: 1000` with `fsGroupChangePolicy: OnRootMismatch` has kubelet set the
group and the group-write bit once, on the empty volume, and skip the walk on every later
start — which matters because an unconditional recursive `chown` is a startup cost that
grows with the database. Verified: the volume root came up `root:postgres drwxrwsr-x` and
`initdb` ran to completion on an empty volume. No privileged container, nothing running as
uid 0, which is also what lets the namespace be `restricted`.

**`PGDATA` is a subdirectory of the mount**, `/var/lib/postgresql/pgdata`, matching the CI
pod and `contract/tools/rig/tsdb.yaml`. The mount root is a filesystem root with its own
contents; `initdb` wants a directory it owns.

**`--data-checksums` is the only `initdb` flag set.** It is detection, not tuning: a torn
page is otherwise silent, and checksums cannot be enabled later without rewriting the
cluster. Until M0-2 lands this volume is the only copy of the data, so silent corruption is
the failure worth paying for. Everything else is left at the image's defaults.

**The server's configuration is the image's, including a tuning pass this item did not
ask for.** `timescale/timescaledb-ha:pg17` runs `timescaledb-tune` from its own
`docker-entrypoint-initdb.d`, once, at first boot, sizing itself from the container's
limits — here 24 GiB and 12 CPUs, giving `shared_buffers = 6GB`,
`effective_cache_size = 18GB`, `max_worker_processes = 31`, `jit = off`. Recorded rather
than adjusted, and recorded because it is a consequence of the memory limit in
`40-statefulset.yaml`: the limit is the only knob this item turns, and it turns
`shared_buffers` with it. `wal_sync_method` is the server's default `fdatasync`, which the
table above measures. The CI pod got the same treatment at its own 4 GiB limit, so CI and
this instance differ in the tuner's inputs rather than in whether it ran.

**A `PodDisruptionBudget` with `minAvailable: 1`.** A single replica on a `local` volume
cannot move, so an eviction is not a failover — `kubectl drain` would otherwise take the
database down with no acknowledgement that it had. This refuses voluntary evictions until
somebody says otherwise. It does nothing about losing the node, which is M0-2's problem.

**`Retain` in three places** — the StorageClass, the PV, and the StatefulSet's PVC
retention policy — so that no ordinary `kubectl delete` can reach the data.

**`terminationGracePeriodSeconds: 300`.** The default 30 s can expire into a `SIGKILL`
during a shutdown checkpoint, which turns every slow stop into a crash recovery.

## What this is not

Not a second replica and not CloudNativePG; §7.4 closed both, and the owner accepted
explicitly that a `local` volume cannot reschedule. M0-2's off-site backup is the condition
that makes that acceptable, and it is a separate work item.

Not the CI database. `mw-ci-timescaledb.mw-ci.svc.cluster.local` is a disposable `emptyDir`
pod on `farm-4` that exists so the database gates can run while GitHub Actions is
unavailable. The gates keep pointing at it; M5-1 is the first thing that points here.

Not tuned, not backed up, and holding no data: at the end of M0-1 this database has the
schema at HEAD and zero rows.
