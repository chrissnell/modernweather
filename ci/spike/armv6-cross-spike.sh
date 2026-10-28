#!/usr/bin/env bash
# Cross-compile the platform probe for one target, assert the ELF really is for the
# architecture asked for, and run it under qemu with the CPU model pinned.
#
# This is the script that answers W8's ARMv6 question, and it exists in the tree rather
# than in a document so that the answer can be re-measured when the crate stack changes —
# in particular if W2 or W4 swaps the SQLite driver.
#
# The two assertions are both necessary. "It linked" is not the answer: a Debian armhf
# cross toolchain links an ARMv6 Rust target quite happily and produces a binary tagged
# ARMv7 that dies with SIGILL on a Raspberry Pi 1. Reading Tag_CPU_arch catches that
# statically, and running under `-cpu arm1176` catches it dynamically.
#
# Usage:
#   ci/spike/armv6-cross-spike.sh                       # armv6, the interesting case
#   ci/spike/armv6-cross-spike.sh armv7-unknown-linux-musleabihf
#   TLS=tls-awslc ci/spike/armv6-cross-spike.sh         # the other crypto provider
#
# Requires: cross (or a toolchain named by CARGO_TARGET_<TRIPLE>_LINKER), and
# qemu-user-static for the run step. The run step is skipped with a loud message if qemu
# is missing, because a build-only result is worth having and a silent skip is not.

set -euo pipefail

TARGET="${1:-arm-unknown-linux-musleabihf}"
TLS="${TLS:-tls-ring}"
PROBE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/probe" && pwd)"

# CPU model per target: the oldest part the target is supposed to support, because that is
# the one an over-eager toolchain breaks.
case "$TARGET" in
  arm-unknown-linux-musleabihf | arm-unknown-linux-gnueabihf)
    QEMU=qemu-arm; CPU=arm1176; EXPECT_ARCH="v6" ;;   # Raspberry Pi 1 / Zero / Zero W
  armv7-unknown-linux-musleabihf | armv7-unknown-linux-gnueabihf)
    QEMU=qemu-arm; CPU=cortex-a7; EXPECT_ARCH="v7" ;; # Raspberry Pi 2
  aarch64-unknown-linux-musl | aarch64-unknown-linux-gnu)
    QEMU=qemu-aarch64; CPU=cortex-a53; EXPECT_ARCH="" ;; # Raspberry Pi 3 and later
  x86_64-unknown-linux-musl | x86_64-unknown-linux-gnu)
    QEMU=""; CPU=""; EXPECT_ARCH="" ;;
  *)
    echo "no CPU model recorded for $TARGET; add one to this script" >&2; exit 2 ;;
esac

BUILDER="${MW_BUILDER:-cross}"
if ! command -v "$BUILDER" >/dev/null 2>&1; then
  echo "note: $BUILDER not found, falling back to cargo (needs a linker for $TARGET)" >&2
  BUILDER=cargo
fi

echo "=== building mw-platform-probe for $TARGET (features: $TLS) with $BUILDER"
( cd "$PROBE_DIR" && "$BUILDER" build --release --target "$TARGET" --features "$TLS" )

TARGET_DIR="${CARGO_TARGET_DIR:-$PROBE_DIR/target}"
BIN="$TARGET_DIR/$TARGET/release/mw-platform-probe"
test -x "$BIN" || { echo "no binary at $BIN" >&2; exit 1; }
echo "=== artefact"
ls -l "$BIN"
file "$BIN" 2>/dev/null || true

if [ -n "$EXPECT_ARCH" ]; then
  echo "=== ARM architecture attributes"
  READELF="${READELF:-readelf}"
  ATTRS="$("$READELF" -A "$BIN")"
  echo "$ATTRS" | grep -E 'Tag_CPU_name|Tag_CPU_arch|Tag_FP_arch' || true
  GOT="$(echo "$ATTRS" | awk '/Tag_CPU_arch:/ {print $2; exit}')"

  # The merged Tag_CPU_arch is the maximum over every linked object, and it is NOT by
  # itself a verdict. Two different things raise it:
  #
  #   * an ARMv7 libc, crt object or libgcc, which makes the binary genuinely unrunnable
  #     on ARMv6 — this is what Debian's armhf cross toolchain does;
  #   * a crate that ships newer assembly behind a runtime CPU-feature check, which is
  #     fine because the newer instructions are never reached. Both candidate crypto
  #     providers do this — `ring` and `aws-lc-sys` each carry bsaes/vpaes/ghash/sha
  #     objects that assemble as ARMv7 and are selected from HWCAP.
  #
  # So the attribute is reported, the objects responsible are named, and an unexpected
  # name fails — but the verdict on whether the binary works is the qemu run below.
  if [ "$GOT" != "$EXPECT_ARCH" ]; then
    echo "note: Tag_CPU_arch is $GOT, above the target's $EXPECT_ARCH; finding the inputs"
    OFFENDERS="$(
      find "$TARGET_DIR/$TARGET/release/build" -name '*.o' 2>/dev/null |
      while read -r o; do
        a="$("$READELF" -A "$o" 2>/dev/null | awk '/Tag_CPU_arch:/{print $2; exit}')"
        [ "$a" = "$EXPECT_ARCH" ] || echo "$o"
      done | sed -E 's#.*/build/(.+)-[0-9a-f]+/.*#\1#' | sort -u
    )"
    echo "${OFFENDERS:-(none found among C/asm objects — the cause is in a prebuilt library)}"
    for crate in $OFFENDERS; do
      case "$crate" in
        # Allowlisted means two things, not one: the crate gates its newer assembly on a
        # runtime CPU-feature check, AND a binary containing it has been observed to run
        # correctly under `-cpu arm1176`. Both providers were checked that way, including a
        # complete TLS 1.3 handshake, which is what actually executes the AEAD assembly.
        ring | aws-lc-sys) ;;
        *)
          echo >&2
          echo "FAIL: $crate contributes objects above $EXPECT_ARCH and is not a known" >&2
          echo "      runtime-gated case. Either it gates its assembly on a CPU feature" >&2
          echo "      check — in which case add it to the allowlist in this script with a" >&2
          echo "      note — or this build genuinely cannot run on the target." >&2
          exit 1 ;;
      esac
    done
  else
    echo "Tag_CPU_arch = $GOT, matching the target exactly"
  fi
fi

# Debian's qemu-user package installs `qemu-arm`; qemu-user-static installs
# `qemu-arm-static`. Accept either, because which one a given runner image carries is not
# something this script should have an opinion about.
for candidate in "$QEMU" "$QEMU-static"; do
  if [ -n "$QEMU" ] && command -v "$candidate" >/dev/null 2>&1; then
    QEMU="$candidate"
    break
  fi
done

# This is the actual verdict. A build that links is not an answer: the Debian armhf cross
# toolchain links an ARMv6 Rust target happily and the result dies right here.
if [ -z "$QEMU" ]; then
  echo "=== running natively"
  "$BIN"
elif command -v "$QEMU" >/dev/null 2>&1; then
  echo "=== running under $QEMU -cpu $CPU"
  "$QEMU" -cpu "$CPU" "$BIN"
else
  echo >&2
  echo "SKIPPED the run step: $QEMU is not installed, so this run is NOT an answer." >&2
  echo "The build and the architecture attributes were checked; execution was not." >&2
  echo "Install qemu-user-static to make this a complete answer." >&2
  exit 3
fi

echo "=== $TARGET: ok"
