#!/bin/sh
# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later

set -eu

if [ "$(uname -sm)" != 'Linux x86_64' ]; then
    echo 'This FairPlay load probe requires Linux x86-64.' >&2
    exit 1
fi

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
stage="$repo/vendor/fairplay/x86_64"
sh "$repo/scripts/fetch-fairplay-libs.sh" --check
(cd "$stage" && sed -n '/  compat\//p' COMPAT-SHA256SUMS | sha256sum --check --quiet --strict)

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
rootfs="$work/rootfs"
lib64="$rootfs/system/lib64"
mkdir -p "$work/data" "$lib64"
while read -r digest name; do
    case "$digest" in ''|'#'*|source) continue ;; esac
    ln -s "$stage/rootfs/system/lib64/$name" "$lib64/$name"
done < "$stage/fairplay-libs.lock"

# The Android linker resolves these SONAMEs during startup. The probe
# exercises library loading only and never requests Android audio services.
printf 'LIBC {};\n' > "$work/libdl.map"
for name in libandroid.so libOpenSLES.so libdl.so; do
    set --
    if [ "$name" = libdl.so ]; then set -- "-Wl,--version-script=$work/libdl.map"; fi
    cc -shared -nostdlib -fPIC -Wl,--hash-style=both "-Wl,-soname,$name" "$@" \
        -x c /dev/null -o "$lib64/$name"
done
cc -std=c11 -Wall -Wextra -Werror -x c -o "$work/probe" - -ldl <<'C'
#include <dlfcn.h>
#include <stddef.h>
#include <stdio.h>

int main(int argc, char **argv)
{
    if (argc != 2)
        return 2;

    void *hybris = dlopen(argv[1], RTLD_NOW | RTLD_GLOBAL);
    if (!hybris) {
        fprintf(stderr, "libhybris: %s\n", dlerror());
        return 1;
    }

    void *(*android_open)(const char *, int) = dlsym(hybris, "android_dlopen");
    const char *(*android_error)(void) = dlsym(hybris, "android_dlerror");
    if (!android_open || !android_error) {
        fputs("libhybris is missing the Android loader entry points.\n", stderr);
        return 1;
    }

    const char *libraries[] = {
        "libCoreFP.so", "libstoreservicescore.so", "libandroidappmusic.so"
    };
    for (size_t i = 0; i < sizeof(libraries) / sizeof(*libraries); ++i) {
        if (!android_open(libraries[i], RTLD_NOW)) {
            const char *error = android_error();
            fprintf(stderr, "%s: %s\n", libraries[i], error ? error : "unable to load");
            return 1;
        }
        printf("Loaded %s\n", libraries[i]);
        fflush(stdout);
    }
    return 0;
}
C

cd "$work"
ulimit -c 0
timeout 12s env -i HOME="$work" PATH=/usr/bin \
    HYBRIS_LINKER_DIR="$stage/compat" HYBRIS_LD_LIBRARY_PATH="$lib64" \
    HYBRIS_ANDROID_LIB64="$lib64" ANDROID_ROOT="$rootfs/system" \
    ANDROID_DATA="$work/data" "$work/probe" "$stage/compat/libhybris-core.so"
