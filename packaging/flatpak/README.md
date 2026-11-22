# Flatpak build

Run `make flatpak-bundle` to build the GTK frontend and daemon against GNOME 49.
The manifest downloads the Rust sources in `cargo-sources.json`. Regenerate
that list after changing `Cargo.lock` with `packaging/flatpak/generate-sources.sh`.

CI builds bundles as packaging artifacts. There is no Flatpak runtime setup
for the FairPlay provider. See the [FairPlay guide](../../vendor/fairplay/README.md)
for local library staging and provider requirements.
