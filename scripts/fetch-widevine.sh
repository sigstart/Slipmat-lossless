#!/bin/sh
# SPDX-FileCopyrightText: 2026 Jan Vermeulen
# SPDX-License-Identifier: GPL-3.0-or-later

# Fetches Google's Widevine CDM into vendor/widevine/x86_64/cdm/, verified
# against widevine.lock. With --check, only verifies what is already there.
set -eu
if [ "$(uname -sm)" != 'Linux x86_64' ]; then
    echo 'The Widevine CDM download requires Linux x86-64.' >&2
    exit 1
fi
case "${1:-}" in
    '') check_only=0 ;;
    --check) check_only=1 ;;
    *) echo "Usage: $0 [--check]" >&2; exit 2 ;;
esac

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
stage="$repo/vendor/widevine/x86_64"
lock="$stage/widevine.lock"
dest="$stage/cdm"
source=$(sed -n 's/^source //p' "$lock")
component_sum=$(sed -n 's/^\([0-9a-f]\{64\}\)  component\.crx3$/\1/p' "$lock")
library_sum=$(sed -n 's/^\([0-9a-f]\{64\}\)  libwidevinecdm\.so$/\1/p' "$lock")
if [ -z "$source" ] || [ -z "$component_sum" ] || [ -z "$library_sum" ]; then
    echo "Missing the source or a checksum in $lock." >&2
    exit 1
fi

verify_library() {
    printf '%s  %s\n' "$library_sum" "$dest/libwidevinecdm.so" | sha256sum --check --status 2>/dev/null
}
if verify_library; then
    exit 0
fi
if [ "$check_only" -eq 1 ]; then
    echo 'The Widevine CDM is missing or does not match widevine.lock.' >&2
    exit 1
fi
for tool in curl unzip od; do
    command -v "$tool" >/dev/null || { echo "$tool is required to fetch the Widevine CDM." >&2; exit 1; }
done

mkdir -p "$stage"
work=$(mktemp -d "$stage/.widevine-fetch.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
trap 'exit 1' HUP INT TERM

curl --disable --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' --connect-timeout 15 --max-time 300 \
    --output "$work/component.crx3" "$source"
if ! (cd "$work" && printf '%s  component.crx3\n' "$component_sum" | sha256sum --check --status); then
    echo 'SHA-256 mismatch for the Widevine component.' >&2
    exit 1
fi

# A CRX3 is "Cr24", a version, a little-endian header length, that header, then
# a zip. Strip the front rather than let unzip guess past it.
magic=$(od -An -c -N4 "$work/component.crx3" | tr -d ' ')
header=$(od -An -tu4 -j8 -N4 "$work/component.crx3" | tr -d ' ')
if [ "$magic" != 'Cr24' ] || [ -z "$header" ]; then
    echo 'The Widevine component is not a CRX3 file.' >&2
    exit 1
fi
tail -c "+$((12 + header + 1))" "$work/component.crx3" > "$work/component.zip"
unzip -q -o "$work/component.zip" manifest.json LICENSE \
    _platform_specific/linux_x64/libwidevinecdm.so -d "$work/out"

mkdir -p "$work/cdm"
mv -- "$work/out/_platform_specific/linux_x64/libwidevinecdm.so" "$work/cdm/"
mv -- "$work/out/manifest.json" "$work/out/LICENSE" "$work/cdm/"
if ! (cd "$work/cdm" && printf '%s  libwidevinecdm.so\n' "$library_sum" | sha256sum --check --status); then
    echo 'SHA-256 mismatch for libwidevinecdm.so inside the component.' >&2
    exit 1
fi
rm -rf -- "$dest"
mv -T -- "$work/cdm" "$dest"
echo "Installed the Widevine CDM in $dest. Google's terms are in $dest/LICENSE."
