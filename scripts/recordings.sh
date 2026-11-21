# shellcheck shell=bash
# Sourced by scripts/replay/fetch.sh and scripts/testset/sync.sh: the live engine's recordings
# (dump-utterances, on recordings-pvc) through a debug container, as the image has no shell or tar.
# Sets CTX, k (kubectl in the namespace), POD, NAME (the container) and REC (the folder as seen from
# it; REC=/proc/1/root/models/recordings for the runs kept there before 2026-11-21).
CTX=${KUBE_CONTEXT:-admin@genesis}
# shellcheck disable=SC2034
REC=${REC:-/proc/1/root/recordings}
k() { kubectl --context "$CTX" -n innestemme "$@"; }

POD=$(k get pod -l app=innestemme -o jsonpath='{.items[0].metadata.name}')
NAME=recordings-$(date +%s)
k debug "$POD" --target=innestemme --container="$NAME" --image=busybox:1.37 --profile=restricted \
  --custom=<(printf '{"securityContext":{"runAsUser":65532,"runAsGroup":65532}}') -- sleep 600 >/dev/null
for _ in $(seq 30); do
  [ "$(k get pod "$POD" -o jsonpath="{.status.ephemeralContainerStatuses[?(@.name=='$NAME')].state.running}")" ] && break
  sleep 1
done
