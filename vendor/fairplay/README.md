# Local FairPlay setup on Linux x86-64

`make install` checks and downloads the Apple Android FairPlay libraries into
the Git-ignored `x86_64/rootfs/system/lib64/` directory. To fetch or repair
just those files, run `make fairplay-libs` from the Slipmat checkout.

[The lockfile](x86_64/fairplay-libs.lock) records both GitHub mirrors at exact
commits and the expected SHA-256 digest of each library. The downloader reuses
matching local files and tries the backup mirror if the primary fails or
returns a file with the wrong digest. It only moves verified downloads into
place. These files stay local and are excluded from distribution packages.
The digests verify agreement with the pinned inventory. Apple publisher
provenance and redistribution permission remain open questions.

## Playback development

`sh scripts/fairplay/prepare.sh` stages the same libraries as
`make fairplay-libs`. Pass `--check` to verify the local files without
downloading anything.

`make fairplay-provider` builds the provider, `crates/fairplay-provider`, with
`libdrm_client.so` from `vendor/adam/drm`. It needs Cargo, GNU make, a C/C++
toolchain, `xxd`, and development packages for OpenSSL, libcurl, and libpulse.
The launcher uses bubblewrap and a
local PulseAudio-compatible Unix socket. The daemon keeps the session profile
on private tmpfs under `$XDG_RUNTIME_DIR`, and saves it encrypted with a key in
Secret Service, reached through `secret-tool`. Without either, sign-in still
works but is not remembered.

The Adam copies of libhybris and its linker are Git LFS pointers. Supply
the matching binaries as `x86_64/compat/libhybris-core.so` and
`x86_64/compat/q.so`. The build checks those and the three staged Android
stubs against `x86_64/COMPAT-SHA256SUMS`.

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

The load probe builds its three Android stub libraries in a temporary
directory with the host C compiler. It requires separately staged libhybris
and its Android linker, verified against the loader entries in the
[compatibility manifest](x86_64/COMPAT-SHA256SUMS). Run
`sh scripts/fairplay-load-probe.sh` to check library loading on x86-64.
