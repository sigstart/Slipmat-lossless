#!/bin/sh
# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later

# Builds the three Android stand-in libraries beside Apple's, or checks them.
# Apple's libraries name libandroid, libOpenSLES and libdl as dependencies but
# import nothing from them, so each stub is one zeroed byte and no code. The
# recipe is the android-stubs rule in vendor/adam/drm/Makefile. They are checked
# by their properties, because their bytes change with the toolchain.
set -eu
PATH=/usr/bin:/bin
export PATH

case "${1:-}" in
    '') check_only=0 ;;
    --check) check_only=1 ;;
    *) echo "Usage: $0 [--check]" >&2; exit 2 ;;
esac

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
dest="$repo/vendor/fairplay/x86_64/rootfs/system/lib64"
stubs='libandroid.so libOpenSLES.so libdl.so'
command -v readelf >/dev/null || { echo 'readelf (binutils) is required to check the Android stubs.' >&2; exit 1; }

refuse() {
    echo "Android stub $1: $2" >&2
    exit 1
}

check_stub() {
    file=$1
    name=$(basename -- "$file")
    test -f "$file" || refuse "$name" 'missing'

    # The API 21 loader can only map them; nothing in them may run.
    readelf -lW "$file" | awk '$1 == "LOAD" { f = ""; for (i = 8; i < NF; i++) f = f $i; if (f ~ /E/) bad = 1 } END { exit bad }' ||
        refuse "$name" 'has an executable segment'

    dynamic=$(readelf -dW "$file")
    for tag in NEEDED INIT FINI INIT_ARRAY FINI_ARRAY PREINIT_ARRAY REL RELA RELR JMPREL TEXTREL RPATH RUNPATH; do
        if printf '%s\n' "$dynamic" | grep -q "($tag)"; then
            refuse "$name" "has a $tag entry"
        fi
    done
    # The API 21 linker reads only DT_HASH; GNU hash arrived in API 23.
    for tag in HASH GNU_HASH; do
        printf '%s\n' "$dynamic" | grep -q "($tag)" || refuse "$name" "lacks $tag"
    done
    printf '%s\n' "$dynamic" | grep -qF "Library soname: [$name]" || refuse "$name" 'has the wrong SONAME'

    expected='OBJECT aml_empty_stub'
    if [ "$name" = libdl.so ]; then
        expected="OBJECT LIBC
$expected"
        readelf -VW "$file" | grep -q 'Name: LIBC$' || refuse "$name" 'lacks the LIBC version node'
    fi
    defined=$(readelf --dyn-syms -W "$file" |
        awk '$1 ~ /^[0-9]+:$/ && $7 != "UND" { sub(/@.*/, "", $8); print $4, $8 }' | sort)
    [ "$defined" = "$(printf '%s\n' "$expected" | sort)" ] || refuse "$name" 'exports something besides its stub symbols'
}

if [ "$check_only" -eq 1 ]; then
    for name in $stubs; do check_stub "$dest/$name"; done
    exit
fi

for tool in make cc; do
    command -v "$tool" >/dev/null || { echo "$tool is required to build the Android stubs." >&2; exit 1; }
done
mkdir -p "$dest"
work=$(mktemp -d "$dest/.android-stubs.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
trap 'exit 1' HUP INT TERM

make -s --no-print-directory -C "$repo/vendor/adam/drm" STUB_DIR="$work" android-stubs
for name in $stubs; do check_stub "$work/$name"; done
for name in $stubs; do mv -fT -- "$work/$name" "$dest/$name"; done
