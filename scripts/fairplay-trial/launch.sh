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
umask 077

if [ "$#" -eq 4 ] && [ "$1" = '--session-profile' ]; then
    # Before extracting, verify the parent is the private RAM runtime dir.
    runtime=${2%/*}
    if [ "$(stat -f -c %T "$runtime")" = tmpfs ] &&
       [ "$(stat -c '%u:%a' "$runtime")" = "$(id -u):700" ]; then
        exec "$trial/profile-store" "$trial/launch" "$2" "$3" "$4"
    fi
    [ "$4" = seed ] || exit 1
    set --
fi

# Normal launches use a fresh tmpfs. The restart probe supplies a private
# RAM directory shared by its two provider processes.
unset probe_flag
case "$#" in
    0) set -- --tmpfs /state ;;
    2)
        profile=$1
        mode=$2
        if [ "$mode" = restore ]; then probe_flag=--restore-probe; fi
        case "$mode" in seed|restore) ;; *) exit 2 ;; esac
        set -- --bind "$profile" /state
        test "$(stat -f -c %T "$profile")" = tmpfs
        test "$(stat -c '%u:%a' "$profile")" = "$(id -u):700"
        ;;
    *) exit 2 ;;
esac
exec bwrap --unshare-all --share-net --die-with-parent --clearenv \
    --ro-bind /usr /usr --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib /lib64 \
    --proc /proc --dev /dev --tmpfs /tmp "$@" --dir /state/home --dir /state/android-data \
    --ro-bind-try /etc/ssl /etc/ssl --ro-bind-try /etc/ca-certificates /etc/ca-certificates \
    --ro-bind-try /etc/resolv.conf /etc/resolv.conf --ro-bind-try /etc/hosts /etc/hosts \
    --ro-bind-try /etc/nsswitch.conf /etc/nsswitch.conf --ro-bind-try /etc/localtime /etc/localtime \
    --ro-bind "$stage/rootfs" /state/rootfs --ro-bind "$stage/compat" /app/hybris-linker \
    --ro-bind "$trial/libdrm_client.so" /app/libdrm_client.so \
    --ro-bind "$trial/provider" /app/provider \
    --ro-bind "$trial/fairplay-alac-output" /app/fairplay-alac-output \
    --ro-bind "$socket" /audio/native --chdir /state/home \
    --setenv HOME /state/home --setenv ANDROID_DATA /state/android-data --setenv LD_LIBRARY_PATH /app \
    -- /app/provider ${probe_flag:+"$probe_flag"}
