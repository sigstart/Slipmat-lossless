#!/bin/sh
# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later

set -eu
PATH=/usr/bin:/bin
export PATH
trial=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo=$(CDPATH='' cd -- "$trial/../.." && pwd)
stage="$repo/vendor/fairplay/x86_64"
IFS= read -r socket < "$trial/pulse-socket"
test -S "$socket"
ulimit -c 0

# Only the namespace owns these RAM-backed profile directories. No account
# state survives cancellation, a crash, sign-out, or a daemon restart.
exec bwrap --unshare-all --share-net --die-with-parent --clearenv \
    --ro-bind /usr /usr --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib /lib64 \
    --proc /proc --dev /dev --tmpfs /tmp --tmpfs /state --dir /state/home --dir /state/android-data \
    --ro-bind-try /etc/ssl /etc/ssl --ro-bind-try /etc/ca-certificates /etc/ca-certificates \
    --ro-bind-try /etc/resolv.conf /etc/resolv.conf --ro-bind-try /etc/hosts /etc/hosts \
    --ro-bind-try /etc/nsswitch.conf /etc/nsswitch.conf --ro-bind-try /etc/localtime /etc/localtime \
    --ro-bind "$stage/rootfs" /state/rootfs --ro-bind "$stage/compat" /app/hybris-linker \
    --ro-bind "$trial/libdrm_client.so" /app/libdrm_client.so \
    --ro-bind "$trial/provider" /app/provider \
    --ro-bind "$trial/fairplay-alac-output" /app/fairplay-alac-output \
    --ro-bind "$socket" /audio/native --chdir /state/home \
    --setenv HOME /state/home --setenv ANDROID_DATA /state/android-data --setenv LD_LIBRARY_PATH /app \
    -- /app/provider
