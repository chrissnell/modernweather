# The weather database: TimescaleDB on a dedicated node

`implementation-plan.md` M0-1. A single-replica `StatefulSet` of
`timescale/timescaledb-ha:pg17` on a `local` PersistentVolume, on a node tainted so nothing
else lands there. `docs/reconciliation.md` §7 has the reasoning; this file records the
decisions that are visible in the YAML without being explained by it.

These manifests are the definition of the instance. It was stood up by applying them, not by
hand, and a rebuild is the same three commands.

## Layout, and why there are two directories

```
base/          the deployment. Generic: no node name, no host path, no address.
site.example/  the shape of a site overlay. Committed, with placeholders.
site/          the real overlay. GITIGNORED.
```

A `local` PersistentVolume has to name **a node** and **a path on that node's disk**. Those
are facts about one cluster, they are worth nothing to anyone else, and this repository is
public — so they live in `site/`, which `.gitignore` keeps out of the tree. `base/` is the
part that ships.

The split is one object wide: `site/` holds the PersistentVolume and nothing else.
Everything structural — the namespace and its PodSecurity level, the StorageClass, the
StatefulSet, both Services, the PodDisruptionBudget — is generic and committed.

The trade is deliberate and worth naming: the reproducible *shape* is under version control,
the one site's *values* are not. If you want both, keep `site/` in a private repository or
alongside the cluster's own configuration. What must not happen is the volume being created
by hand with no record of it anywhere.

## Apply

```sh
cp -r deploy/timescaledb/site.example deploy/timescaledb/site
$EDITOR deploy/timescaledb/site/persistentvolume.yaml      # node, path, capacity

# Once, out of band: the one input the manifests do not carry.
kubectl apply -f deploy/timescaledb/base/00-namespace.yaml
kubectl -n modernweather create secret generic modernweather-db-superuser \
  --from-literal=password="$(openssl rand -base64 36 | tr -d '\n')"

kubectl apply -k deploy/timescaledb/site
```

Clients connect to `modernweather-db-rw.modernweather.svc.cluster.local:5432`, database
`modernweather`. The `modernweather-db` service is headless and exists to give the set its
stable pod name; it is not the one to point an application at.

`kubectl apply -k deploy/timescaledb/base` on its own is valid and leaves the claim
`Pending` until an overlay supplies a volume.

## What the node has to provide

From §7.4, which matched the hardware to the workload rather than guessing at it:

| | |
|---|---|
| Disk | **SSD or NVMe, not spinning, and not NFS.** The one hard constraint |
| Capacity | Nearly irrelevant — a decade of history is ~36 GB compressed |
| RAM | 16–32 GB is generous |
| CPU | 4–8 cores; single-thread speed matters more than count |
| Network | 1 GbE is ample |
| Taint / label | `workload-class=database:NoSchedule` and `workload-class=database` |
| Volume | mounted by the node's own configuration before Kubernetes sees it — on Talos, a user volume under `/var/mnt/` |

## fsync latency, which is the specification that matters

§7.3 records why: a durable aggregate refresh is dominated by WAL fsync (A-W4-3), so disk
*latency* is what consumes W4 D-26's 10-second refresh budget, and capacity is nearly
irrelevant.

**Measure it, and keep the numbers with the overlay.** `pg_test_fsync -s 3` from inside the
pod, against the mounted volume. One real datapoint, from a SATA SSD — not an NVMe, which is
the lower end of what §7.4 allows — for calibration, one 8 kB write:

| Method | µs/op |
|---|---|
| `open_datasync` | 807 |
| `fdatasync` — Linux's default, and this server's `wal_sync_method` | 795 |
| `fsync` | 1014 |
| `open_sync` | 1003 |
| non-sync'ed 8 kB write | 1 |

Reproducible to within 1% across two consecutive runs. **This is the baseline M5-1 will
judge W4 D-26's refresh deadline against. Nothing here is tuned off it** — see below for the
one place the server's configuration is not the image's default, and it is not a choice made
here.

## Decisions a reader of the YAML would otherwise have to guess at

**Image `timescale/timescaledb-ha:pg17`**, not the plain `timescale/timescaledb`, which
ships no `timescaledb_toolkit` — DESIGN.md D-2 requires it and W8 D-17 is the reason. It is
also the image CI uses, so CI and this instance share an image family.

**PodSecurity `restricted`, set explicitly.** A namespace that inherits the cluster default
is admitted at whatever that default happens to be and warns at it forever. This workload
needs nothing `baseline` grants: a `local` PV is mounted through a PVC, so PSA does not see a
`hostPath`, and the only privileged-looking thing wanted is a writable data directory, which
an `fsGroup` provides. `restricted` is therefore free, and it is cheaper to pick it now than
to discover later that something was relying on `baseline`.

**`fsGroup`, not an `initContainer`.** A Talos user volume is mounted `root:root 0755` and
the image runs as `postgres` (uid 1000), so a container writing straight to the mount gets
`EACCES`. `fsGroup: 1000` with `fsGroupChangePolicy: OnRootMismatch` has kubelet set the
group and the group-write bit once, on the empty volume, and skip the walk on every later
start — which matters because an unconditional recursive `chown` is a startup cost that grows
with the database. Verified: the volume root came up `root:postgres drwxrwsr-x` and `initdb`
ran to completion on an empty volume. No privileged container, nothing running as uid 0,
which is also what lets the namespace be `restricted`.

**`PGDATA` is a subdirectory of the mount**, `/var/lib/postgresql/pgdata`, matching the CI
pod and `contract/tools/rig/tsdb.yaml`. The mount root is a filesystem root with its own
contents; `initdb` wants a directory it owns.

**`--data-checksums` is the only `initdb` flag set.** It is detection, not tuning: a torn
page is otherwise silent, and checksums cannot be enabled later without rewriting the
cluster. Until M0-2 lands this volume is the only copy of the data, so silent corruption is
the failure worth paying for. Everything else is left at the image's defaults.

**The server's configuration is the image's, including a tuning pass this item did not ask
for.** `timescale/timescaledb-ha:pg17` runs `timescaledb-tune` from its own
`docker-entrypoint-initdb.d`, once, at first boot, sizing itself from the container's
limits — at 24 GiB and 12 CPUs that gives `shared_buffers = 6GB`,
`effective_cache_size = 18GB`, `max_worker_processes = 31`, `jit = off`. Recorded rather than
adjusted, and recorded because it is a consequence of the memory limit in
`base/40-statefulset.yaml`: the limit is the only knob this item turns, and it turns
`shared_buffers` with it. `wal_sync_method` is the server's default `fdatasync`, which the
table above measures.

**A `PodDisruptionBudget` with `minAvailable: 1`.** A single replica on a `local` volume
cannot move, so an eviction is not a failover — `kubectl drain` would otherwise take the
database down with no acknowledgement that it had. This refuses voluntary evictions until
somebody says otherwise. It does nothing about losing the node, which is M0-2's problem.

**`Retain` in three places** — the StorageClass, the PV, and the StatefulSet's PVC retention
policy — so that no ordinary `kubectl delete` can reach the data.

**`terminationGracePeriodSeconds: 300`.** The default 30 s can expire into a `SIGKILL` during
a shutdown checkpoint, which turns every slow stop into a crash recovery.

## What this is not

Not a second replica and not CloudNativePG; §7.4 closed both, and the owner accepted
explicitly that a `local` volume cannot reschedule. M0-2's off-site backup is the condition
that makes that acceptable, and it is a separate work item.

Not the CI database, which is a disposable `emptyDir` pod that exists so the database gates
can run while GitHub Actions is unavailable. The gates keep pointing at it; M5-1 is the first
thing that points here.

Not tuned, not backed up, and holding no data: at the end of M0-1 this database has the
schema at HEAD and zero rows.
