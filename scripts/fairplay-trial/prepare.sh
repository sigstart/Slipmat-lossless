#!/bin/sh
# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later

set -eu
repo=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
reference="$repo/../INSPIRATION/apple-music-linux"
revision=0761f491a70f44a83f4baae63fbc115b9f324318
stage="$repo/vendor/fairplay/x86_64"

if [ "$(uname -sm)" != 'Linux x86_64' ]; then
    echo 'The native FairPlay trial requires Linux x86-64.' >&2
    exit 1
fi
cd "$repo"
(cd "$stage" && sha256sum --check --quiet COMPAT-SHA256SUMS)
(cd "$stage/rootfs/system/lib64" && sha256sum --check --quiet "$stage/SHA256SUMS")
command -v bwrap >/dev/null

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
    echo 'The local PulseAudio-compatible socket is unavailable.' >&2
    exit 1
fi

cargo build --locked --target-dir "$repo/target" -p slipmat-playback --bin fairplay-alac-output >&2
mkdir -p "$repo/target"
trial=$(mktemp -d "$repo/target/fairplay-trial.XXXXXXXX")
git -C "$reference" archive -o "$trial/reference.tar" "$revision" drm engine LICENSE
tar -xf "$trial/reference.tar" -C "$trial"
make -C "$trial/drm" HYBRIS_CORE="$stage/compat/libhybris-core.so" libdrm_client.so >&2
mkdir "$trial/engine/cmd/slipmat-fairplay-trial"
cp "$repo/scripts/fairplay-trial/provider.go" "$trial/engine/cmd/slipmat-fairplay-trial/main.go"
cp "$repo/scripts/fairplay-trial/provider_test.go" "$trial/engine/cmd/slipmat-fairplay-trial/main_test.go"
(
    cd "$trial/engine"
    export CGO_ENABLED=1 CGO_LDFLAGS="-L$trial/drm" LD_LIBRARY_PATH="$trial/drm"
    go test -buildvcs=false -mod=readonly -tags adamplay_backend ./cmd/slipmat-fairplay-trial >&2
    go build -buildvcs=false -mod=readonly -tags adamplay_backend \
        -o "$trial/provider" ./cmd/slipmat-fairplay-trial
)
cp "$trial/drm/libdrm_client.so" "$trial/libdrm_client.so"
cp "$repo/target/debug/fairplay-alac-output" "$trial/fairplay-alac-output"
cp "$repo/scripts/fairplay-trial/launch.sh" "$trial/launch"
chmod 700 "$trial/launch"
printf '%s\n' "$socket" > "$trial/pulse-socket"
app_state="${XDG_STATE_HOME:-$HOME/.local/state}/slipmat"
mkdir -p "$app_state"
provider_file=$(mktemp "$app_state/native-provider.XXXXXXXX")
printf '%s\n' "$trial/launch" > "$provider_file"
mv -f -- "$provider_file" "$app_state/native-provider"
printf '%s\n' "$trial/launch"
