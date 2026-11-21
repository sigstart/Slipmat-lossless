# Flatpak build

Run `make flatpak-bundle` to build the GTK frontend and daemon against GNOME 49.
The manifest downloads the Rust sources in `cargo-sources.json`. Regenerate
that list after changing `Cargo.lock` with `packaging/flatpak/generate-sources.sh`.

CI builds bundles as packaging artifacts. The native FairPlay provider currently
requires local preparation outside this build and has no Flatpak runtime setup.
Use the [source build](../../README.md#development) to test playback.
