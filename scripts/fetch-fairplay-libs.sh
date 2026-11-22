#!/bin/sh
# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later

set -eu
if [ "$(uname -sm)" != 'Linux x86_64' ]; then
    echo 'The FairPlay library download requires Linux x86-64.' >&2
    exit 1
fi

case "${1:-}" in
    '') check_only=0 ;;
    --check) check_only=1 ;;
    *) echo "Usage: $0 [--check]" >&2; exit 2 ;;
esac

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
stage="$repo/vendor/fairplay/x86_64"
lock="$stage/fairplay-libs.lock"
dest="$stage/rootfs/system/lib64"
sources=$(sed -n 's/^source //p' "$lock")
checksums=$(sed -e '/^#/d' -e '/^source /d' -e '/^$/d' "$lock")
if [ -z "$sources" ] || [ -z "$checksums" ]; then
    echo "Missing download sources or checksums in $lock." >&2
    exit 1
fi

if [ "$check_only" -eq 1 ]; then
    cd "$dest"
    printf '%s\n' "$checksums" | sha256sum --check --strict --quiet
    exit
fi

mkdir -p "$dest"
cd "$dest"
work=$(mktemp -d .fairplay-fetch.XXXXXXXX)
trap 'rm -rf -- "$work"' EXIT
trap 'exit 1' HUP INT TERM

while read -r digest library; do
    if printf '%s  %s\n' "$digest" "$library" |
        sha256sum --check --status; then
        continue
    fi
    command -v curl >/dev/null || { echo 'curl is required to fetch FairPlay libraries.' >&2; exit 1; }
    for source in $sources; do
        if curl --disable --fail --silent --show-error --location \
            --proto '=https' --proto-redir '=https' --connect-timeout 15 --max-time 300 \
            --output "$work/$library" "$source/$library"; then
            if (cd "$work" && printf '%s  %s\n' "$digest" "$library" |
                sha256sum --check --status); then
                mv -fT -- "$work/$library" "$library"
                continue 2
            fi
            echo "SHA-256 mismatch for $library from $source" >&2
        fi
    done
    echo "Could not fetch a verified copy of $library." >&2
    exit 1
done <<EOF_LOCK
$checksums
EOF_LOCK
