# libhybris for Linux x86-64

libhybris lets a glibc process load Android's bionic libraries. The FairPlay
provider uses it to run Apple's Android libraries in-process.

| File | Used | How |
| --- | --- | --- |
| `x86_64/libhybris-core.so` | build time | embedded into `libdrm_client.so` by `vendor/adam/drm/Makefile` (`HYBRIS_CORE`), then loaded from a memfd |
| `x86_64/q.so` | run time | libhybris's port of the Android 10 linker; `scripts/fairplay/launch.sh` mounts this directory as `/app/hybris-linker` |

`scripts/fairplay/build-provider.sh` and `scripts/fairplay-load-probe.sh`
check both files against [`SHA256SUMS`](SHA256SUMS) before using them.

## Provenance

**Not reproducible yet.** These are the binaries Adam supplied with
`vendor/adam/drm`, where they arrived as Git LFS pointers with no source. What
the files themselves show:

- `q.so` was built from a libhybris checkout's `hybris/common/q`. Its debug
  paths start `/home/daksh/Git Projects/libhybris/`.
- Both were built with GCC 16.1.1 and 16.2.1, and are not stripped.
- `libhybris-core.so` links only against the host's glibc, libstdc++ and
  libgcc, and defaults its linker directory to `/tmp/hybris-linker`.

Which libhybris commit, and whether upstream or a fork, is unknown. The hashes
only confirm that you have these exact files, not what they were built from.

Replacing them with a build from a pinned libhybris commit, with the recipe
kept here, is what closes this. Check licence terms against that commit's
`COPYING` when it is pinned.
