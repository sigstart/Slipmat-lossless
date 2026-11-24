# Google's Widevine CDM

Songs with no lossless master play as 256 kbps AAC under Widevine, through
Google's own CDM (see the provider spec, *Songs with no lossless master*).

`make widevine` downloads it into the Git-ignored `x86_64/cdm/` directory.
`make install` does not: it is opt-in. To check an existing copy, run
`sh scripts/fetch-widevine.sh --check`.

[The lockfile](x86_64/widevine.lock) pins one URL on Google's own release
server, `www.google.com/dl/release2/chrome_component/…`, the one Chrome's
component updater fetches the CDM from (app id
`oimompecagnajdejgnnjijobebaeigek`), and the SHA-256 of both the component and
the `libwidevinecdm.so` inside it. The same library, byte for byte, is what
Firefox's updater installs (verified 2026-11-24 against Firefox's
`gmp-widevinecdm/4.10.3112.0`).

## Terms

The component's `LICENSE`, copied beside the library, says the software may not
be used, modified, sold or distributed without a separate licence agreement
with Google. Chrome and Firefox have one; Slipmat does not. It is therefore
never committed, never packaged, and never fetched unless asked for.

## Updating

Google revokes old CDM versions, and a revoked one stops getting licences. To
bump, take the component URL Chrome's updater currently offers (the
`chromium-widevine` AUR package records it), then regenerate both digests in
the lockfile with `sha256sum`.
