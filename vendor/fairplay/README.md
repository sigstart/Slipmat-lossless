# Local FairPlay setup on Linux x86-64

`make install` checks and downloads the Apple Android FairPlay libraries into
the Git-ignored `x86_64/rootfs/system/lib64/` directory. To fetch or repair
just those files, run `make fairplay-libs` from the Slipmat checkout.

[The lockfile](x86_64/fairplay-libs.lock) records both GitHub mirrors at exact
commits and the expected SHA-256 digest of each library. The downloader reuses
matching local files and tries the backup mirror if the primary fails or
returns a file with the wrong digest. It only moves verified downloads into
place. These files stay local and are excluded from distribution packages.
Every pinned file has been traced to an official Apple or Google build; see
[Provenance](#provenance). Redistribution permission is still an open question.

## Provenance

Verified on 2026-11-22. All 22 pinned files are byte-identical (full SHA-256)
to an official build:

| Files | Official source |
| --- | --- |
| The 17 Apple libraries, `libc++_shared.so` and `libBlocksRuntime.so` included | `lib/x86_64/` of **Apple Music for Android 3.9.1, versionCode 1187** (the `split_config.x86_64.apk` inside APKMirror's bundle, posted 2022-03-29) |
| `libc.so`, `libm.so`, `liblog.so`, `libz.so`, `libstdc++.so` | `/system/lib64/` of Google's Android SDK image **`x86_64-21_r05.zip`** (Android 5.0.2, API 21). The zip's SHA-1 `9078a095825a69e5e215713f0866c83cef65a342` matches [Google's index](https://dl.google.com/android/repository/sys-img/android/sys-img2-1.xml). |

To repeat the check, hash `lib/x86_64/*.so` from that APK, and
`/system/lib64/*` from the image's `system.img` (extract the files with
`debugfs`), against the lock. Ignore the `Music/4.9`, `5.0.2` and `5.7` version
strings in the upstream wrapper source. They are client identities the wrapper
reports to Apple, not where the libraries came from. Nearby releases (3.9.0,
3.10, 4.0.0, 4.9.x, 7.0.3) do not match.

What is not covered:

- **libhybris** (`vendor/libhybris/x86_64/`) is a build Adam made and has no
  source here. Its [README](../libhybris/README.md) records what is known.
- **The three Android stubs** are not downloaded at all, so they need no
  provenance. `scripts/fairplay/android-stubs.sh` builds `libandroid.so`,
  `libOpenSLES.so` and `libdl.so` from `vendor/adam/drm/stubs/` and refuses
  any that has executable code, a constructor, a dependency, a relocation,
  anything exported besides `aml_empty_stub` (plus the `LIBC` version node in
  `libdl`), the wrong SONAME, or no `DT_HASH`. The API 21 loader reads only
  `DT_HASH`. The check looks at properties, not hashes, because the bytes change
  with the toolchain. GCC 16.2.1 with GNU ld 2.47 reproduces the stubs that
  upstream installers generate byte for byte.
- **`linker64` and `tzdata`** from the upstream rootfs are verified, and their
  hashes are recorded, commented out, in the lock. Slipmat fetches neither:
  libhybris's own linker replaces `linker64`, and the provider runs with no
  tzdata. Adding tzdata would change how bionic behaves, and that is untested.

### Why the libraries stay at 3.9.1

The wrapper calls 99 of Apple's private C++ functions by their exact compiled
names, and it depends on Apple's object layouts. A layout mismatch corrupts
memory rather than failing cleanly, so swapping in a newer APK is not an
upgrade. 4.0.0 is the only later release with no missing functions, and its
curl and libxml2 are the same as 3.9.1's. 4.9.7.2 is missing
`FootHillConfig::config` and the auth-flow constructors. 7.0.3 drops
`storeservicescore`, so 89 of the 99 functions are missing. From 4.9 onward the
libraries also need a newer bionic than API 21.

So the code is official but old: **curl 7.64.0, OpenSSL 1.1.1b (Feb 2019),
libxml2 2.9.9**, on an Android 5.0 bionic. The answer is containment, not
upgrading. The provider runs under bubblewrap, but it shares the host network,
so restricting it to Apple's servers is the gap that matters most. Revisit the
version only if Apple stops accepting 3.9.1 or the upstream wrappers move to
newer libraries, and then repeat the check above.

## Playback development

`sh scripts/fairplay/prepare.sh` stages the same libraries as
`make fairplay-libs` and builds the Android stubs beside them. Pass `--check`
to verify the local files without downloading or building anything.

`make fairplay-provider` builds the provider, `crates/fairplay-provider`, with
`libdrm_client.so` from `vendor/adam/drm`. It needs Cargo, GNU make, a C/C++
toolchain, `xxd`, and development packages for OpenSSL, libcurl, and libpulse.
The launcher uses bubblewrap and a
local PulseAudio-compatible Unix socket. The daemon keeps the session profile
on private tmpfs under `$XDG_RUNTIME_DIR`, and saves it encrypted with a key in
Secret Service, reached through `secret-tool`. Without either, sign-in still
works but is not remembered.

libhybris and its linker are tracked in `vendor/libhybris/x86_64/`. The build
checks them against `vendor/libhybris/SHA256SUMS`, and checks the Android stubs
by their properties.

```sh
make fairplay-libs
provider=$(sh scripts/fairplay/build-provider.sh)
state="${XDG_STATE_HOME:-$HOME/.local/state}/slipmat"
mkdir -p "$state"
printf '%s\n' "$provider" > "$state/fairplay-provider"
cargo build --locked --workspace
target/debug/slipmat
```

GTK and CLImat start the daemon with the saved launcher path. Restart any
running daemon after changing the setting. Update `fairplay-provider` after
moving the checkout or deleting the build directory. For a one-off override,
use `slipmatd --fairplay-provider /absolute/path/to/launch`.

Sign in from GTK or run `target/debug/climat sign-in`. This build supports
the six-digit verification prompt. Other Apple verification dialogs end the
attempt with an explicit error. Live sign-in and session restore passed.
Licensed ALAC output and licence denial still need live verification.

The load probe uses the staged Android stubs and the tracked libhybris after
checking both. Run
`sh scripts/fairplay-load-probe.sh` to check library loading on x86-64.
