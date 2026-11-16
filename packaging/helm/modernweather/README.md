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

### 2. The database is a vendored subchart, not a dependency on somebody else's

`charts/timescaledb/Chart.yaml` carries the full argument. In short: the deployment is a
**single replica on a `local` PersistentVolume pinned to one machine**, because the
workload is fsync-latency bound (`docs/reconciliation.md` §7.4), and it **cannot
reschedule** — which the owner accepted explicitly, conditional on off-site backups
(§7.5). The image must be `timescale/timescaledb-ha` for `timescaledb_toolkit` (D-2).
CloudNativePG was already considered and declined in §7.4.

An upstream chart would have to be overridden on every one of those points, and overriding
an upstream chart's image is the override that drops its init scripts, its `pg_hba`
templating and its exporter into a combination nobody upstream tests. There are no
decisions left to inherit, so there is nothing to depend on.

What the subchart does **not** own: the weekly backup CronJob and its `ExternalSecret`.
Those hold R2 credentials and stay in `kube-cluster-helm-values` under
`apps/modernweather-db/backup/` (`docs/backup-and-restore.md` §4).

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
| `Deployment/modernweather`, `Service/modernweather`, `PVC/modernweather` | same |
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

**2. Remove the preview, which this chart replaces.** A `LoadBalancer` Service cannot take
an address another Service holds, so this is before the install and not after:

```
kubectl delete -n modernweather deployment/mw-preview service/mw-preview
kubectl delete -n modernweather pvc/mw-preview          # holds a kubectl-cp'd binary only
```

**3. Let Helm take ownership of the objects that stay.** Helm refuses to adopt a resource
it did not create unless it is labelled as its own:

```
for kind_name in \
  deployment/modernweather service/modernweather pvc/modernweather serviceaccount/modernweather \
  statefulset/modernweather-db service/modernweather-db service/modernweather-db-rw \
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

**4. Install.** Then delete `Deployment/modernweather`'s old ReplicaSets and the
`modernweather-config-seed-*` ConfigMap, which nothing references any more.

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
