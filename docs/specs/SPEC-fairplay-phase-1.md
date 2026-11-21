# FairPlay backend evaluation

> Migration record. The current build uses the native provider for playback and catalogue access.

## Goal

Replace the Chromium/Electron sidecar with a native Linux x86-64 backend using
`libandroidappmusic.so` and its companion x86-64 FairPlay libraries through
libhybris. The finished application must start, sign in, and play lossless
Apple Music with zero Chromium, Electron, or Widevine dependencies at build
time or runtime, including fallback playback. The existing sidecar serves as a
reference for behavior while the replacement is developed. Support for other
architectures can follow separately.

The prototype should preserve normal account authentication, subscription, and
licence checks. It may decode and buffer audio internally for immediate playback.
The Slipmat daemon and clients exchange controls, metadata, and status. The DRM
component retains its keys, and decoded audio goes through the backend's audio
output. See [licensed playback](../../CLAUDE.md#1-licensed-playback).

## Reference checkouts

| Checkout | Revision at setup | Start here |
| --- | --- | --- |
| Slipmat | `c833dbf` | `crates/slipmat-core/src/player/{sidecar,protocol}.rs`, `crates/slipmatd/src/{serve,heal,watchdog}.rs`, `crates/slipmat-core/src/ipc.rs`, `sidecar/{main,preload}.js` |
| Adam (`../INSPIRATION/apple-music-linux`) | `3c03a4d` | `README.md`, `drm/fairplay-libs.lock`, `scripts/fetch-fairplay-libs.sh`, `electron/fairplay-fetch.mjs`, `drm/drm_hybris.c` |

Record the full revisions and worktree state when starting. The child speaks
NDJSON to `slipmatd`. The daemon exposes its own IPC to GTK and CLImat and owns
the MPRIS service. Change the playback boundary below those clients if the
existing child protocol can carry the required behavior. Trace the account,
catalogue, and library operations that depend on Chromium today and replace
those dependencies before removing the sidecar.

Adam's README describes loading Android FairPlay libraries through libhybris.
Its pinned checksums can establish agreement with its lockfile. Publisher
provenance, compatibility, and permission to use or distribute the libraries
require their own evidence. Adam's Linux loader also contains code that modifies
an Apple sign-in response. Determine whether a candidate path can use the normal
sign-in and subscription checks. Use Adam's library loading as a reference for
the prototype.

Apple documents MusicKit SDKs for Android, Apple platforms and the web. Its
FairPlay Streaming Server SDK is intended for content providers. The public
documentation reviewed at setup did not establish a supported Apple Music
FairPlay client path for native Linux.
[MusicKit](https://developer.apple.com/musickit/)
[FairPlay Streaming](https://developer.apple.com/streaming/fps/)

## Native subscriber sign-in

GTK and CLImat share an opt-in native sign-in service. Start the daemon with
`slipmatd --native-sign-in-provider /absolute/path/to/provider` to enable
the **Lossless Sign-In (Experimental)** action in GTK and the native session
commands in CLImat. Use a trusted local provider. CLImat accepts `sign-in`,
`sign-in-status`, and `sign-out`. The native session is independent of the
current account and playback path.

The provider inherits no environment and receives newline-delimited JSON over
stdin. Its stdout carries replies. Both directions use a camel-case `type`
field. The sequence is `begin`, `credentials` with `account` and `password`,
and `verificationCode` with `code` for each requested challenge. The provider
replies with `credentialsRequired`, `verificationRequired`, `signedIn`,
`rejected`, `expired`, or `failed`. It must stay alive to hold an authenticated
session. The daemon terminates it on sign-out, cancellation, or timeout.

The prototype currently uses synthetic providers. The daemon tests cover
verification scoped to the initiating connection, shared session status,
sign-out, cancellation, expiry, rejection, and provider exit. The GTK and
CLImat smoke tests exercised the same daemon endpoint with synthetic values,
including CLImat reporting a session opened and closed through GTK.

Reference revision
`0761f491a70f44a83f4baae63fbc115b9f324318` has the same x86-64 FairPlay file
and libhybris pins, and predates the reviewed Linux sign-in response
interception. Its `libdrm_client.so` compiled from a disposable Git export and
its `hybris_backend_init` loaded the staged libraries in a credential-free
process. Ordinary account verification, TLS behaviour, in-memory session
handling, a full protected track, and licence denial remain to be qualified.

## Single-track native playback trial

From the Slipmat checkout on the audio desktop, build the trial with Go,
the host C/C++ toolchain, and bubblewrap. The build uses the staged binaries
and the pinned reference source above. It does not modify the reference
checkout. Keep the trial daemon running in its terminal.

```sh
cargo build --locked -p slipmatd -p climat
provider=$(sh scripts/fairplay-trial/prepare.sh)
target/debug/slipmatd --native-trial-provider "$provider"
```

In a second terminal, enter the account details and any verification code
locally, then use the decimal song ID from the Apple Music link.

```sh
target/debug/climat sign-in
target/debug/climat native-play <catalog-song-id>
target/debug/climat sign-out
```

The trial service uses the existing native auth socket and can run alongside
the regular daemon. It selects an explicitly declared ALAC rendition and
requires the FairPlay stream to succeed before opening the audio sink. The
entire trial track fits in a bounded in-memory buffer of up to 256 MiB.
Bubblewrap keeps the wrapper's writable session profile in private tmpfs.
Successful playback reports Symphonia's decoded sample rate, bit depth, and
channel count after the sink drains. The provider reports an ambiguous native
licence or media failure at the stream stage. Ctrl+C during playback ends the
trial session. Sign-out stops its worker and audio output.

The provider build and credential-free launch passed. A synthetic fragmented
ALAC track passed through the trial's Go-to-Symphonia handoff and drained to a
private PipeWire silent sink at 44.1 kHz, 16-bit stereo. The provider's stream
failure test included bytes written before a simulated refusal and verified
that the audio stage was skipped. A subscriber confirmed native sign-in and
reported a catalogue lookup failure for song `1442996689`. The updated
provider resolves numeric native storefront values via Apple's
`/v1/me/storefront` and reports storefront, authorization, missing song, and
missing lossless asset outcomes separately. Protected playback and Apple
licence denial remain unverified.

## Local library staging and catalogue authorization

The 22 x86-64 FairPlay libraries matched the reference SHA-256 manifest before
and after staging under
`vendor/fairplay/x86_64/rootfs/system/lib64/`. The manifest and staging
instructions live in [vendor/fairplay](../../vendor/fairplay/README.md).
The libhybris core and Android linker match the Git LFS pointers in the
reference checkout. The three stubs rebuilt from the reference source also
matched the installed copies. With both manifests verified, the isolated load
probe opened `libCoreFP.so`, `libstoreservicescore.so`, and
`libandroidappmusic.so` and exited successfully. It exercised library loading
without account credentials.

If the native session cannot authorize catalogue and library requests, Slipmat
will use the system browser for a separate MusicKit sign-in. The supplied Android
MusicKit SDK documents supplying a Music User Token from `link.apple.com`
alongside a developer token and storefront through its `TokenProvider`.
The registered Apple-hosted request and callback for Slipmat remain
unverified. The fallback is a Slipmat-controlled HTTPS MusicKit Web page
using `authorize()` and a Slipmat-issued developer token. Qualify the
browser-to-daemon handoff for personalized API access, then independently
verify that the native playback component accepts the session and enforces
subscription and licence checks. Keep the current playback path until both
checks pass. [MusicKit Web authorization](https://js-cdn.music.apple.com/musickit/v3/docs/iframe.html?id=user-authorization--page)

The Apple-hosted path needs the app's registered launch URL and callback. The
hosted MusicKit Web fallback needs a Slipmat Media ID, a server-held Media
Services key, and an HTTPS origin. Sign-in completion needs a short-lived
correlation value and a one-time handoff to the requesting daemon session.
The current REST client sends Apple web origin headers for the sidecar's
developer token. Verify the new app's token and request origin together with
a personalized `/v1/me/library/songs` request before switching the client.
A successful library request establishes account access. A licensed track
and denied-licence control are still needed to qualify the native session.
[Apple Media ID and key setup](https://developer.apple.com/help/account/capabilities/create-a-media-identifier-and-private-key/)

The reference Linux wrapper's initializer takes account credentials and a
verification callback. The supplied Android SDK's `TokenProvider` describes
a different interface. No browser-session input has been verified for the
Linux wrapper or the staged x86-64 component.

## Findings from the Linux playback trial and source review

The user's installed Adam player logged an active `codec=alac` session and
displayed 44.1 kHz during audible playback. Bit depth at the decoder and DAC
output format were unmeasured. At the next track the renderer reported a queue
handoff error and playback failed to advance. The installed build's correspondence
to the reference checkout was not independently verified.

The review used Slipmat `c833dbf12503aa4be12d2f85b013101c0d987933` with local
setup and audit scaffolding, and clean Adam `3c03a4d20ee8c2ed523cd86d35d9807dea2de5fa`.
Adam targets x86-64 Linux. Its lockfile and fetch scripts verify the mirror's
files against pinned digests. Evidence of Apple publisher provenance and desktop
licensing remains open.

Adam's playback controls ultimately call `openVLCSource` in
`engine/cmd/handlers_vlc.go`. The player can read a committed file or a
download in progress. The package comment in `engine/core/diskcache/diskcache.go`
identifies those files as decrypted audio. The Go engine can also return clear
media from `engine/core/playback/manager.go`. Separately, `drm/drm_hybris.c`
modifies an Apple sign-in response concerning phone verification. The source
review did not establish a bypass of Apple's subscription check.

Adam establishes that the x86-64 libraries can run in a Linux process and
participate in ALAC playback. The native Slipmat prototype needs to verify an
unmodified account flow and its own playback path. The current MusicKit child
owns and advances the queue. The daemon mirrors its events and publishes the
state to clients and MPRIS. A replacement must report each natural track
transition, including changes between two occurrences of the same song.

The offline regression in `sidecar/main.test.js` feeds the existing bridge a
paused restore, transport commands, and a synthetic natural advance between
duplicate entries. It checks the commands sent to the renderer and the event
frames sent back to the daemon. All nine sidecar tests passed with
`node --test --test-isolation=none sidecar/*.test.js`.

After fetching the locked dependencies, `cargo test --offline --locked
--workspace --quiet` passed with local Unix sockets and D-Bus available. The
daemon contract test uses a scripted child and a private session bus. It
checks paused restore, duplicate queue advancement, an MPRIS Pause round trip,
consistent status during a failed respawn, and sign-out. The Symphonia tests
compare exact PCM for synthetic 16-bit and 24-bit ALAC fixtures and a
prefetched track boundary. The clear-ALAC probe also exercised a private
PipeWire Pulse server. Two matching 16-bit stereo tracks at 44.1 kHz produced
one sink input across the boundary. A second run kept one stream across the
short 16-bit fixtures and opened a new stream for the 24-bit, 96 kHz fixture.
These runs used a silent sink. Audible gaplessness and licensed playback
through the native backend remain open.

```sh
cargo run --locked -p slipmat-playback --bin clear-alac-probe -- track.m4a next.m4a
```

## Official Android MusicKit SDK

The supplied `Android-MusicKit-SDK-1.1.4` package documents ALAC and Hi-Res
Lossless and reports the active stream variant through
`MediaPlayerController.getAudioVariant()`. Its controller accepts catalog or
library IDs, owns the playback queue, and provides playback and queue events.
The public API requires Android's application context and a token provider.

The MediaPlayback AAR supplies `libappleMusicSDK.so` for `arm64-v8a` and
`armeabi-v7a`. Its sample app loads that library by name. The installed Adam
bundle instead has an x86-64 `libandroidappmusic.so`. The SDK's native binary
imports real OpenSL ES functions and interfaces. Adam's `drm/Makefile` builds an
empty OpenSL ES stub for its own path. The supplied SDK therefore needs Android
runtime services and an ARM execution environment, or a separately supplied
compatible native build. No SDK playback, output format, or audio cache
behavior was verified at runtime.

## Work sequence

1. Delegate the two static reviews defined in [AGENTS.md](../../AGENTS.md).
   Trace the current child lifecycle and document the Adam dependency candidates
   with source anchors, trust boundaries, and open provenance questions. The
   existing `docs/security/README.md`, if present, can support the provenance
   review. Keep any wider audit separate.
2. Hand the reports to the architect. Specify the smallest test seam and the
   command/event pairs it must cover. Prefer exercising the existing protocol
   and daemon code over introducing a backend trait or a new protocol. Identify
   readiness, signed-out state, process exit, and queue restoration behavior.
3. Delegate one offline contract test against that seam. Use synthetic queue
   entries and events for play, pause, seek, skip, and a natural track advance.
   Cover duplicate entries and either restore without autoplay or the daemon's
   existing error/restart behavior, depending on the seam. Reuse test helpers
   and run targeted tests with cached dependencies. A synthetic event can
   verify the daemon's response to a natural advance. Audibly gapless playback
   requires separate measurement with a real audio backend.
4. Identify the exact native library build and verify startup and transport against
   synthetic fixtures. Test ordinary sign-in and a licensed track once the
   playback path is ready. Record the result or the specific blocker.
5. Switch Slipmat's startup and packaging to the native backend. Remove the
   Chromium sidecar, its provisioning and launch code, and its build and CI
   dependencies. Verify from a clean install on Linux x86-64.

## Exit criteria

- The lead records the verified revisions, relevant file or symbol references,
  and findings for provenance, architecture support, and account handling.
- The offline test exercises the agreed boundary and the lead records commands,
  results, and any checks blocked by the local environment.
- The prototype demonstrates playback with an active subscription and intact
  authentication and licence checks, or records the concrete blocker. Record who
  supplied the native binaries and which builds were exercised.
- For live playback, measure the selected stream and decoder output
  (codec, sample rate and bit depth), transport and MPRIS behavior, and a genuine
  gapless boundary. A lossless setting or an ALAC manifest entry alone does not
  establish the actual playback format.
- On a clean x86-64 system without Chromium, Electron, or Widevine, build and
  install Slipmat, sign in, browse the catalogue and library, and play an ALAC
  track. Verify that GTK and CLImat can control playback and that playback
  failure surfaces as an error without starting a secondary backend.
