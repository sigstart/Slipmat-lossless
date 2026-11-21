#!/bin/sh
# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu
umask 077
repo=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
runtime=${XDG_RUNTIME_DIR:?}
test "$(stat -f -c %T "$runtime")" = tmpfs
test "$(stat -c '%u:%a' "$runtime")" = "$(id -u):700"
trial=$(mktemp -d "$runtime/slipmat-restore.XXXXXXXX")
archive="$trial/state/slipmat/native-session.enc"
pid=
stop() {
    if [ -n "$pid" ]; then
        kill "$pid" 2>/dev/null || :
        wait "$pid" || :
        pid=
    fi
}
cleanup() {
    stop
    secret-tool clear application dev.miguelrincon.Slipmat profile "$archive" >/dev/null 2>&1 || :
    rm -rf -- "$trial"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
mkdir "$trial/profile" "$trial/run"
export XDG_STATE_HOME="$trial/state"
cargo build --locked --manifest-path "$repo/Cargo.toml" -p climat -p slipmatd
launcher=$(sh "$repo/scripts/fairplay-trial/prepare.sh")
export XDG_RUNTIME_DIR="$trial/run"
start() {
    "$repo/target/debug/slipmatd" --native-trial-provider "$launcher" &
    pid=$!
    for attempt in $(seq 1 100); do
        if [ -S "$XDG_RUNTIME_DIR/slipmat-auth.sock" ]; then return; fi
        sleep 0.05
    done
    exit 1
}

start
echo 'Sign in once to create the test profile.'
if ! "$repo/target/debug/climat" sign-in; then
    echo 'The initial sign-in did not complete.' >&2
    exit 10
fi
if [ ! -s "$archive" ]; then
    echo 'No encrypted session was saved. Check the desktop keyring and provider.' >&2
    exit 11
fi
stop
start
for attempt in $(seq 1 85); do
    status=$("$repo/target/debug/climat" sign-in-status 2>/dev/null) || status=
    case "$status" in
        'A native Apple Music session is active.') break ;;
        'No native Apple Music session is active.')
            echo 'Automatic restore failed.' >&2
            exit 12 ;;
    esac
    sleep 1
done
test "$status" = 'A native Apple Music session is active.'
echo 'Fresh daemon validated the encrypted session with Apple.'
"$repo/target/debug/climat" sign-out
test ! -e "$archive"
stop
start
test "$("$repo/target/debug/climat" sign-in-status)" = 'No native Apple Music session is active.'
echo 'Sign-out removed the saved profile.'
