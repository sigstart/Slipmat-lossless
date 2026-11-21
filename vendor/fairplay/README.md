# x86-64 FairPlay libraries for local evaluation

The Linux x86-64 library inventory comes from Adam's `drm/fairplay-libs.lock`
at commit `3c03a4d20ee8c2ed523cd86d35d9807dea2de5fa`. The checksums verify
that the files match the reference inventory. Publisher provenance and
redistribution permission are open questions.

From the Slipmat repository, stage the files from an existing Adam installation.
The destination is ignored by Git and is for local evaluation.

```sh
set -eu
stage="$PWD/vendor/fairplay/x86_64"
source="$HOME/.config/adam/drm/rootfs/system/lib64"
(cd "$source" && sha256sum --check "$stage/SHA256SUMS")
mkdir -p "$stage/rootfs/system/lib64"
while read -r digest library; do
    cp -- "$source/$library" "$stage/rootfs/system/lib64/$library"
done < "$stage/SHA256SUMS"
(cd "$stage/rootfs/system/lib64" && sha256sum --check "$stage/SHA256SUMS")
```

## Compatibility files and load probe

`COMPAT-SHA256SUMS` identifies the libhybris core and Android linker from the
same reference revision. The source paths are `drm/libhybris-core.so` and
`drm/hybris-linker/q.so`. Use the Git LFS contents and put both files in
`vendor/fairplay/x86_64/compat/`. Build the three Android stubs from
`drm/stubs/empty_stub.c` and `drm/stubs/libdl.map` with the reference
Makefile's `android-stubs` target in a disposable build copy. Stage them in
`rootfs/system/lib64/` alongside the x86-64 FairPlay libraries. The verified
stub build matches the copies in the local installation.

Run `sh scripts/fairplay-load-probe.sh` from Slipmat. It verifies the two
manifests and opens the three main libraries through libhybris in a short-lived
process with an empty home directory. The probe verifies startup only. See the
[FairPlay evaluation spec](../../docs/specs/SPEC-fairplay-phase-1.md) for
playback and account requirements.
