# Off-site backup and restore

**Produces:** `ci/db/offsite-backup.sh`, `ci/db/restore-rehearsal.sh`, the
`db-restore-rehearsal` gate, `xtask/tests/offsite_backup.rs`, and the measured dump sizes in
§4 — which replace the one extrapolated figure in `docs/reconciliation.md` §7.5.

`DESIGN.md` **D-6** says backup and restore is `pg_dump` plus an aggregate refresh and that no
bespoke tool is built. That was the right mechanism with nothing owning it. On `pitcairn` that
was latent; on a single-replica `local`-PV StatefulSet the volume *is* the single point of
failure, and the owner made off-site backup the condition of accepting one replica
(`docs/reconciliation.md` §7.5). `implementation-plan.md` **M0-2** is the schedule, the
destination, and — the half D-6 never had — the **rehearsal**.

The deliverable is not a dump. It is a **restore that has been performed and reconciled**,
because a backup you hesitate to restore from is not a backup.

---

## 1. What is in the archive, and what is deliberately not

> **The closed inventory fired for real within hours of M0-2 merging, which is the best
> evidence it could have had.** M6-3 added `alerts`, `snow_depth_estimates` and `snow_events`,
> and the next dump against a database at HEAD stopped with all three named rather than
> quietly omitting them from every archive until someone needed them. The fix was three
> declarations — one of which, `snow_depth_estimates`, needed somebody to work out that it is
> *not* derived data a refresh can rebuild.

The inventory is **closed**: every ordinary table in the weather database is named in
`RELATIONS` in `ci/db/offsite-backup.sh` with a mode and a reason, and **a table that exists
and is not named stops the dump**. That is not fussiness. "The legacy archive is the one that
gets forgotten" (§7.5) is a prediction about a future migration, and the only thing that beats
a prediction like that is a backup that refuses to run until somebody says what the new table
is for. `ci/db/offsite-backup.sh plan` prints the inventory and touches nothing.

| | Mode | Why |
|---|---|---|
| `observations` | one object **per calendar year** | Raw. Derived from nothing; D-4 retains it indefinitely |
| `observations_1d_legacy`, `observations_1h_legacy` | `pg_dump -Fc`, **schema and data** | The only copy of the ~294 days of roll-up with no raw behind them (W9 D-4). `migrations/` does **not** create these, so an archive without their schema has nowhere to put the rows |
| `stations` | full COPY | Raw's `station_id` means nothing without it |
| `station_metrics`, `almanac_cache`, `snow_totals_cache`, `forecasts`, `alerts` | full COPY | Projections and caches. Small enough that excluding them is not worth the thought; `alerts` refills from its upstream feed on its own |
| `snow_depth_estimates`, `snow_events` | full COPY | **Not regenerable by a refresh.** Each depth estimate carries the rate limit forward from the one before it, so the series is recursive over its own output and no `CREATE MATERIALIZED VIEW` can express it — and it is what the numbers `/snow` serves are computed from. `snow_events` is a different algorithm over the same series, not a second copy |
| `config.db` (SQLite) | verbatim | Station calibration, certificates, website configuration — the one thing a human cannot re-derive (D-66) |
| `observations_1m` / `_5m` / `_1h` / `_1d` | **not backed up** | D-15 makes them disposable, W4 D-5 bounds their retention, and they would be the largest thing in the archive. **A-W9-2 is why they could not be loaded back even if they were dumped** |
| `_sqlx_migrations`, `_mw_ci_applied` | **not backed up** | Migration ledgers. A restore applies `migrations/` itself; restoring a ledger would describe a history the restored database never had |

**The aggregate exclusion is worth stating as a size, not just a decision.** At the generated
width `observations_1m` alone is 8.8 GB/year (`docs/database-schema.md` §6, correcting D-18)
against raw's 19 GB/year — so backing up the levels would roughly double the archive to store
something that is a pure function of what is already in it.

### Two modes, because `observations` is a hypertable

`pg_dump -t observations` dumps an **empty parent**: the rows live in chunks under
`_timescaledb_internal`. There is no `pg_dump` invocation that produces one file per year, so
raw is dumped with `COPY (SELECT … WHERE time >= … AND time < …) TO STDOUT`, gzipped, with an
**explicit column list recorded in the manifest**. The compressor is the same zlib `pg_dump
-Fc` uses, at the same default level; what changes is the slicing, and the slicing is the
whole point of the per-year design. The legacy tables are plain tables and keep `pg_dump -Fc`,
which is also what carries their schema.

The explicit column list is not decoration. A positional `COPY` breaks the first time a
migration adds a column between dump and restore; a named list restores an older archive into
a newer schema for as long as no dumped column has been **dropped**, and the restore stops with
the column names when one has been.

---

## 2. Per year, because raw is append-only

No reading is ever updated, so a historical year cannot change. The archive holds one object
per year and **the weekly run re-dumps only the current year** — measured below at ~3.4 GB
rather than ~34 GB — and a restore of one year does not pull the other nine.

"Cannot change" is an assumption about the write path, so the dump **checks** it: every
historical year's row count is compared against the count the manifest recorded, and a year
that moved is re-dumped and reported loudly. That costs one `count(*)` per year per run, which
is a sequential scan of raw and is the entire cost of a weekly run above the current year's
upload. `MW_BACKUP_VERIFY_HISTORY=0` turns it off and records `history_verified: false` in the
manifest, because a weekly job that is cheaper than it looks should say so in the artefact.

**Measured, on the second consecutive run with nothing changed:** 2025 reported
`unchanged, and the existing object is still there` and 2026 was re-uploaded — 27.7 MB of
traffic instead of 110 MB, on a 1.38 M-row archive.

A row count is only half the question, and the other half is not skippable. A manifest
describes what was **written**; the rows it counts live in the database and are there whether
or not the object still is. So every historical year the run intends to skip also gets **one
HEAD against the archive** — is the object there, at the length recorded? A missing or
wrong-sized object re-dumps the year and says so loudly. §6 has the incident that put that
check in.

---

## 3. Restore, and the order it has to happen in

```sh
# 1. a database at HEAD. The restore does not create the schema.
createdb modernweather && psql -c 'CREATE EXTENSION timescaledb' modernweather
sqlx migrate run                       # or ci/db/migrate-twice.sh's applier

# 2. SUSPEND THE BACKGROUND JOBS. See the box below; this is not optional.
psql -c "SELECT alter_job(job_id, scheduled => false)
           FROM timescaledb_information.jobs
          WHERE proc_name IN ('policy_retention', 'policy_compression',
                              'policy_refresh_continuous_aggregate')" modernweather

# 3. the archive, newest year first
MW_BACKUP_REMOTE=r2:<bucket>/modernweather/db \
  ci/db/offsite-backup.sh restore --into modernweather

# 4. the charts, and ONLY the charts
DATABASE_URL=…/modernweather ci/db/rebuild-aggregates.sh --recent-only

# 5. serve. Then, behind the running service, the rest of history:
DATABASE_URL=…/modernweather ci/db/rebuild-aggregates.sh

# 6. put the jobs back
psql -c "SELECT alter_job(job_id, scheduled => true) FROM timescaledb_information.jobs
          WHERE proc_name IN ('policy_retention', 'policy_compression',
                              'policy_refresh_continuous_aggregate')" modernweather
```

`ci/db/restore-rehearsal.sh` is steps 1–4 and 6 with the reconciliation in the middle. It is
the procedure, not a description of it — which is why the runbook above is short: if these
steps drift, the gate goes red.

> ### Step 2 is the step people skip, and M0-1 measured what it costs
>
> **TimescaleDB runs a new policy's first job immediately** — measured at under a second after
> the migration that created it, with `next_start` a day out. `migrations/` installs three
> retention policies (`drop_after` 7 / 14 / 90 days), so a restore into a freshly migrated
> database is inside that window **by construction**: the job drops the chunk the rebuild has
> just materialised.
>
> **A re-refresh does not repair it.** It answers `already up-to-date` and the count stays
> zero, because the watermark reads as refreshed while the chunk is gone. That is the
> expensive part: the failure is silent and it is *later* than the mistake.
>
> `ci/db/rebuild-aggregates.sh` raises the same exposure as **N-21** and deliberately does not
> fix it there. M0-2 fixes it for the restore path, which is the path M0-2 owns.

### Step 4 before step 5 is W4 D-23, and it is the difference between a restore and an outage

A full rebuild at ten years is an hour-scale operation (A-W4-3). Phase 1 — recent ranges
first, bottom-up — makes every band `/span` can serve correct, measured at **0.87 s against
467 s** for the full materialisation. A restore runbook that blocks on step 5 is an outage
that did not need to be one, so **`--recent-only` comes before the service is restored and
the unbounded pass comes after it**, and `ci/db/restore-rehearsal.sh` does not run phase 2 at
all unless asked.

### Why the archive cannot just carry the aggregates — A-W9-2, re-measured every run

A continuous aggregate **accepts an `INSERT`**, reads the row back correctly, and **discards
it on the next refresh**. Pre-seeded aggregate rows are not a restore mechanism. The whole
exclusion in §1 rests on that, so the rehearsal does not cite it: it inserts a row into
`observations_1m` over a range with no raw behind it, refreshes, and **requires the row to be
gone**. If a pre-seeded row ever survives, the gate fails on purpose — the premise has changed
and the archive's contents are worth revisiting.

---

## 4. The measured dump size

§7.5's cost table carried one extrapolated figure — "~3.6 GB/year compressed", inferred from
the measured 19 GB/year uncompressed through an assumed `pg_dump -Fc` zlib ratio. This replaces
it with measurements.

Produced with `ci/db/measure/00-harness.sql` — the harness every number in
`docs/database-schema.md` §6 came from — against the real instance on the database node, 4
stations, 140 of 216 columns populated, dumped through this script. **Every compression figure
is quoted next to its hold rate**, following D-10 and §6: the consecutive-identical rate is
what explains a ratio, and a figure without it is not interpretable.

| Cadence | Hold rate | Rows | COPY text, B/row | **Stored, B/row** | Ratio |
|---|---|---|---|---|---|
| 2 s | **76.88%** | 1 036 800 | 871.98 | **80.18** | 10.88× |
| 2 s | 76.88% | 345 600 | 871.99 | 80.17 | 10.88× |
| 3 s | **65.31%** | 806 400 | 871.96 | **112.59** | 7.74× |
| 4 s | **53.84%** | 604 800 | 871.91 | **139.43** | 6.25× |
| 30 s | 6.30% | 345 600 | 871.67 | 228.13 | 3.82× |

Two things to read off it. The **COPY text width is 871.9 B/row and does not move** — not with
row count, not with hold rate, not between two independently dumped years with a 3× row-count
difference. And the **stored** size is almost entirely a function of the hold rate, which is
W0's spike result reproduced on a different compressor.

### Production

`docs/database-schema.md` §6's correction note measured production directly: **19 GB/year
uncompressed** and a hold rate of **59.6%**. That lands between the 3 s and 4 s rows above.

| | | |
|---|---|---|
| Production rows per year | **27.0 M** | derived: 19 GB/year ÷ §6's measured 703 B/row |
| Stored bytes per row at 59.6% | **~126 B** | interpolated between the 65.31% and 53.84% rows |
| **Stored per year** | **~3.4 GB** | 27.0 M × 126 B |
| **A decade at rest** | **~34 GB** | |
| Weekly upload | **~3.4 GB** | the current year only |
| R2 storage | **~$0.51/month**, ~$0.36 after the 10 GB-month free tier | at $0.015/GB-month |
| Egress | **free**, and this is still the reason to pick R2 | |

**§7.5's extrapolation was good.** It said ~3.6 GB/year and 40–50 GB for a decade against a
measured ~3.4 GB/year and ~34 GB — conservative by about a quarter, in the safe direction. No
decision moves. What changes is that the only remaining inference is **one named variable**,
the hold rate, instead of the whole chain; and the manifest records `rows`, `bytes` and
`stored_bytes` per year, so **the first weekly dump against real data replaces the
interpolation with production's own number** without anyone having to measure anything.

Two caveats, stated rather than buried. The harness is a *model* of held sensors, not
production's data — §6 says the same about every figure derived from it. And `gzip -6` over a
COPY stream is not bit-for-bit `pg_dump -Fc`; it is the same zlib at the same level over a
different serialisation, so the ratios transfer and the absolute bytes are this script's own.

---

## 5. Retention, and why nothing age-based may be applied to this bucket

The normal instinct for a backup bucket is "delete anything older than N days". **On this
archive that deletes the history and keeps only the present**, and it is worth being exact
about why rather than asserting it.

Raw is one object per calendar year and **a historical year is written once and never
rewritten** — that is what makes a weekly backup cost ~3.4 GB instead of ~34 GB. So a
historical year's `LastModified` is frozen and only ever gets older. Measured, two dumps a
minute apart with nothing changed in the database:

```
  692  17:29:38  raw/observations-2024.copy.gz   <- first dump, never touched again
  692  17:29:40  raw/observations-2025.copy.gz   <- first dump, never touched again
  699  17:31:08  raw/observations-2026.copy.gz   <- the current year, rewritten
   72  17:31:09  tables/stations.copy.gz         <- rewritten
    …  17:31:2x  tables/…, manifest/latest.json  <- rewritten
```

An age-based rule sees those first two lines getting older every week forever. At 90 days it
deletes every year but the current one.

| Object | Under an age rule |
|---|---|
| `raw/observations-<YEAR>.copy.gz`, historical | **deleted.** Nine tenths of a decade, and the part D-4 retains indefinitely |
| `tables/*`, `legacy/*`, `config/*`, `manifest/latest.json` | survive — **while the job is healthy.** That caveat is the whole problem |
| `manifest/<stamp>.json` | the only thing that genuinely accumulates: ~3 KB a week, ~160 KB a year |

**"While the job is healthy" is not a quibble.** A backup job that stops, plus an age-based
rule that does not, equals an archive that deletes itself — and `manifest/latest.json` is the
entry point `restore` reads, so it becomes *unreadable* before it becomes empty. §6 records
the bucket where exactly that appears to have happened.

### So the retention rule lives in the job, and covers one thing

`prune_manifests` in `ci/db/offsite-backup.sh` deletes timestamped manifests older than
`MW_BACKUP_MANIFEST_RETENTION_DAYS` (**default 90**; `0` keeps them forever). It is scoped
twice — pinned to the `manifest/` prefix *and* excluding `latest.json` by name — because
`delete` is the one verb here that destroys data and "this cannot reach the archive" should be
obvious to a reader rather than inferred. `xtask/tests/offsite_backup.rs` asserts both, and
both were watched failing.

Putting it here rather than in the object store is the point: **this code only runs when a
backup ran**, so it can never delete on a week when there was nothing new to replace what it
removed. A lifecycle rule has no such property.

It saves nothing measurable — 160 KB a year against a ~34 GB archive. Its value is that the
prefix does not grow without bound, and that the question "where is retention handled?" has
one answer.

### If you want a rule in Cloudflare anyway

Scope it to the `db/manifest/` prefix and nothing else, and know that it will delete
`latest.json` too if the weekly job ever stops for longer than the rule's window. The
in-job prune is strictly safer for the same outcome.

---

## 6. The destination

**A dedicated R2 bucket, `modernweather-backups`**, with a bucket-scoped token that can reach
nothing else. `kube-cluster-helm-values` → `apps/modernweather-db/backup/README.md` has the
exact Cloudflare and Vault steps; the requirements, because three of them are load-bearing:

| | |
|---|---|
| Bucket | `modernweather-backups`, **private** — no public access, no custom domain |
| Token | **Object Read & Write**, scoped to that bucket alone. The jobs use `GetObject`, `PutObject` and `ListBucket`; nothing needs bucket-level admin |
| **No lifecycle / expiry rule** | **The per-year design cannot survive one** — §5 measures why. Retention is handled in the job instead |
| **No object versioning** | The current year is overwritten weekly. With versioning on, storage grows without bound for no benefit |
| `RCLONE_CONFIG_R2_NO_CHECK_BUCKET=true` | so the job never calls `CreateBucket`/`HeadBucket` and the token needs no permission to |

**An earlier revision of this document specified a prefix inside the bucket that holds the
Vault raft snapshots.** That was not a preference — it was the only destination the *existing*
credential could reach, because that token is bucket-scoped and cannot create a bucket. It
carried a real cost (one leaked token would expose both backup sets) and the owner declined it.
A dedicated bucket was always the better answer and it is a configuration change, not a
redesign: one Vault path, two values, no manifest edit.

### What that near-miss taught the design, which is the part worth keeping

The shared bucket was checked before it was used, and the check found something unrelated and
worse: **the bucket held zero objects.** The Vault snapshot job's last successful upload was
2026-09-09 — it had been failing on `vault operator raft snapshot save` for over two months —
and every snapshot it had ever written was gone from the bucket as well.

Nothing in the first draft of this design would have noticed the same thing happening here. The
per-year skip trusts the previous manifest, and **a manifest describes what was written, not
what survived**: the row-count check compares counts of rows *in the database*, which are
unaffected by an object disappearing from the archive. A lifecycle rule, a mis-aimed
`rclone purge`, or anything else with delete permission would have quietly reduced a decade to
one year, with every weekly run reporting success.

So `history_object_intact` in `ci/db/offsite-backup.sh` asks the second question: **is the
object a skipped year is relying on still there, at the length the manifest recorded?** One
HEAD per historical year per week. It is *not* covered by `MW_BACKUP_VERIFY_HISTORY=0` — that
flag exists to skip the sequential scan, and the cheap check is the one that catches history
disappearing. A missing object re-dumps the year and says so loudly; a length that disagrees
with the manifest also re-dumps, because an object of the wrong size is not the object the
recorded digest was computed over.

### The credential, which is three fields and no config file

Vault holds it at `secret/modernweather` (KV v2) as `accessKeyId`, `secretAccessKey` and
`endpoint`, and an `ExternalSecret` against the existing `vault` `ClusterSecretStore` — the
pattern every other credential in this cluster uses — projects those three into the namespace.

**There is no `rclone.conf` anywhere.** rclone takes a whole remote from the environment as
`RCLONE_CONFIG_<REMOTE>_<KEY>`, so the CronJobs set `RCLONE_CONFIG_R2_ACCESS_KEY_ID` and
friends and the remote `r2:` exists with no file to write. That matters for one specific
reason: assembling a config file in a shell means a secret passing through a heredoc, and a
quoting bug there is invisible until a restore. The bucket name is a plain env var in the two
CronJobs — a bucket name is not a secret, and a value that lives in git is one you can read in
a review.

A fourth field, `apiToken`, is deliberately **not** pulled in. It is a token for Cloudflare's
own API rather than for S3; nothing here can use it, and a credential a workload cannot use is
a privilege it should not be holding.

The manifests — the weekly dump `CronJob`, the monthly `--from-archive` rehearsal `CronJob` and the
`ExternalSecret` — are in `kube-cluster-helm-values` under `apps/modernweather-db/backup/`, not
here: this repository is public and `/deploy/` is in `.gitignore` for that reason.

## 7. The gate, and what each half of it proves

`ci/gates.toml` declares **`db-restore-rehearsal`**, armed by `ci/db/offsite-backup.sh`'s
existence. It has two modes and it always prints which one it ran.

| | What it proves | Where it runs |
|---|---|---|
| default (round-trip) | the archive layout, the per-year slicing, the explicit column lists, the byte-for-byte reconciliation, A-W9-2, and that phase 1 alone makes every `/span` band correct — through a **local** rclone remote, with no credential | every PR, including from a fork |
| `--from-archive` | the same, against the **real R2 archive** | a monthly `CronJob` beside the cluster |

**Why the PR half is the one without the credential.** A gate that is red on every PR because
a secret is unavailable gets overridden until nobody reads it, which is where
`docs/reconciliation.md` **G-1** ended up by a different route. So the half that can always
run runs always, never claims to have talked to R2, and the half that needs a credential runs
on a schedule where its failure is visible on its own.

**Reconciliation is byte equality, not a row count.** The manifest records the sha256 of the
*ordered* COPY stream each object was produced from; the rehearsal re-runs the identical
ordered query against the restored database and compares digests. A row count cannot see a
value that came back as `0` instead of `NULL`, which is exactly the class of defect
`db-tests` exists for and exactly the kind of thing a restore is capable of on its own. The
`pg_dump`-mode tables are the one exception — a `pg_dump` container is not byte-stable, so
those are compared on row count and the output says so rather than presenting it as the same
check.

### Both halves have been seen failing

`docs/reconciliation.md` G-1's standing rule is that a check nobody has watched fail proves
nothing.

| Injected | Result |
|---|---|
| one row dropped from the restore stream | both raw years reported `MISMATCH` with the recorded and observed digests, **exit 1** |
| `LAYOUT_VERSION` bumped against an existing archive | the restore refused to run and nothing was restored, **exit 2** — no verdict, which is the correct classification |
| an aggregate level moved from `skip` to `copy` | `xtask/tests/offsite_backup.rs` failed, naming the level |
| the undeclared-relation check neutered | six of the seven tests in that file failed |
| an archive object deleted while the manifest still claims it | the year was re-dumped and `NOT IN THE ARCHIVE` reported — and still reported with `MW_BACKUP_VERIFY_HISTORY=0` |

`xtask/tests/offsite_backup.rs` drives the dump's *plan* with `PSQL`, `PG_DUMP` and `RCLONE`
pointed at stubs, so the decisions the rehearsal cannot check — a table nobody declared stops
the dump; the aggregate levels are never written; an unchanged historical year is not
re-uploaded; a historical year **missing from the archive** is — are checked with no database
and no network. A live database at HEAD has no undeclared table in it, and a healthy archive
has no missing object in it, which is precisely why those checks need tests rather than runs.

---

## 8. Known gaps

- **`config.db` is not in the archive yet, and the dump says so on every run.** There is no
  daemon and therefore no configuration database; `MW_BACKUP_ALLOW_NO_CONFIG_DB=1` in the
  `CronJob` tolerates that and the manifest records `config_db.state`. Without the flag the
  dump **fails**, which is deliberate: a tiny irreplaceable file is exactly the thing that gets
  left out quietly. The flag comes out when the daemon owns a `config.db` path.
- **The legacy archive tables do not exist until M5-1.** They are declared, reported `ABSENT`
  on every run, and recorded in the manifest's `absent` list, so their arrival needs no change
  here — and their silent *disappearance* from a future archive would show up as a difference
  in that list.
- **N-21 is still open.** Suspending the background jobs is implemented for the restore path
  only. A plain `ci/db/rebuild-aggregates.sh` run still has the same exposure.
- **Retention covers the manifests and nothing else, and that is the decision rather than an
  omission.** There is no "delete backups older than N days" here because there is nothing to
  delete: the archive is one current copy of an append-only dataset, not a rolling set of full
  backups, and its oldest object is its most irreplaceable one. §5.
- **The archive's own integrity is checked by presence and length, not by digest.** Verifying
  a historical year's *content* means downloading and re-hashing it — free in egress on R2, but
  a decade a week. The weekly run checks that each object is there and the right size; the
  monthly `--from-archive` rehearsal is what actually reads one back and compares digests.
  Nothing re-hashes every historical year on a schedule, and that is a deliberate gap rather
  than an oversight.
