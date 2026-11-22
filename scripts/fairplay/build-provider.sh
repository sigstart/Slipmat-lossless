#!/bin/sh
# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later

# Builds the Rust FairPlay provider bundle and prints the path to its launcher.
set -eu
repo=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
stage="$repo/vendor/fairplay/x86_64"
adam="$repo/vendor/adam"

sh "$repo/scripts/fairplay/prepare.sh" --check
(cd "$stage" && sha256sum --check --quiet --strict COMPAT-SHA256SUMS)
for tool in cargo make bwrap xxd; do
    command -v "$tool" >/dev/null || { echo "Missing build or runtime tool: $tool" >&2; exit 1; }
done

case "${PULSE_SERVER:-}" in
    unix:/*) socket=${PULSE_SERVER#unix:} ;;
    '') socket="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is required}/pulse}/native" ;;
    *) echo 'Set PULSE_SERVER to a local Unix socket.' >&2; exit 1 ;;
esac
case "$socket" in
    /*) ;;
    *) echo 'The audio socket path must be absolute.' >&2; exit 1 ;;
esac
if [ ! -S "$socket" ]; then
    echo "The PulseAudio-compatible socket is unavailable at $socket." >&2
    exit 1
fi

cd "$repo"
cargo build --locked --release --target-dir "$repo/target" -p fairplay-provider >&2
build=$(mktemp -d "$repo/target/fairplay-provider.XXXXXXXX")
work=$(mktemp -d)
trap 'rm -rf -- "$work"; if [ ! -x "$build/launch" ]; then rm -rf -- "$build"; fi' EXIT
trap 'exit 1' HUP INT TERM

cp -R "$adam/drm" "$work/"
make -B -C "$work/drm" HYBRIS_CORE="$stage/compat/libhybris-core.so" libdrm_client.so >&2
cp "$work/drm/libdrm_client.so" "$repo/target/release/fairplay-provider" "$build/"
printf '%s\n' "$socket" > "$build/pulse-socket"
install -m 700 "$repo/scripts/fairplay/launch.sh" "$build/launch"
printf '%s\n' "$build/launch"
