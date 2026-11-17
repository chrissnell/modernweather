# The ModernWeather Helm chart

Installs the daemon and the TimescaleDB it writes to. Nothing has to be copied into a pod
and no SQLite file has to be seeded by hand, which is what the arrangement this chart
replaces needed.

```
helm upgrade --install modernweather packaging/helm/modernweather \
  --namespace modernweather --create-namespace \
  -f values-site.yaml
```

`values-example.yaml` is a complete, renderable set of values with nothing real in it.
Copy it, fill it in, and keep the copy **out of this repository** — see below.

---

## Four decisions, and why

### 1. The chart is in this repository; the values are not

`kube-cluster-helm-values` is the obvious home, and `.gitignore` here says why it is not:
`/deploy/` is ignored because those manifests "name nodes, host paths and addresses, and
this repository is public". A **chart** names none of those. Its **values** do.

So the split is along that line rather than along repository boundaries:

| | Where | Why |
|---|---|---|
| The chart | `packaging/helm/modernweather`, here | It is a build artefact of this repository. It deploys the image this repository's release workflow pushes, and a template that outlives its binary is a template that drifts |
| `values-site.yaml` — `lil-bulky`, `/var/mnt/modernweather-db`, `preview.suncrestweather.com`, `10.50.0.221` | `kube-cluster-helm-values` | Exactly the content `/deploy/` is ignored for |

`.helmignore` excludes `values-site*.yaml`, so a copy left in this directory cannot be
packaged into a chart tarball by accident. It is not a substitute for not committing it.

### 2. The database is a vendored subchart, because the official chart is unmaintained

**The official chart was the first choice and it was checked, not assumed.**
`timescale/helm-charts` exists and `helm repo add timescale https://charts.timescale.com`
still resolves, so the answer is not "they do not have one". The answer is that it is
retired:

| | |
|---|---|
| Upstream repository | `timescale/helm-charts`, **`"archived": true`** |
| Its last commit | 2024-01-18, and the commit is titled **"Unmaintained note in README"** |
| The README's first line | ⚠ **"This project is no longer maintained."** |
| Last `timescaledb-single` release | **0.33.1, 2023-01-26** |
| Its default image | `timescale/timescaledb-ha:pg14.6-ts2.9.1-p1` — **PostgreSQL 14.6, TimescaleDB 2.9.1** |
| Its default topology | `replicaCount: 3`, Patroni-managed HA |
| Its data volume | `volumeClaimTemplate` named `storage-volume` |

Each of the last three is a problem on its own. This deployment is PG17 / TimescaleDB
2.31, and the chart's templates — `patroni_callback.sh`, `pgbackrest_bootstrap.sh`,
`tstune.sh` — are written against the 2023 generation of `timescaledb-docker-ha`, so
running pg17 through them means overriding the image out from under the scripts that
configure it. Three Patroni replicas is the shape `docs/reconciliation.md` §7.3 rejected
and §7.4 recorded the owner rejecting. And `storage-volume` is the one that settles it: a
StatefulSet derives its claim name from the volume's, so the existing
`data-modernweather-db-0` — with 350 GiB of observations in it — would be bound to nothing
and an empty volume provisioned beside it.

**The part of it worth having is pgBackRest, and we do not need the chart to get it.** Its
`backup.pgBackRest` block does support a custom `repo1-s3-endpoint`, so R2 would work. But
the existing arrangement already backs up to R2 and does something pgBackRest does not:
`ci/db/offsite-backup.sh` keeps a **closed inventory** — a table that exists and is not
declared stops the dump — and it is per-year, so a weekly run re-dumps only the current
year. A physical-backup tool has no view of that. So the backups came here as they are
(below), rather than being replaced by a feature of a chart that is no longer maintained.

So: vendored subchart. `charts/timescaledb/Chart.yaml` carries the rest of the argument —
**single replica on a `local` PersistentVolume pinned to one machine**, because the
workload is fsync-latency bound (§7.4), and it **cannot reschedule**, which the owner
accepted conditional on off-site backups (§7.5); `timescaledb-ha` for `timescaledb_toolkit`
(D-2); CloudNativePG considered and declined in §7.4.

### 2a. The backups, which the chart now owns

The first pass left the backup CronJobs in `kube-cluster-helm-values`. They are here now,
because "set the database up the way we like it" includes them.

| | |
|---|---|
| `backup.enabled` | **`true` by default.** §7.5 makes off-site backups the condition that makes a single replica safe, so a chart that defaulted them off would default to the configuration the owner declined. `backup.enabled: false` is how you decline it deliberately |
| Mechanism | `ci/db/offsite-backup.sh` — `pg_dump` plus an aggregate refresh on restore, per D-6. No bespoke tool |
| The script | `files/offsite-backup.sh` is a **symlink** to `ci/db/offsite-backup.sh`. Helm follows it and inlines the content when packaging, so the repository holds one copy and there is no second to drift |
| Backup job | Mounts that script from a ConfigMap, because it must not depend on GitHub being up at 04:00 on a Sunday |
| Rehearsal job | **Fetches `main` at run time, deliberately.** Its question is whether *today's* restore path can read the archive you have; a pinned script freezes that question |
| Required | `backup.bucket` and `backup.credentials.existingSecret` (keys `accessKeyId`, `secretAccessKey`, `endpoint` — the endpoint is in the Secret because it contains the account id) |

What the subchart still does **not** own: the `ExternalSecret` that populates that
credential. It belongs to the cluster's secret plumbing, not to this chart
(`docs/backup-and-restore.md` §4).

### 3. Configuration is not chart values

The chart has no `stations:` or `websites:` block, and that is deliberate rather than
unfinished.

The daemon's configuration lives in a SQLite database it creates and migrates itself, and
the management API is the thing that writes it. There is no environment variable and no
flag for a station, a website or the weather-database URL — `crates/modernweather/src/args.rs`
is the whole command line. A chart that wanted to seed those rows would have to write
SQLite from a template, which is what the arrangement this replaces did: an Alpine init
container that ran `apk add sqlite` under a namespace enforcing `restricted` Pod Security,
and crash-looped 392 times.

So the chart owns the deployment and the management UI owns the configuration. `config.db`
is on a retained PersistentVolumeClaim because it holds station calibration, certificates
and website configuration — `docs/reconciliation.md` §7.5 lists it among the things a human
cannot re-derive.

The one place this is felt is first install, which takes three steps instead of zero. The
chart's `NOTES.txt` prints them.

### 4. `setupListen` is required for a first install, and stops mattering afterwards

The management API binds `127.0.0.1` until an administrator account exists (W5 §3.4,
A-W5-3). In a container that means nothing can reach it: the image is `FROM scratch`, so
there is no shell to `kubectl exec` into and no loopback client to run there.

`setupListen: "0.0.0.0"` renders `--setup-listen 0.0.0.0`, which widens that window
deliberately and makes the daemon log a `WARN` saying so. **It is ignored the moment an
account exists.** From then on the bind address is `listener.management_listen_addr` in the
configuration database, whose default is `127.0.0.1` — so set that to `0.0.0.0` in the
management UI during the first session, or the UI becomes unreachable until you reset the
password.

---

## Adopting the objects that were applied by hand

The names this chart renders are the names that are already in the cluster, which is not a
coincidence — it is what makes adoption possible rather than requiring a 350 GiB copy. With
the release named `modernweather`:

| Rendered | Existing |
|---|---|
| `Service/modernweather`, `PVC/modernweather` | same |
| `Deployment/modernweather` | same name, but **deleted and re-created rather than adopted** — step 2 |
| `CronJob/modernweather-db-backup`, `CronJob/modernweather-db-restore-rehearsal` | same |
| `StatefulSet/modernweather-db`, `Service/modernweather-db`, `Service/modernweather-db-rw` | same |
| `StorageClass/modernweather-db-local`, `PV/modernweather-db-lil-bulky` | same |

The StatefulSet name is the load-bearing one: its `volumeClaimTemplate` derives
`data-modernweather-db-0`, so a differently named StatefulSet would leave the observations
bound to nothing and provision an empty volume beside them.

**1. Check that the chart agrees with what is running.** This is a read-only server-side
diff, and for the database it should come back with label changes and no spec changes at
all:

```
helm template modernweather packaging/helm/modernweather -n modernweather -f values-site.yaml \
  | kubectl diff -n modernweather -f -
```

**2. Delete `Deployment/modernweather`. Do not adopt it.**

```
kubectl delete -n modernweather deployment/modernweather
```

Adoption is a three-way merge against the chart's manifest, and on a first install Helm has
no previous manifest of its own to diff against — so a field that is in the LIVE object and
absent from the chart's is not removed. It is kept. For every other object here that is
harmless, because the chart renders what is already there. For this one it is fatal: the
hand-applied Deployment carries the `seed-config` init container and its `seed` volume, and
adopting it installs the chart *with the crash loop the chart exists to delete* —
`apk add sqlite` as non-root under `restricted` Pod Security, `Init:CrashLoopBackOff`, and a
`--wait` that burns its whole timeout before saying so.

Deleting it costs nothing and is **first** for a second reason: it is what releases
`PVC/modernweather`, which is `ReadWriteOnce` and which step 3 has to mount. The Deployment
holds no state of its own — `config.db` and the spool live on that claim, which is adopted
and is annotated `helm.sh/resource-policy: keep`.

Step 1's `kubectl diff` does not show this class of problem: a diff of live against the
chart's manifest shows what the chart would ADD, and what bites here is what it would fail
to take away. Any other object applied by hand with fields the chart does not render wants
the same treatment.

**3. Move the daemon's state off the preview's volume, THEN remove the preview.**

`pvc/mw-preview` does **not** hold "a kubectl-cp'd binary only", which is what this file
used to say and is the one sentence in it that could have cost something. It holds
`config.db` — the administrator credential, the node identity, the station, the website and
the weather-database URL — and `spool/`, which holds the writer cursor and any readings not
yet in TimescaleDB. Deleting it before copying is how you arrive at an unclaimed instance
with no station and a re-read spool.

Nothing here is a re-type: the file is carried across as it is, so the values an operator
corrected through the management UI survive the move, and so does their password.

**The daemon has to be stopped first, and gracefully.** `config.db` is `journal_mode = WAL`
(`crates/mw-store/src/config_db.rs`), so a copy of the main file alone is only complete once
the last writer has closed and checkpointed it. A `scale --replicas=0` is a SIGTERM and the
daemon checkpoints on its way out; a `delete pod --force` is not, and would leave committed
rows in a `config.db-wal` this copy does not take — silently, because `PRAGMA
integrity_check` on the truncated main file still answers `ok`.

```
kubectl -n modernweather scale deployment/mw-preview --replicas=0
kubectl -n modernweather wait --for=delete pod -l app=mw-preview --timeout=120s
```

The label is the hand-applied Deployment's own selector (`app: mw-preview`), not one of the
chart's — check it against the live object rather than against this repository, which does
not contain the manifest that set it. A `wait` with a selector that matches nothing returns
success immediately, and the next step would then copy from under a running daemon.

```
# A pod that mounts both claims. Both are ReadWriteOnce, so one pod is also the only
# way to have them attached at the same time.
kubectl -n modernweather run mw-transplant --image=busybox:1.37 --restart=Never \
  --overrides='{"spec":{"securityContext":{"runAsNonRoot":true,"runAsUser":65532,
  "runAsGroup":65532,"fsGroup":65532,"seccompProfile":{"type":"RuntimeDefault"}},
  "containers":[{"name":"staging","image":"busybox:1.37","command":["sleep","1800"],
  "securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,
  "capabilities":{"drop":["ALL"]}},"volumeMounts":[{"name":"src","mountPath":"/src"},
  {"name":"dest","mountPath":"/dest"}]}],"volumes":[{"name":"src",
  "persistentVolumeClaim":{"claimName":"mw-preview"}},{"name":"dest",
  "persistentVolumeClaim":{"claimName":"modernweather"}}]}}'
kubectl -n modernweather wait --for=condition=Ready pod/mw-transplant --timeout=120s

# The destination must be empty. `cp -a /src/spool /dest` nests to /dest/spool/spool if a
# spool is already there, and a config.db already there is a DIFFERENT daemon's identity --
# in both cases stop and decide, rather than merging two states.
kubectl -n modernweather exec mw-transplant -- sh -c \
  'ls -A /dest | grep -q . && { echo "destination is not empty:"; ls -la /dest; exit 1; }; :'

kubectl -n modernweather exec mw-transplant -- cp -a /src/spool /dest/
```

`node.spool_path` has to be rewritten on the way across, and that is not cosmetic: the
preview mounted its volume at `/data`, the chart mounts it at `dir(configPath)` —
`/var/lib/modernweather` — and the daemon container has `readOnlyRootFilesystem: true`, so
a daemon still pointed at `/data/spool` cannot create it and does not start.

Check the copy BEFORE writing to it — an `integrity_check` run after the `UPDATE` is a
check of a file you have already mutated — and gate on the answer rather than reading past
it:

```
kubectl -n modernweather cp mw-transplant:/src/config.db ./config.db
# No -wal or -shm came across, so an `ok` here means the main file is self-contained.
[ "$(sqlite3 ./config.db 'PRAGMA integrity_check;')" = ok ] || { echo "STOP"; exit 1; }
sqlite3 ./config.db "UPDATE node SET spool_path = '/var/lib/modernweather/spool';"
sqlite3 ./config.db 'SELECT spool_path FROM node;'

kubectl -n modernweather cp ./config.db mw-transplant:/dest/config.db
kubectl -n modernweather delete pod mw-transplant --wait=true
```

Only now is the preview removable. A `LoadBalancer` Service cannot take an address another
Service holds, so this is before the install and not after:

```
kubectl delete -n modernweather deployment/mw-preview service/mw-preview
# AFTER the release is verified -- it is the rollback until then, and it still holds the
# binary the preview ran.
kubectl delete -n modernweather pvc/mw-preview
```

**4. Let Helm take ownership of the objects that stay.** Helm refuses to adopt a resource
it did not create unless it is labelled as its own:

```
# Every name here must EXIST and must not be one step 2 deleted: `kubectl annotate` of a
# missing object fails, and under `set -e` that abandons the loop with the remaining
# objects still unowned -- which the install then refuses on.
#
# `deployment/modernweather` is absent because step 2 deleted it; Helm creates it.
# `serviceaccount/modernweather` and `configmap/modernweather-db-backup-scripts` are absent
# because the chart renders names nothing applied by hand. The two CronJobs ARE here: they
# were applied from `kube-cluster-helm-values` before the subchart took them over.
for kind_name in \
  service/modernweather pvc/modernweather \
  statefulset/modernweather-db service/modernweather-db service/modernweather-db-rw \
  cronjob/modernweather-db-backup cronjob/modernweather-db-restore-rehearsal \
  ingress/modernweather
do
  kubectl -n modernweather annotate --overwrite "$kind_name" \
    meta.helm.sh/release-name=modernweather meta.helm.sh/release-namespace=modernweather
  kubectl -n modernweather label --overwrite "$kind_name" app.kubernetes.io/managed-by=Helm
done

# Cluster-scoped, so no -n.
for kind_name in storageclass/modernweather-db-local persistentvolume/modernweather-db-lil-bulky
do
  kubectl annotate --overwrite "$kind_name" \
    meta.helm.sh/release-name=modernweather meta.helm.sh/release-namespace=modernweather
  kubectl label --overwrite "$kind_name" app.kubernetes.io/managed-by=Helm
done
```

**5. Install**, then verify by request rather than by pod status — `/latest`,
`/almanac?station=<name>`, `/span/1h?station=<name>` and the management UI on `:8081` all
answer, and `mw_spool_cursor_skipped_envelopes_total` on `/metrics` is `0`, which is what
says the transplanted writer cursor was read rather than re-read.

Then delete the `modernweather-config-seed-*` ConfigMap and the hash-suffixed
`modernweather-db-backup-script-*` ConfigMap the subchart's
`modernweather-db-backup-scripts` replaces — nothing references either any more. (Step 2
took the old ReplicaSets with the Deployment, so there are none left to clean up.)

### What never gets deleted

`helm.sh/resource-policy: keep` is on the PersistentVolume, the StorageClass and the
daemon's PersistentVolumeClaim, and the StatefulSet carries
`persistentVolumeClaimRetentionPolicy: Retain` on both `whenDeleted` and `whenScaled`. A
`helm uninstall`, or a failed upgrade that Helm rolls back, leaves every volume in place.
The cost is that re-creating them later needs a deliberate `kubectl delete` first; that is
the right way round for the only copy of a decade of observations.

---

## Values worth knowing about

| Value | Default | Note |
|---|---|---|
| `image.tag` | `.Chart.AppVersion` | Prefer `sha-<short>`, which the release workflow publishes for every build and never moves. `latest` moves only on a tag push; `0.0.0-dev` is overwritten by the next rehearsal |
| `image.digest` | — | Wins over `tag` |
| `setupListen` | `""` | `0.0.0.0` for a first install. See decision 4 |
| `service.type` | `ClusterIP` | `LoadBalancer` + `service.loadBalancerIP` for a MetalLB address. One Service does both jobs; there is no second object for an Ingress backend to drift away from |
| `service.grpc.enabled` | `false` | The collector uplink and peer enrolment. A Service port in front of an unbound socket hangs rather than refusing, so it is off until there is a peer |
| `containerPorts.*` | 8080 / 8081 / 50051 | Must match the `listener` row in the configuration database. The chart cannot write that row, so changing a port here means changing it in the management UI too |
| `timescaledb.enabled` | `true` | `false` points the daemon at a database elsewhere — still configured through the management UI |
| `timescaledb.auth.existingSecret` | — | **Required.** The chart does not generate a superuser password: a generated one would rotate on every `helm upgrade` unless the chart read back its own Secret, and this database holds a decade of observations |
| `timescaledb.persistence.local.node` | — | **Required.** The one machine the database can run on |
| `timescaledb.persistence.local.path` | — | **Required.** The directory on that machine. A Talos user volume under `/var/mnt` |

`replicas` is not a value on either workload, and both refusals are in the templates with
their reasons: two daemons cannot share a SQLite database over a `ReadWriteOnce` volume,
and a second database replica on a `local` volume is a pod that stays `Pending`.
