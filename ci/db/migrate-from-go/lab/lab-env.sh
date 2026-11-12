#!/usr/bin/env bash
# Which pods the lab is, and which of them is the source. Sourced by run-in-pod.sh and by
# 10-seed.sh.
#
# It is one file rather than the same eight lines in both, because the failure mode when
# the two disagree is seeding or migrating the wrong database — and the drift that would
# cause it is a one-word change to the IMAGE default, which has no reason to look
# dangerous.
#
# Not executed directly, and deliberately sets no shell options: its callers own those.

POD="${POD:-w9-migration-lab}"
NS="${NS:-default}"
IMAGE="${IMAGE:-timescale/timescaledb-ha:pg17}"
# The source half only. Defaults to IMAGE, in which case the source shares the target's
# pod and nothing about the pre-2026-11-12 behaviour changes. Set it to production's
# version to run the whole procedure across the pair it will actually cross.
SRC_IMAGE="${SRC_IMAGE:-$IMAGE}"
if [ "$SRC_IMAGE" = "$IMAGE" ]; then
  SRC_POD="${SRC_POD:-$POD}"
else
  SRC_POD="${SRC_POD:-$POD-src}"
fi

# Whether this lab is split is recorded by the pods, not by whether the current shell
# happens to have SRC_IMAGE exported — and `migrate`, `10-seed.sh` and `down` all run in
# shells that may not. Without this, a `down` after a split `up` leaves the source pod
# running forever and the next split `up` fails with "already exists".
#
# Only safe once the pods exist, so `up` does not call it: a stale `-src` pod from an
# earlier split run must not capture a deliberately single-pod lab.
lab_adopt_existing_source_pod() {
  if [ "$SRC_POD" = "$POD" ] && kubectl get pod "$POD-src" -n "$NS" > /dev/null 2>&1; then
    SRC_POD="$POD-src"
    echo "lab: the source is the existing pod $SRC_POD"
  fi
}
