# CI gate and release pipeline

> **This work item writes configuration, not only prose.** The gate in
> `.github/workflows/pr.yml` runs; the target matrix in `.github/workflows/release.yml` is
> real; `ci/spike/` is a re-runnable measurement rather than a claim. Where something is
> not yet exercised, this document says so in those words.

**Disagreement with the W8 prompt, flagged at the top as the protocol requires.** The
prompt's §2 asks for "the union-view assertions W4 defines". There are none to define:
`DESIGN.md` D-5 rejects the versioned-aggregate and union-view scheme outright, because raw
data is retained indefinitely (D-4) and so no history is trapped in an old aggregate
version to union over. Carrying on under the existing decision, the migration gate asserts
the equivalent property on the design as decided — the two migration paths produce the same
schema, the same continuous-aggregate definitions, and the same data — which is strictly
stronger than comparing union-view text. See §2 and the proposed amendment A-1 at the end.

---

## 0. What exists after this work item

| Path | What it is |
|---|---|
| `.github/workflows/pr.yml` | the PR gate; the single required check is `gate` |
| `.github/workflows/platform.yml` | cross-compilation and the ARMv6 answer, re-measured |
| `.github/workflows/release.yml` | tag-driven release; publishes nothing until configured |
| `ci/gates.toml` | the deferred-gate manifest, and the mechanism that arms each one |
| `ci/release.toml` | the target matrix, read by both `cargo xtask` and the workflow |
| `ci/sql/assert-timescaledb-version.sql` | D-42's server-version assertion |
| `ci/db/migrate-twice.sh` | apply migrations from empty and from the previous release |
| `ci/spike/probe/` | the platform probe: the non-pure-Rust parts of the crate stack |
| `ci/spike/armv6-cross-spike.sh` | builds the probe for a target and runs it under qemu |
| `xtask/` | every gate and release step, runnable locally with the same command |
| `packaging/` | systemd unit, sysusers, deb/rpm scriptlets, Arch PKGBUILD, Dockerfile |
| `Cargo.toml`, `rust-toolchain.toml`, `rustfmt.toml` | the workspace the gate builds |

The workspace contains one crate, `xtask`. **No application crate was created**, because
`DESIGN.md` R-4 gives the crate and workspace layout to W3 §1. The root manifest uses a
`crates/*` glob so W3 can add crates without editing it.

---

## 1. The PR gate

### Jobs

| Job | Command | Notes |
|---|---|---|
| `fmt` | `cargo fmt --all --check` | 45 files in the Go tree fail `gofmt`; this is why |
| `clippy` | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | |
| `build` | `cargo build --workspace --all-targets --all-features --locked` | |
| `test` | `cargo test --workspace --all-features --locked` | |
| `race` | `cargo test … -Zsanitizer=thread -Z build-std` | pinned nightly |
| `loom` | `cargo test` with `RUSTFLAGS=--cfg loom` | stable |
| `gates` | `cargo xtask gates` | prints the deferred set into the run summary |
| `codegen-drift` | `cargo xtask gate codegen-drift` | plus an assertion that the tree is clean |
| `conformance` | `cargo xtask gate conformance` | W1 arms it |
| `driver-emulators` | `cargo xtask gate driver-emulators` | W2a arms it |
| `database` | version assertion + `cargo xtask gate db-migrate-twice` | TimescaleDB service container |
| `coverage` | `cargo xtask gate coverage-ratchet` | a ratchet, not a threshold |
| `gate` | checks that all of the above succeeded | **the only required check** |

Every one of these is a `cargo xtask` subcommand or a one-line `cargo` invocation on
purpose: a gate that cannot be reproduced locally gets worked around rather than fixed.

### `--locked` everywhere

A PR that changes a dependency has to commit the `Cargo.lock` change, because that is the
only way a reviewer sees it. Without `--locked`, CI silently resolves a new version and the
build that was tested is not the build that was reviewed.

### Required checks: exactly one

`gate` is the single entry in branch protection. It `needs:` every other job and fails if
any of them is anything other than `success` — including `skipped` and `cancelled`, which
GitHub's own aggregation would let through. Adding a job means adding it to one `needs:`
list, not editing a repository setting that nobody remembers exists.

### Push versus pull request

| Event | What runs |
|---|---|
| `pull_request` | the whole gate |
| `push` to `main` | the whole gate |
| `merge_group` | the whole gate |
| `pull_request` touching `Cargo.lock`, `rust-toolchain.toml`, `ci/release.toml`, `ci/spike/**` | the gate, plus `platform.yml` |
| `push` to `main` | plus `platform.yml` |
| weekly schedule | `platform.yml` |
| tag `v*` | `release.yml` |

**Cross-compilation is not in the required set, and that is a decision rather than an
omission.** Five `cross` builds pull five container images and cost on the order of ten
minutes. Paying that on every PR to answer a question that can only change when the
dependency graph, the toolchain pin or the target matrix changes is the wrong trade — so
`platform.yml` triggers on exactly those paths, on every push to `main`, and weekly. The
weekly run exists because the answer can also change without any commit at all: a caret
requirement picks up a new transitive version that ships new assembly.

Nothing can regress the ARMv6 answer without either touching one of those paths or a
dependency publishing an update, and both cases are covered.

### Concurrency

In-flight runs for the same PR are superseded. Runs on `main` are never cancelled, because
the last green commit on `main` is the thing a release is cut from.

### The nightly pin

`race` is the only job that uses nightly, because ThreadSanitizer needs `-Zsanitizer` and an
instrumented `std`. It is pinned (`nightly-2026-10-01`), in one place, next to the stable
pin. **When a nightly regression breaks it, bump the pin in the same PR — do not disable the
job.** Both data races `DESIGN.md` records (D-28, D-29) survived the life of the Go project
precisely because nothing looked.

`loom` complements it rather than duplicating it: loom is exhaustive over a model of the
interleavings, TSan observes one real run. W3 §8's concurrency tests will want both.

### Deferred gates, and why they cannot stay silent

W8 lands before W1, W2, W2a and W4, so four of the gate's checks have no subject yet. A
check that simply skips in that situation is **worse than no check**, because it keeps
reporting green after its subject arrives.

`ci/gates.toml` declares each such check with the path whose existence arms it, and
`cargo xtask gate <id>` reads that file rather than a human's memory:

```
$ cargo xtask gates
codegen-drift     DEFERRED  arms when metrics/registry.toml exists  (W2 (metric registry))
conformance       DEFERRED  arms when tests/conformance/manifest.toml exists  (W1)
db-migrate-twice  DEFERRED  arms when migrations exists  (W4 (database schema))
driver-emulators  DEFERRED  arms when tests/emulators/manifest.toml exists  (W2a §4a)
coverage-ratchet  ARMED     arms when ci/coverage-floor.txt exists  (W8)
```

Three properties make this work:

1. **Nobody has to remember.** The work item that creates `metrics/registry.toml` arms the
   drift gate by doing so.
2. **It fails closed.** If a gate's command does not exist when the gate arms, `xtask`
   fails and names the owner from the manifest. A gate cannot arm into a no-op.
3. **It is visible.** The `gates` job writes the table into every run's summary, so the
   deferred set is on the PR rather than in someone's head.

### The drift check may not write to the tree

`DESIGN.md` R-2: the check regenerates into a temporary location and diffs, because a check
that fixes the problem it is checking for always passes. `xtask codegen --check` generates
into `target/codegen-check/`, and the `codegen-drift` job additionally asserts
`git status --porcelain` is empty afterwards. The contract with W2 is three paths, all in
one file (`xtask/src/codegen.rs`):

| | |
|---|---|
| registry | `metrics/registry.toml` |
| generator | `cargo run -p mw-codegen -- --registry <file> --out <dir>` |
| committed output | `generated/` |

W2 may rename any of them; that file is the only thing that changes with it.

### Coverage is a ratchet

`ci/coverage-floor.txt` holds a percentage that can only go up. `cargo xtask coverage
--check` fails if measured line coverage is below it by more than 0.5 points; `--bump`
raises it. **CI never commits the bump** — the floor is raised by hand in the PR that earns
it, both because this pipeline writes nothing back to the repository (§5) and because a
floor that moves on its own is not a reviewed decision.

The prompt for this work item is explicit that a fixed coverage target invites tests written
to hit a number. A floor that cannot decrease has no number to aim at; it only forbids going
backwards. The floor starts at 0.00, which is the honest reading of a repository whose only
crate is `xtask`.

---

## 2. Tests that need a real database

`pr.yml`'s `database` job runs `timescale/timescaledb-ha:pg17` as a service container. The
`-ha` image, not the plain one: `DESIGN.md` D-2 requires `timescaledb_toolkit`, which the
plain `timescale/timescaledb` images do not carry, and CI running a server the design does
not specify would make three of the six load-bearing spike answers untested.

**Two things the job does.**

**The version assertion (D-42).** `ci/sql/assert-timescaledb-version.sql` fails the build on
PostgreSQL < 17 or TimescaleDB < 2.31, and warns when the Toolkit is absent. It compares
versions as `int[]` rather than as text, so 2.9.0 does not sort above 2.31.0 — a mistake
that would make the assertion pass on exactly the servers it exists to reject.

*Measured:* run against `timescale/timescaledb-ha:pg17` (PostgreSQL 17.11, TimescaleDB
2.31.0, Toolkit 1.26.0): passes, and with the required version raised to 9.9.9 it fails with
`ERROR: timescaledb 2.31.0 is older than the required 9.9.9`. Both directions checked.

**Migrations applied twice.** `ci/db/migrate-twice.sh`:

```
path A   empty database            -> every migration at HEAD
path B   previous release's schema -> seeded rows -> every migration at HEAD
```

Then it asserts three things are identical between A and B: the `pg_dump --schema-only`
output, the `timescaledb_information.continuous_aggregates` rows including each view's
definition, and — on path B — that the seeded rows are still there.

Three details that are load-bearing rather than incidental:

- **Path B must apply only the delta.** A loop that re-applies every file in filename order
  dies on `relation already exists` and tells you nothing. The script keeps a ledger table
  and matches `sqlx-migrate` on the two properties that matter here: each migration applied
  exactly once, in filename order, inside a transaction.
- **The previous schema comes from git, not from a committed dump.** `git describe --tags`
  finds the previous release and `git archive` extracts its `migrations/`, so the baseline
  cannot go stale. `MW_PREVIOUS_MIGRATIONS_DIR` overrides it for local use.
- **TimescaleDB's own schemas are excluded from the comparison, not filtered out of it.**
  Path B has chunks because it has rows; path A does not. Chunks are not schema. Excluding
  `_timescaledb_*` is the difference between a meaningful comparison and a permanent red.

*Measured:* run against a live `timescale/timescaledb-ha:pg17` with a two-migration fixture
(hypertable plus continuous aggregate, then `ADD COLUMN`) and 600 seeded rows.
Path A and path B produced identical schemas and identical aggregate definitions, the
seeded rows survived, and the script exited 0. Adding a table to the *previous* set only —
the exact shape of "a fresh install and an upgraded install differ" — made it exit 1 and
print the offending `CREATE TABLE legacy_rollup` lines. Both directions checked.

W4 arms this gate by creating `migrations/`, and owns `ci/db/seed.sql` and
`ci/db/verify-seed.sql`. Until then the gate reports itself deferred.

---

## 3. Hardware-free driver tests

The four Go emulators stay in Go (`DESIGN.md` D-40) and are fixed external test
infrastructure. Two of them — `davis-emulator` and `campbell-emulator` — are plain TCP
listeners (ports 22222 and 8123 by default), so CI needs no serial hardware and no pty
bridge for them:

```
go run github.com/chrissnell/remoteweather/cmd/davis-emulator -port 22222 &
cargo test -p <driver crate> --test davis_emulator   # connects to 127.0.0.1:22222
```

The `driver-emulators` job installs a Go toolchain alongside Rust for exactly this, and
calls `cargo xtask emulator-tests`, which W2a §4a implements: check the reference repository
out **at a pinned commit**, build the emulators, start them, run the driver tests against
them, and shut them down.

Three things this design insists on:

1. **A pinned commit, not a branch.** The reference repository is read-only to this program
   but it is not frozen. A moving emulator turns an unrelated upstream commit into a red
   PR here.
2. **The emulator is the oracle, and it is deliberately not ours.** `davis-emulator`
   already models flaky hardware — dropped bytes, corrupted CRCs, truncated packets, hangs,
   slow and absent responses, all with tunable rates. A Rust re-implementation written by
   whoever wrote the driver would test that author's understanding against itself. Keeping
   the emulator in Go buys cross-implementation validation for nothing.
3. **Fault injection runs with a fixed seed.** The emulator's flaky modes are probabilistic;
   a gate that fails one run in twenty gets ignored within a week. W2a should drive the
   rates explicitly per test rather than enabling `-flaky` and hoping.

The serial-framed drivers (Davis over serial, Campbell over serial, KISS TNC over serial)
need a pty pair — `socat PTY,link=/tmp/ttyV0 PTY,link=/tmp/ttyV1` with the emulator on one
end — which works on a GitHub runner. W2a owns whether that is worth doing per driver or
whether the serial framing is tested at the codec level instead.

---

## 4. The release pipeline, and the ARMv6 answer

### The headline: ARMv6 works, and the obvious way of doing it does not

**Every Linux target is a statically linked musl build.** This is not a style preference; it
is the only configuration measured to work on ARMv6.

| Target | Toolchain | Builds | Tagged | Runs on an ARM1176 |
|---|---|---|---|---|
| `arm-unknown-linux-gnueabihf` | Debian armhf glibc cross | yes | **ARMv7-A** | **no — SIGILL** |
| `arm-unknown-linux-musleabihf` | musl armv6 cross, `+crt-static` | yes | ARMv6 | **yes** |
| `armv7-unknown-linux-musleabihf` | musl armv7 cross, `+crt-static` | yes | ARMv7 | yes (Cortex-A7) |
| `aarch64-unknown-linux-musl` | musl aarch64 cross, `+crt-static` | yes | — | yes (Cortex-A53) |
| `x86_64-unknown-linux-musl` | musl x86_64 cross, `+crt-static` | yes | — | yes (native) |
| `x86_64-pc-windows-gnu` | Debian mingw-w64 | yes | — | not executed |

Full numbers, commands and provenance are in **Appendix A**. The short version of the first
row: Debian's `gcc-arm-linux-gnueabihf` links an ARMv6 Rust target perfectly happily, and
the result is a binary whose merged `Tag_CPU_arch` is `v7` — because Debian's armhf glibc,
`crt1.o` and `libgcc` are all built for ARMv7-A — which dies with an illegal instruction on
the CPU in a Raspberry Pi 1 or Zero and runs fine on a Pi 2. **"It cross-compiled" is not an
answer to this question.** That is why `ci/spike/armv6-cross-spike.sh` ends by executing the
binary under `qemu-arm -cpu arm1176`, and why that run, not the build, is the verdict.

### This does not feed back into the SQLite driver choice

The prompt flagged that `sqlx`'s SQLite backend links `libsqlite3-sys` with bundled C, and
asked whether the armv6 answer should feed back into W2/W4's choice of driver. **It should
not.** `cc-rs` passes `-march=armv6 -marm -mfpu=vfp` for this target by itself; `sqlite3.c`
compiles to ARMv6 objects, and the resulting binary executes real SQLite work — create,
1,000 inserts, an aggregate read, `sqlite_version()` — on an emulated ARM1176. The C in the
SQLite path is a non-issue. D-32's `sqlx` choice stands, and no amendment is proposed.

The real ARMv6 risk in the chosen stack is the **TLS crypto provider**, which the planning
did not mention: `ring` and `aws-lc-rs` are hand-written assembly, not portable C. Both were
measured, both work, and both bring a wrinkle described in Appendix A §A.3.

### Target matrix and what happens to each

| Target | Produced | Notes |
|---|---|---|
| linux/amd64 | tar.gz, deb, rpm, container | |
| linux/arm64 | tar.gz, deb, rpm, container | Raspberry Pi 3 and later |
| linux/armv7 | tar.gz, deb, rpm, container | Raspberry Pi 2 |
| linux/armv6 | tar.gz, deb, rpm, container | Raspberry Pi 1 / Zero / Zero W |
| windows/amd64 | zip, **build only** | OQ-3: whether this ships at all is the owner's call |
| Docker image | multi-arch manifest over the four Linux platforms | **no QEMU** |
| deb / rpm | `cargo-deb`, `cargo-generate-rpm` | plus systemd unit and sysusers |
| Arch package | PKGBUILD in-tree, version-checked by CI, **published by hand** | no armv6: Arch Linux ARM has no ARMv6 port |

Nothing in the Go matrix is silently dropped. Two are consciously narrowed, with reasons:

- **Windows is built but not shipped on CI's word.** Nothing in the capability inventory
  needs it; the station drivers are serial, TCP, UDP and HTTP, and the Go build only ever
  produced it with `CGO_ENABLED=0`. This pipeline can cross-compile it (measured) but cannot
  execute it, so "it compiles" is the honest extent of the claim. OQ-3.
- **The Arch package drops armv6**, because Arch Linux ARM has no ARMv6 port. The Pi Zero is
  served by the deb, the rpm and the tarball.

### Docker without QEMU

The Go pipeline built its multi-arch images with `buildx` plus QEMU, which is slow and a
source of flakes. Here the binaries are already cross-compiled static executables, so
nothing in the image build executes target code: `packaging/docker/Dockerfile` is
`FROM scratch` and copies `${TARGETPLATFORM}/modernweather` out of a staged context.
Emulation is not needed because nothing runs.

A `scratch` image needs three things the Go Alpine image got for free, and all three are
copied in from a builder stage: CA certificates (outbound TLS to Aeris, PWS Weather,
Weather Underground and APRS-IS), the zoneinfo database (`chrono` reads
`/usr/share/zoneinfo`), and an `/etc/passwd` entry so uid 65532 has a name. The uid matches
the Go image so an existing bind-mounted data directory keeps working.

### `cargo-dist` versus `cross` versus hand-rolled — committed to `cross`

- **`cargo-dist` is rejected.** It is good at the targets and installers it covers, and it
  covers neither ARMv6 nor ARMv7, and it does not produce deb, rpm, Arch packages or a
  multi-arch container manifest. Adopting it would mean running two release systems, with
  the one that handles the Raspberry Pi targets — the ones that actually matter here — being
  the hand-rolled half.
- **Hand-rolled toolchains are rejected as the primary path.** This is what Appendix A
  measured with (musl cross toolchains fetched from `musl.cc`), and it works, but a release
  pipeline that depends on a single-maintainer download host breaks when that host does.
- **`cross` is the choice.** It carries maintained, versioned container images for all five
  targets including the awkward ARMv6 one, and `cross build --target <triple>` is the whole
  invocation. `cargo xtask dist` calls it; `MW_BUILDER=cargo` overrides it for a developer
  building the host target without Docker.

Packages come from `cargo-deb` and `cargo-generate-rpm`, which read their configuration
from the daemon crate's `[package.metadata]` — §6 gives the blocks to paste.

---

## 5. What the pipeline must not do

**Publishing is off until the owner turns it on.** The Go pipeline pushed to `ghcr.io` and
created a GitHub Release on every tag. That behaviour is re-established deliberately, not
inherited: three repository variables switch the three surfaces on independently, and unset
means off.

| Variable | Effect when `true` |
|---|---|
| `MW_PUBLISH_RELEASE` | create the GitHub Release and upload the artefacts |
| `MW_PUBLISH_CONTAINERS` | push images and the multi-arch manifest to `ghcr.io` |
| `MW_SIGN_ARTIFACTS` | cosign keyless signature over the checksum file |

With all three unset, a tag still builds the entire matrix and attaches it to the workflow
run, so the release is rehearsable before it is publishable. `workflow_dispatch` defaults
`dry_run` to `true`, so a rehearsal cannot publish because someone forgot a checkbox. The
`config` job prints the resolved switches into the run summary — a release that published
nothing says so at the top of its own log.

Two related choices:

- **CI has no write access to the repository.** `permissions: contents: read` at the
  workflow level; the `publish` job raises it to `contents: write` only when it is enabled.
  This is also why the coverage floor is bumped by hand (§1).
- **The AUR is published by hand.** Pushing there needs the maintainer's SSH key. What CI
  does instead is refuse to let `packaging/arch/PKGBUILD`'s `pkgver` fall out of step with
  the tag being released.
- **Signing signs the checksum file, not each artefact.** The checksums cover the artefacts,
  so one keyless signature over that file is a complete chain and a verifier has one thing
  to check.

---

## 6. What the next work items have to do

Short, because each item is one block or one file.

**W3, when `crates/modernweather` lands**, add to its `Cargo.toml`:

```toml
[package.metadata.deb]
maintainer = "Chris Snell <chris.snell@gmail.com>"
depends = ""                      # statically linked; no shared-library dependencies
section = "net"
priority = "optional"
assets = [
  ["target/release/modernweather", "usr/bin/", "755"],
  ["../../packaging/systemd/modernweather.service", "usr/lib/systemd/system/", "644"],
  ["../../packaging/sysusers/modernweather.conf", "usr/lib/sysusers.d/", "644"],
]
conf-files = ["/var/lib/modernweather/config.db"]
maintainer-scripts = "../../packaging/deb"

[package.metadata.generate-rpm]
assets = [
  { source = "target/release/modernweather", dest = "/usr/bin/modernweather", mode = "755" },
  { source = "../../packaging/systemd/modernweather.service", dest = "/usr/lib/systemd/system/modernweather.service", mode = "644" },
  { source = "../../packaging/sysusers/modernweather.conf", dest = "/usr/lib/sysusers.d/modernweather.conf", mode = "644" },
]
pre_install_script = "../../packaging/rpm/preinstall.sh"
post_install_script = "../../packaging/rpm/postinstall.sh"
pre_uninstall_script = "../../packaging/rpm/preremove.sh"
```

W3 also owns two things the systemd unit assumes and this work item could not decide:

1. **The config flag is spelled `--config <path>`** in `packaging/systemd/modernweather.service`
   and `packaging/docker/Dockerfile`. If W3 chooses otherwise, those two lines change.
2. **The unit is `Type=notify`**, so the daemon is expected to signal readiness once its
   components have started. The Go unit was `Type=simple`, which made "started" and "about
   to fail" indistinguishable to systemd. If W3 does not implement readiness notification,
   the unit must go back to `Type=simple` — leaving `Type=notify` with no notification makes
   the service time out and get killed.

**W2**, when `metrics/registry.toml` lands, the drift gate arms itself. Provide a
`mw-codegen` binary taking `--registry <file> --out <dir>` and commit its output to
`generated/`, or change the three constants at the top of `xtask/src/codegen.rs`.

**W1**, when `tests/conformance/manifest.toml` lands, the conformance gate arms itself and
calls `cargo xtask conformance --target-binary <path>`. Implement that subcommand; a gate
whose command does not exist fails loudly and names you (that is deliberate). The Go-binary
run belongs in `platform.yml` or a scheduled workflow rather than the PR gate — it validates
the fixtures, not the PR.

**W4**, when `migrations/` lands, the migration gate arms itself. Provide `ci/db/seed.sql`
and `ci/db/verify-seed.sql`. The seed should include at least one compressed chunk and one
continuous aggregate with rows in it, because those are the cases where a migration behaves
differently from an empty-table run.

**W2a**, when `tests/emulators/manifest.toml` lands, implement `cargo xtask emulator-tests`
per §3, with a pinned reference commit and deterministic fault-injection rates.

---

## Appendix A — the ARMv6 measurement

Everything here was measured, with the commands shown. The script that reproduces it is
`ci/spike/armv6-cross-spike.sh`, and `platform.yml` re-runs it weekly, so these numbers do
not have to be trusted — they can be re-taken.

### A.0 Rig

| | |
|---|---|
| Host | x86_64 Linux, 8 vCPU, Debian 13 container |
| Rust | 1.96.0 (`ac68faa20`, 2026-05-25) |
| Cross toolchains | Debian `gcc-14-arm-linux-gnueabihf` 14.2.0; `musl.cc` gcc 11.2.1 for armv6/armv7/aarch64/x86_64 musl; Debian `gcc-mingw-w64-x86-64` 14.2.0 |
| Emulator | qemu-user 10.0.13, `-cpu arm1176` (Pi 1/Zero), `-cpu cortex-a7` (Pi 2), `-cpu cortex-a53` (Pi 3+) |
| Probe | `ci/spike/probe`: tokio, sqlx (SQLite bundled C + Postgres), axum, argon2, zerocopy, rustls with `ring` or `aws-lc-rs`, rcgen |

The probe is not a hello-world. It opens SQLite through `sqlx`, creates a table, inserts
1,000 rows, reads `count`/`sum` back and asserts the float sum, reads `sqlite_version()`,
hashes an Argon2id password, exercises the crypto provider's RNG and hash, completes an
ECDHE for every classical key-exchange group with both halves in-process, and then performs
a **complete TLS 1.3 handshake plus application data in both directions**. That last part
matters: initialising a crypto provider proves nothing, because a provider's assembly is
only reached by use.

### A.1 glibc: the obvious approach, and why it fails

```
$ cargo build --release --target arm-unknown-linux-gnueabihf     # Debian armhf cross
   Finished `release` profile [optimized] target(s) in 1m 56s
$ arm-linux-gnueabihf-readelf -A mw-platform-probe
  Tag_CPU_name: "7-A"
  Tag_CPU_arch: v7
  Tag_FP_arch: VFPv3-D16
$ qemu-arm -cpu arm1176 -L <sysroot> mw-platform-probe
qemu: uncaught target signal 4 (Illegal instruction) - core dumped
$ qemu-arm -cpu cortex-a7 -L <sysroot> mw-platform-probe
sqlite rows=1000 sum=24995.0000
sqlite_version=3.46.0
argon2_ok len=97
OK
```

The build succeeds, the Rust code and `sqlite3.c` are both compiled for ARMv6 — and the
binary is still ARMv7, because the merged `Tag_CPU_arch` is the maximum over every linked
object and Debian's armhf glibc, `crt1.o` and `libgcc` are ARMv7-A:

```
$ arm-linux-gnueabihf-readelf -A .../libc.so.6 | grep Tag_CPU_arch
  Tag_CPU_arch: v7
```

So the Debian armhf cross toolchain cannot produce an ARMv6 binary at all. An "armv6" job
built this way ships an ARMv7 binary under an ARMv6 name, and the failure appears on a user's
Raspberry Pi Zero rather than in CI.

### A.2 Static musl: the answer

```
$ cargo build --release --target arm-unknown-linux-musleabihf   # RUSTFLAGS=-Ctarget-feature=+crt-static
   Finished `release` profile [optimized] target(s) in 2m 33s
$ file mw-platform-probe
ELF 32-bit LSB executable, ARM, EABI5 version 1 (SYSV), statically linked
$ qemu-arm -cpu arm1176 mw-platform-probe
arch=arm pointer_width=32
sqlite rows=1000 sum=24995.0000
sqlite_version=3.46.0
argon2_ok len=97
rustls_suites=9 kx_groups=3
secure_random_nonzero=true
hash=SHA384 len=48
kx X25519 agreed=true secret_len=32
kx secp256r1 agreed=true secret_len=32
kx secp384r1 agreed=true secret_len=48
tls_handshake suite=TLS13_AES_256_GCM_SHA384 roundtrip=true
OK

real 0m0.813s
```

Bundled C SQLite, Argon2id, X25519, P-256, P-384 and a full TLS 1.3 handshake with
AES-256-GCM, all on the CPU in a Raspberry Pi Zero. Artefact sizes, `tls-ring`:

| Target | Static binary |
|---|---|
| `arm-unknown-linux-musleabihf` | 4.9 MB |
| `armv7-unknown-linux-musleabihf` | 4.5 MB |
| `aarch64-unknown-linux-musl` | 4.9 MB |
| `x86_64-unknown-linux-musl` | 5.1 MB |
| `x86_64-pc-windows-gnu` (`.exe`) | 12.5 MB |

Static musl also removes a second problem nobody had named: the Go pipeline builds its Linux
daemon with `CGO_ENABLED=1`, which links glibc dynamically, so the artefact carries the
builder's glibc version requirement to a Pi that may be on an older Raspberry Pi OS. Nothing
in the Go dependency set needs cgo — `CGO_ENABLED=0 GOARCH=arm GOARM=6 go build ./cmd/remoteweather`
succeeds and produces a 38.9 MB static binary, measured here. A static artefact has no such
requirement by construction.

### A.3 Both crypto providers work, with one wrinkle worth knowing

`ring` and `aws-lc-rs` both cross-compile for ARMv6 musl and both complete the handshake on
an ARM1176. The wrinkle is that **`ring` ships ARMv7 assembly**, selected at runtime from
`HWCAP`:

```
$ # objects above the target's architecture, by crate
ring   (bsaes-armv7, vpaes-armv7, ghash-armv4, armv4-mont, chacha-armv4, sha256/512-armv4)
```

Those raise the final binary's `Tag_CPU_arch` to `v7` even in the musl build. It is benign —
`ring` gates them on a NEON check and `qemu -cpu arm1176` reports no NEON, exactly as a Pi
Zero does, which is why the handshake completes — but it means **the ELF attribute is not a
verdict**. `ci/spike/armv6-cross-spike.sh` therefore reports the attribute, names the crates
responsible, fails on any crate not on a short allowlist (`ring`), and treats the qemu run as
the actual answer. A new dependency that quietly drags in ARMv7 code gets flagged; `ring`
does not produce a permanent false alarm.

One honest limit: `Tag_FP_arch` is `VFPv3` even without `ring`, because the `libunwind.a`
that rustup ships for this target contains VFPv3 register save/restore helpers. Those are
only called when an unwind table asks for `d16`–`d31`, which ARMv6-compiled code never
produces, and the probe's panic path was not exercised on ARMv6. **Before the first ARMv6
release, run the probe on real Pi Zero hardware** — qemu is lenient in ways silicon is not,
and this is the one claim here that emulation cannot fully close.

### A.4 What was not measured

- **No ARMv6 hardware.** Everything above is qemu with the CPU model pinned, which is a
  strong proxy and not a Pi.
- **No performance numbers.** Measuring throughput under emulation would be noise.
  Specifically unmeasured: musl's allocator is slower than glibc's under multi-threaded
  contention, which a tokio daemon can feel. The usual remedy is a `#[global_allocator]` of
  `mimalloc` on musl targets. That is OQ-2, not a decision taken here.
- **No Windows execution.** The pipeline has no Windows runner for it and no need for one
  until OQ-3 is answered.
- **`cross` images were not used for the measurement**; the toolchains above were fetched
  directly. `platform.yml` uses `cross`, so the first run of that workflow is also the
  first check that `cross`'s images agree with these results.

---

# Decisions

1. The single required check is `gate`, a job that `needs:` every other job in `pr.yml` and
   fails on any result other than `success` — including `skipped` and `cancelled`.
2. The PR gate is `fmt`, `clippy`, `build`, `test`, `race` (ThreadSanitizer), `loom`,
   `gates`, `codegen-drift`, `conformance`, `driver-emulators`, `database` and `coverage`.
3. Every `cargo` invocation in the gate passes `--locked`, so a dependency change must be
   committed as a lockfile change and therefore reviewed.
4. Everything the gate does is a `cargo xtask` subcommand, reproducible locally with the
   identical command.
5. The whole gate runs on pull requests, on pushes to `main`, and in a merge group.
6. Cross-compilation runs in a separate workflow (`platform.yml`) on PRs that touch
   `Cargo.lock`, `rust-toolchain.toml`, `ci/release.toml` or `ci/spike/**`, on every push to
   `main`, weekly, and on demand. It is not in the required set.
7. Concurrency cancels superseded PR runs and never cancels a run on `main`.
8. Checks whose subject is another work item's deliverable are declared in `ci/gates.toml`
   with the path that arms them; `cargo xtask gate <id>` reports deferred or runs the check,
   so no human has to remember to turn one on.
9. A gate that arms into a missing command fails and names the owning work item. Gates
   cannot arm into a no-op.
10. The deferred-gate table is written into every run's summary, so the not-yet-armed set is
    visible on each PR.
11. `cargo xtask codegen --check` generates into `target/codegen-check/` and diffs; the job
    additionally asserts `git status --porcelain` is empty afterwards (`DESIGN.md` R-2).
12. The W8–W2 codegen contract is three paths — `metrics/registry.toml`,
    `cargo run -p mw-codegen -- --registry <file> --out <dir>`, and `generated/` — defined in
    one file, `xtask/src/codegen.rs`.
13. Concurrency checking is both ThreadSanitizer on a pinned nightly and `loom` on stable;
    they are complementary, not redundant.
14. The nightly pin is bumped when a nightly regression breaks the `race` job. The job is
    never disabled.
15. Coverage is a ratchet in `ci/coverage-floor.txt` that can only rise, with a 0.5-point
    tolerance, and no fixed target. It starts at 0.00.
16. CI never commits to the repository; the coverage floor is raised by hand in the PR that
    earns it.
17. The database job runs `timescale/timescaledb-ha:pg17` as a service container, because
    `DESIGN.md` D-2 requires `timescaledb_toolkit`, which the plain image lacks.
18. `ci/sql/assert-timescaledb-version.sql` fails on PostgreSQL < 17 or TimescaleDB < 2.31
    and compares versions as integer arrays, not text.
19. Migrations are applied twice — from empty, and from the previous release's schema with
    seeded rows — and the two results are compared on schema dump, continuous-aggregate
    catalog including view definitions, and surviving data.
20. The previous schema for path B is reconstructed from git (`git describe` plus
    `git archive`), never from a committed dump, so the baseline cannot go stale.
21. `_timescaledb_*` schemas are excluded from the comparison: path B has chunks because it
    has rows, and chunks are not schema.
22. The migration applier keeps a ledger so path B applies only the delta. Re-applying every
    file would make path B fail on `relation already exists` and prove nothing.
23. The Go emulators are run from a **pinned commit** of the reference repository, with
    deterministic fault-injection rates rather than `-flaky` and hope.
24. **Every Linux release target is a statically linked musl build.** Measured: a glibc
    ARMv6 build from Debian's armhf cross toolchain produces an ARMv7-tagged binary that
    dies with SIGILL on an ARM1176; the static musl build runs.
25. The ARMv6 verdict is the qemu run under `-cpu arm1176`, not the build and not the ELF
    attribute. A build that links is not an answer.
26. The ELF architecture attribute is reported, not asserted: `ring` ships runtime-gated
    ARMv7 assembly. The spike script names the crates responsible and fails on any crate not
    on a short allowlist.
27. **The ARMv6 answer does not feed back into the SQLite driver choice.** `sqlx`'s bundled C
    SQLite compiles for ARMv6 and runs on an ARM1176; D-32 stands unamended.
28. `cross` is the cross-compilation tool, with `MW_BUILDER=cargo` as the local override.
    `cargo-dist` is rejected: it covers neither ARMv6 nor ARMv7, nor deb, rpm, Arch or
    multi-arch containers, so adopting it would mean two release systems.
29. Packages are built by `cargo-deb` and `cargo-generate-rpm`, configured from the daemon
    crate's `[package.metadata]`; §6 gives the blocks.
30. Container images are `FROM scratch` over the pre-cross-compiled static binaries, so the
    multi-arch build needs no QEMU. CA certificates, zoneinfo and an `/etc/passwd` entry for
    uid 65532 are copied from a builder stage.
31. The target matrix lives in `ci/release.toml` and is read by both `cargo xtask` and the
    release workflow's `strategy.matrix`, so it is stated once.
32. Windows amd64 is cross-compiled and zipped but not executed by CI, and whether it ships
    is OQ-3.
33. The Arch package drops ARMv6 because Arch Linux ARM has no ARMv6 port; the Pi Zero is
    served by the deb, rpm and tarball.
34. The Arch package is published to the AUR by hand. CI only asserts the PKGBUILD's
    `pkgver` matches the tag.
35. **Publishing is off until the owner sets `MW_PUBLISH_RELEASE`, `MW_PUBLISH_CONTAINERS`
    or `MW_SIGN_ARTIFACTS`.** Unset means off; a tag still builds and attaches the whole
    matrix to the workflow run.
36. `workflow_dispatch` defaults `dry_run` to `true`, so a rehearsal cannot publish by
    accident, and the resolved switches are printed into the run summary.
37. Workflow permissions are `contents: read`; the `publish` job raises them only when
    publishing is enabled.
38. Signing covers the checksum file, not each artefact: the checksums cover the artefacts,
    so one signature is a complete chain.
39. Release archives are built with `tar --sort=name --mtime=@0 --owner=0 --group=0`, so the
    same input produces the same bytes and a rebuilt tag can be compared with a published
    one.
40. The release profile is `panic = "unwind"` with `strip = "debuginfo"` and `lto = "thin"`:
    a Raspberry Pi deployment wants a usable backtrace more than it wants the last few
    percent of throughput.
41. The toolchain is pinned in `rust-toolchain.toml` and there is no separate MSRV policy;
    the supported compiler is the pinned one.
42. No application crate was created. The workspace is the root manifest plus `xtask`, with
    a `crates/*` glob, because `DESIGN.md` R-4 gives that layout to W3 §1.
43. The platform probe lives in `ci/spike/probe` with its own `[workspace]`, so it is test
    infrastructure rather than part of the daemon, and can be run against a candidate
    dependency version before the daemon adopts it.
44. The systemd unit is `Type=notify` and drops the Go unit's `MemoryMax=512M`; both are
    flagged to W3 in §6.

---

# Open questions

**OQ-1. Which TLS crypto provider?** Both `ring` and `aws-lc-rs` build and run on every
target including ARMv6. `ring` is smaller and brings no ARMv7-tagged-object surprise beyond
the one documented in A.3; `aws-lc-rs` is FIPS-oriented, adds a `cmake` build dependency and
pulls in a post-quantum hybrid group. *Recommendation:* **`ring`**, for the smaller build
and the shorter dependency chain, with the probe keeping both measured so the decision is
reversible. W3 records the choice.

**OQ-2. Does a static musl build need a replacement allocator?** musl's `malloc` is slower
than glibc's under multi-threaded contention, and the daemon is a tokio application. This
was deliberately not measured, because measuring it under emulation would produce noise.
*Recommendation:* ship musl as decided, and have W3 add a `#[global_allocator]` of
`mimalloc` for musl targets if a real workload shows contention. Do not pre-emptively adopt
one; it is a dependency with a C build for a problem not yet observed.

**OQ-3. Does Windows amd64 still ship?** It cross-compiles (measured, A.0) and nothing in
the capability inventory needs it: every station driver is serial, TCP, UDP or HTTP, and the
Go build only ever produced Windows with `CGO_ENABLED=0`. CI cannot execute it, so it would
ship untested. *Recommendation:* **drop it**, and re-add it if a user asks. Keeping an
untested published artefact costs more credibility than removing one costs convenience. The
owner decides, because dropping a published artefact is a user-facing change; until then it
is built and zipped.

**OQ-4. Does musl's resolver break WeatherLink Live discovery?** The capability inventory
lists "WeatherLink discovery", and musl's resolver supports neither `nsswitch.conf` nor NSS
modules, so a `.local` name resolved through `getaddrinfo` and `nss-mdns` would stop working
in a static musl build. If discovery is implemented with an mDNS crate that sends its own
multicast queries, there is no issue at all. *Recommendation:* W2a §4a implements discovery
with an in-process mDNS client rather than `getaddrinfo`. Flagged here because the
constraint comes from this work item's linkage decision, not from W2a's.

**OQ-5. Who owns the `race` job's nightly pin over time?** A pinned nightly goes stale; an
unpinned one turns a green PR red on an unrelated commit. The pin is the lesser evil, but it
needs an owner. *Recommendation:* bump it whenever the gate is touched for any other reason,
and let `platform.yml`'s weekly run catch the case where the pin stops being installable.

**OQ-6. Is ARMv6 worth keeping at all?** It is the only target that needed a special answer,
and the hardware is a single-core 1 GHz ARM11 with 512 MB of RAM. The static musl build
works, so this is not a technical blocker — but if no deployment is actually on a Pi Zero,
the target is carrying cost for nobody. *Recommendation:* keep it, since it works and the
pipeline cost is one more matrix entry. The owner should say whether a Pi Zero is in use,
because the answer also decides whether the real-hardware validation in A.3 is needed before
the first release or at all.

---

# Assumptions about other work items

1. **W3 creates `crates/modernweather` producing a binary named `modernweather`.** Both
   names are in `ci/release.toml`; the release pipeline is armed by that manifest existing.
2. **W3's daemon accepts `--config <path>`.** Assumed by the systemd unit and the
   Dockerfile. If the spelling differs, two lines change (§6).
3. **W3's daemon signals systemd readiness.** The unit is `Type=notify`. If W3 does not
   implement it, the unit must revert to `Type=simple`, or the service will time out and be
   killed.
4. **W3 owns the crate and workspace layout** (`DESIGN.md` R-4), so this item created no
   application crate and used a `crates/*` glob.
5. **W3 §8's concurrency tests will be runnable under both ThreadSanitizer and `loom`.** Both
   jobs exist and currently find nothing to run. If W3's concurrency tests need a real
   multi-threaded tokio runtime that TSan cannot instrument, say so — the gate's shape
   changes.
6. **W3 asserts the database version at startup** as well as in CI (D-42). This item covers
   only the CI half.
7. **W2 provides a `mw-codegen` binary** taking `--registry` and `--out`, and commits its
   output under `generated/`. Three constants in `xtask/src/codegen.rs` change if not.
8. **W2's generator is deterministic.** The drift check is a byte-for-byte diff, so a
   generator that emits a timestamp, a hash-map iteration order or an absolute path makes the
   gate fail on unchanged input.
9. **W1's conformance runner takes a base URL** (`DESIGN.md` R-3) and is reachable as
   `cargo xtask conformance --target-binary <path>`. The PR gate runs it against the Rust
   binary only; validating the fixtures against the Go daemon is a scheduled job.
10. **W1's fixtures are in `tests/conformance/`**, with `manifest.toml` as the file that arms
    the gate.
11. **W4 creates `migrations/`** as a directory of `.sql` files applied in filename order,
    and provides `ci/db/seed.sql` and `ci/db/verify-seed.sql`.
12. **W4's migrations are idempotent in the sense `sqlx-migrate` means** — each file applied
    exactly once, tracked by name — not in the `CREATE TABLE IF NOT EXISTS` sense that froze
    the Go schema at first-run shape (D-8).
13. **W4 does not introduce union views or versioned aggregates** (D-5). If that decision is
    revisited, the migration gate's comparison needs revisiting with it.
14. **W4's rebuild procedure can run inside CI's time budget** on a fixture-sized database.
    D-17's measured full rebuild is ~1 hour at ten years of history; the gate assumes
    fixtures, not production volumes.
15. **W2a §4a's emulator tests connect to a TCP port** supplied by the harness, for the Davis
    and Campbell drivers at least. Serial-framed drivers need a `socat` pty pair, and W2a
    decides whether that is worth it per driver.
16. **W2a implements WeatherLink Live discovery with an in-process mDNS client**, not via
    `getaddrinfo` of a `.local` name. This follows from the static musl decision (OQ-4).
17. **The reference Go repository stays readable at a pinned commit**, since the emulators
    are checked out from it rather than vendored.
18. **W6 reads these four closing sections** and nothing above them.

---

# Proposed amendments to `DESIGN.md`

**A-1 — D-5's rejection of union views should be reflected in the W8 prompt, not worked
around.** The canonical W8 prompt (`docs/rewrite/W8-ci-and-release.md` in both repositories)
asks §2 for "the union-view assertions W4 defines", which contradicts D-5. No union views
exist to assert on. *Carried on under D-5:* the migration gate compares schema dumps,
continuous-aggregate catalog entries including view definitions, and surviving data between
the two paths. *Requested:* correct that line in the prompt so W4 is not sent looking for a
scheme the design rejected. No change to any decision in `DESIGN.md`.

**A-2 — add a decision recording that Linux artefacts are statically linked against musl.**
`DESIGN.md` §9 covers CI and tooling but says nothing about linkage, and this is not a
packaging detail: it is the measured precondition for the ARMv6 target existing at all
(Appendix A.1), it removes the glibc-version coupling between builder and Raspberry Pi, and
it is what lets the container images be built without QEMU. *Proposed:* **D-43. Linux release
artefacts are statically linked against musl, for every architecture.** Rationale: a glibc
ARMv6 cross build from Debian's toolchain produces an ARMv7 binary that faults on a
Raspberry Pi 1/Zero; one linkage for all four architectures also means one packaging rule
and no runtime library skew. Consequence: musl's allocator and resolver differ from glibc's
— see OQ-2 and OQ-4.

**A-3 — record that the TLS crypto provider is a named crate choice.** §6's crate table lists
`axum`, `tonic` and `sqlx` but not a rustls crypto provider, and the choice is load-bearing on
ARMv6 (A.3) and affects `sqlx`'s own feature flags, which name the provider explicitly
(`_tls-rustls-ring-webpki` versus `_tls-rustls-aws-lc-rs`). Leaving it unnamed is how two work
items end up selecting different ones and the build pulls in both. *Proposed:* add a row to
D-32's table — **TLS: `rustls` with the `ring` provider** — subject to OQ-1. Both providers
are measured working on every target, so this is a choice, not a constraint.

**A-4 — D-40's "the emulators stay in Go" should say "at a pinned commit".** D-40 and the
capability inventory both treat the emulators as fixed external test infrastructure, which is
right, but "fixed" is doing load-bearing work there: the reference repository is read-only to
this program and is not frozen, so an unrelated upstream commit can turn this repository's CI
red. *Proposed:* extend D-40's rationale with: *the emulators are consumed at a pinned commit
of the reference repository, recorded in this repository, and bumping it is a reviewed
change.* No decision reversed.
