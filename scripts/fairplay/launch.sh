#!/bin/sh
# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later

set -eu
PATH=/usr/bin:/bin
export PATH
bundle=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo=$(CDPATH='' cd -- "$bundle/../.." && pwd)
stage="$repo/vendor/fairplay/x86_64"
IFS= read -r socket < "$bundle/pulse-socket"
test -S "$socket"
ulimit -c 0
umask 077

# The daemon supplies a private RAM profile directory and the mode. Without
# one the provider gets a fresh tmpfs, which can sign in but is never saved.
case "$#" in
    0)
        mode=sign-in
        set -- --tmpfs /state
        ;;
    2)
        profile=$1
        mode=$2
        case "$mode" in sign-in|restore) ;; *) exit 2 ;; esac
        test "$(stat -f -c %T "$profile")" = tmpfs
        test "$(stat -c '%u:%a' "$profile")" = "$(id -u):700"
        set -- --bind "$profile" /state
        ;;
    *) exit 2 ;;
esac
exec bwrap --unshare-all --share-net --die-with-parent --clearenv \
    --ro-bind /usr /usr --symlink usr/bin /bin \
    --ro-bind-try /lib /lib --ro-bind-try /lib64 /lib64 \
    --proc /proc --dev /dev --tmpfs /tmp "$@" --dir /state/home --dir /state/android-data \
    --ro-bind-try /etc/ssl /etc/ssl --ro-bind-try /etc/ca-certificates /etc/ca-certificates \
    --ro-bind-try /etc/resolv.conf /etc/resolv.conf --ro-bind-try /etc/hosts /etc/hosts \
    --ro-bind-try /etc/nsswitch.conf /etc/nsswitch.conf --ro-bind-try /etc/localtime /etc/localtime \
    --ro-bind "$stage/rootfs" /state/rootfs --ro-bind "$repo/vendor/libhybris/x86_64" /app/hybris-linker \
    --ro-bind "$bundle/libdrm_client.so" /app/libdrm_client.so \
    --ro-bind "$bundle/fairplay-provider" /app/fairplay-provider \
    --ro-bind "$bundle/wvhost" /app/wvhost --dir /state/widevine \
    --ro-bind-try "$repo/vendor/widevine/x86_64/cdm" /app/widevine \
    --ro-bind "$socket" /audio/native --chdir /state/home \
    --setenv HOME /state/home --setenv ANDROID_DATA /state/android-data --setenv LD_LIBRARY_PATH /app \
    -- /app/fairplay-provider "$mode"
