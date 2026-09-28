#!/usr/bin/env bash
set -euo pipefail
# Container-only fixture. Root arranges UIDs and modes; each app/probe runs with
# real/effective UID 10001 or 10002 and an empty capability bounding set.
[[ ${CALLBOARD_CONTAINER_FIXTURE:-} == 1 && $(id -u) == 0 ]] || {
    echo 'Refusing: run via scripts/security/run.sh, never directly on the host.' >&2
    exit 1
}
[[ -x /app/callboard && -d /fixture && ! -e /fixture/initialized ]] || exit 1
touch /fixture/initialized
pids=()
cleanup() {
    for pid in "${pids[@]}"; do kill -TERM "$pid" 2>/dev/null || true; done
    wait || true
}
trap cleanup EXIT
as_user() {
    local uid=$1 base=$2
    shift 2
    setpriv --reuid="$uid" --regid="$uid" --clear-groups \
        --inh-caps=-all --ambient-caps=-all --bounding-set=-all \
        env -i PATH=/usr/bin:/bin HOME="$base" TOKIO_WORKER_THREADS=2 \
        XDG_DATA_HOME="$base/data" XDG_CONFIG_HOME="$base/config" \
        CALLBOARD_SOCKET_DIR="$base/run" "$@"
}
for owner in 10001 10002; do
    other=10002
    [[ $owner != 10002 ]] || other=10001
    base=/fixture/user-$owner
    install -d -m 0700 -o "$owner" -g "$owner" "$base"
    # exec makes $! the actual service, so cleanup sends SIGTERM to it.
    (
        exec setpriv --reuid="$owner" --regid="$owner" --clear-groups \
            --inh-caps=-all --ambient-caps=-all --bounding-set=-all \
            env -i PATH=/usr/bin:/bin HOME="$base" TOKIO_WORKER_THREADS=2 \
            XDG_DATA_HOME="$base/data" XDG_CONFIG_HOME="$base/config" \
            CALLBOARD_SOCKET_DIR="$base/run" /app/callboard serve
    ) >"/fixture/service-$owner.log" 2>&1 &
    pids+=("$!")
    ready=false
    for _ in {1..100}; do
        if as_user "$owner" "$base" /app/callboard --no-auto-start feeds >/dev/null 2>&1; then
            ready=true
            break
        fi
        kill -0 "${pids[-1]}" || { cat "/fixture/service-$owner.log"; exit 1; }
        sleep 0.05
    done
    "$ready" || { cat "/fixture/service-$owner.log"; echo 'Service readiness timed out' >&2; exit 1; }
    printf '%s' '[{"key":"owner","title":"private data"}]' |
        as_user "$owner" "$base" /app/callboard --no-auto-start put private >/dev/null
    echo "Testing service UID $owner, foreign UID $other"
    [[ $(stat -c %a "$base/run") == 700 && $(stat -c %a "$base/run/callboard.sock") == 600 ]]
    as_user "$owner" "$base" python3 /checks/probe.py allowed "$base/run/callboard.sock" "$owner"
    as_user "$other" "$base" python3 /checks/probe.py filesystem-denied "$base/run/callboard.sock" "$other"
    # Deliberately remove the DAC barrier only after startup, in this container.
    # The raw probe must connect, then be denied by the production server.
    chmod 0755 "$base" "$base/run"
    chmod 0666 "$base/run/callboard.sock"
    for attempt in uid-denied-read uid-denied-write; do
        as_user "$other" "$base" python3 /checks/probe.py "$attempt" "$base/run/callboard.sock" "$other"
    done
    as_user "$owner" "$base" python3 /checks/probe.py allowed "$base/run/callboard.sock" "$owner"
    chmod 0700 "$base" "$base/run"
    chmod 0600 "$base/run/callboard.sock"
    as_user "$owner" "$base" /app/callboard --no-auto-start get private >/dev/null
    kill -TERM "${pids[-1]}"
    wait "${pids[-1]}"
    unset 'pids[-1]'
    [[ ! -e "$base/run/callboard.sock" ]]
done
echo 'PASS: both real UIDs accepted as owner and rejected as foreign user; no cases skipped'
