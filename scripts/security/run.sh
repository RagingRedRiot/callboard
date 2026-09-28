#!/usr/bin/env bash
set -euo pipefail
[[ $# == 1 ]] || { echo "Usage: $0 /absolute/path/to/freshly-built/callboard" >&2; exit 2; }
binary=$(realpath -- "$1")
[[ -x "$binary" ]] || { echo 'Missing executable' >&2; exit 1; }
context=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# Docker is required. Missing engine/build failures must never become a skip.
docker info >/dev/null
scratch=$(mktemp -d)
cleanup() {
    if [[ -s "$scratch/cid" ]]; then docker rm -f "$(cat "$scratch/cid")" >/dev/null 2>&1 || true; fi
    rm -rf -- "$scratch"
}
trap cleanup EXIT
docker build --iidfile "$scratch/image" "$context"
# No privileged mode, host networking, daemon socket, secrets, or writable host
# mounts. Capabilities are for the root fixture only; setpriv strips all of them
# before executing candidate code. Runtime network access is disabled.
timeout --signal=TERM --kill-after=10s 90s docker run --rm --init \
    --cidfile "$scratch/cid" --network none --read-only \
    --cap-drop ALL --cap-add DAC_OVERRIDE --cap-add CHOWN --cap-add FOWNER --cap-add SETUID \
    --cap-add SETGID --cap-add SETPCAP --cap-add KILL \
    --security-opt no-new-privileges --pids-limit 128 --memory 512m --cpus 2 \
    --tmpfs /fixture:rw,nosuid,nodev,noexec,mode=0755,size=64m \
    --tmpfs /tmp:rw,nosuid,nodev,noexec,mode=1777,size=16m \
    --mount "type=bind,src=$binary,dst=/app/callboard,readonly" \
    --env CALLBOARD_CONTAINER_FIXTURE=1 "$(cat "$scratch/image")"
