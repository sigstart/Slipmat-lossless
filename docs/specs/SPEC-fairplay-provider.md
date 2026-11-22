# Spec: The FairPlay provider in Rust

- Status: Implemented; the Go provider is deleted
- Issue: none yet
- Branch: `feat/fairplay-provider`

## Objective

Replace the Go provider built from `vendor/adam/engine` with **`fairplay-provider`**,
a Rust binary in this workspace. It keeps today's process boundary: `slipmatd`
spawns it in the bubblewrap sandbox and talks to it over stdio. It calls the
same C library, `libdrm_client.so`, which loads Apple's Android FairPlay
libraries through libhybris.

When this is done:

- Building Slipmat with FairPlay needs `cargo`, `make` and a C toolchain. It no
  longer needs Go, `vendor/adam/engine`, or the `profile-store` helper.
- Decryption streams one fragment at a time into the decoder. Today the whole
  track is decrypted into a 256 MiB buffer before any audio plays.
- A refused licence is reported as its own failure, before any audio starts.
- The "native" vocabulary is gone. It meant "not the Electron sidecar", and
  there is no sidecar any more.

## What phase 1 established

Phase 1 replaced the Electron sidecar with the Go provider. The full record is
in this file's history (`git log --follow docs/specs/SPEC-fairplay-provider.md`).
Facts that still constrain this work:

- **The library loads and signs in on Linux x86-64.** The 22 Apple libraries are
  pinned by SHA-256 in `vendor/fairplay/x86_64/fairplay-libs.lock`. The pins
  show agreement with a mirror. **Apple publisher provenance and redistribution
  permission are still open**, so the libraries stay local and out of
  distribution packages.
- **Sign-in goes through Apple unmodified.** Adam's loader rewrote an Apple
  sign-in response about phone verification. The vendored copy does not. The
  provider supports the six-digit code and exits on any other verification
  dialog.
- **The Android MusicKit SDK is not a route.** Its native library is ARM-only
  and needs real OpenSL ES and Android runtime services.
- **The daemon owns and advances the queue** (rule 3). The provider plays one
  track at a time and reports when it finishes.

## Scope

### In scope

1. The renames in [Vocabulary](#vocabulary), including removing the
   single-track trial mode.
2. Porting `scripts/fairplay/profile-store.go` into `slipmatd`.
3. A `fairplay-wire` crate that holds the daemon ↔ provider protocol types.
4. A `fairplay-provider` crate that covers sign-in, session restore, catalogue
   credentials, and streaming ALAC playback.
5. Keeping the Go provider buildable as `fairplay-provider-go` until the Rust
   provider passes [live qualification](#live-qualification). After that, delete
   it along with `vendor/adam/engine`.

### Out of scope

- Rewriting `vendor/adam/drm` (C). It is the libhybris loader plus vendored
  native code, and nothing is gained by porting it.
- Loading FairPlay inside `slipmatd`. See [Why a separate process](#why-a-separate-process).
- Seeking, gapless playback and prefetching the next track. The streaming design
  makes them possible later; this spec does not deliver them.
- Moving the bubblewrap invocation from `launch.sh` into Rust.
- Other architectures, Flatpak or AUR packaging of the provider.

## Why a separate process

Loading the FairPlay stack changes the process it runs in, and some of those
changes cannot be undone:

- `hybris_backend_init` calls `setenv` (`HYBRIS_*`, `ANDROID_ROOT`,
  `ANDROID_DATA`). That is unsound while other threads read the environment,
  which tokio's worker threads do.
- libhybris loads bionic next to glibc, and on Linux its core is loaded from a
  memfd.
- The native code ends the process on failure: `abort()` in `hybris_stubs.c`,
  `exit(0)` in `native/main.c`, and `std::terminate` handling in `guard.cpp`.
- Sign-in cannot be interrupted. After a timeout the native call keeps running.

A separate process can simply be killed. That is what rule 6 relies on, and the
sandbox is what keeps content keys inside the DRM component (rule 1).

## Design

### Processes

```
slipmatd ──fairplay-wire (JSON lines, stdio)──▶ launch (sh) ──▶ bwrap ──▶ fairplay-provider
   │ profile_store: decrypts/encrypts the profile          │
   │ on private tmpfs, key in Secret Service               ├─ dlopen /app/libdrm_client.so
   └─ binds that tmpfs into the sandbox as /state          └─ slipmat-playback → /audio/native
```

The provider runs in one of two modes, given as an argument:

- **`fairplay-provider sign-in`** — empty profile; asks for credentials.
- **`fairplay-provider restore`** — profile restored by the daemon; never asks
  for a password.

These replace "no argument" and `--restore-probe`.

### Crates

| Crate | Depends on | Holds |
| --- | --- | --- |
| `fairplay-wire` | `serde`, `serde_json`, `tokio` (`io-util`), `zeroize` | The request and reply enums, the line framing and its limit, and the `Secret` type that is zeroised on drop. The daemon's client socket reuses the framing and `Secret`. |
| `fairplay-provider` (bin) | `fairplay-wire`, `slipmat-playback`, `reqwest`, `tokio`, `serde_json`, `zeroize`, `libc`, `anyhow` | Everything below. |

The protocol gets its own crate because `slipmat-core` pulls in
`mpris-server` and zbus, and the sandboxed process should not carry a D-Bus
stack it never uses. With two parties that must agree byte for byte, a shared
definition is about correctness, not reuse, so rule 11's "third use" bar does
not apply.

Suggested modules in `fairplay-provider`, sized to the Go they replace:

| Module | Replaces | Notes |
| --- | --- | --- |
| `main.rs` | `provider.go:run` | Reserves the stdout pipe, initialises hybris, then starts the runtime. |
| `ffi.rs` | cgo declarations in `adamplay_backend.go` | `dlopen` table and safe wrappers. |
| `fairplay.rs` | `adamplay_backend.go`, `adamplay_cgo_bridge.go` | The thread that owns every library call. |
| `catalogue.rs` | `catalogueALAC`, `musicJSON` | Storefront and `enhancedHls` lookup. |
| `hls.rs` | the parts of `core/hls` that are used | Master variants, media map, byte ranges, key tags. |
| `cbcs.rs` | `core/fairplay/cbcs.go`, `utils/alacstream` | fMP4 walk, key mapping, in-place decrypt. |
| `play.rs` | `output`, `runDecoder`, `fairplay-alac-output` | Decrypted fragments → `AlacDecoder` → Pulse. |

### Loading the library: `dlopen`, not link-time

`ffi.rs` opens `/app/libdrm_client.so` with `libc::dlopen` and resolves seven
symbols into a table of typed function pointers:

```
hybris_backend_init        hybris_fairplay_init       hybris_last_error
hybris_open_kd_ctx_from_uri hybris_backend_decrypt   drm_lib_get_account
drm_shutdown
```

Linking at runtime means `cargo build --workspace`, clippy and CI work on
machines that never built the C library. It also lets tests swap in a fake
decrypt function. A missing library or symbol is a `failed` reply, never a
panic (rule 5).

### Threads, and the order of start-up

1. **Reserve stdout.** Duplicate fd 1 as the reply pipe, mark it close-on-exec,
   then point fd 1 at `/dev/null`, because the native libraries print to
   stdout. This is the same as `provider.go:run`.
2. **Initialise hybris while the process has one thread.** Call
   `hybris_backend_init` before the tokio runtime or any other thread exists.
   This is what makes its `setenv` calls sound. The `SAFETY` comment states
   that ordering.
3. **Start a single "fairplay" OS thread.** It owns every later library call:
   sign-in, account, key contexts and decrypt. Other code reaches it through a
   channel. This replaces the Go backend's `enter`/`gen` guard. The libraries'
   thread affinity is undocumented, and the Go bridge already pins its
   callbacks with `LockOSThread`.
4. **Start a current-thread tokio runtime** for stdio and HTTP. Audio runs on
   `slipmat-playback`'s own thread.

### Sign-in and restore

- **`sign-in`:** read `begin`, reply `credentialsRequired`, then read
  `credentials`. Pass the credentials to `hybris_fairplay_init` on the
  fairplay thread, and zeroise the C strings as soon as the call returns.
- **Verification callback.** It runs inside `hybris_fairplay_init`, on the
  fairplay thread, and blocks on a channel fed by the wire task. A two-factor
  challenge replies `verificationRequired` and waits for a six-digit
  `verificationCode`. **Any other challenge replies `verificationUnsupported`
  and exits the process before returning to the library**, because the bridge
  would otherwise pick a dialog option on the user's behalf.
- **`restore`:** call `hybris_fairplay_init` without credentials. If the library
  asks for credentials anyway, that is `failed`, never a prompt.
- **Both modes:** read `drm_lib_get_account()`, resolve the storefront through
  `/v1/me/storefront`, and reply `signedIn` with the catalogue credentials.
  Keep the rules 1 and 7 boundaries: tokens leave the sandbox only on this
  pipe and never appear in logs or error text.

### Playing a track

For `playTrack { track_id }`:

1. **Catalogue.** `GET amp-api…/catalog/{sf}/songs/{id}?extend=extendedAssetUrls`
   with an `Origin: https://music.apple.com` header gives the `enhancedHls`
   URL. The distinct failures (`storefront`, `catalogueAuthorization`,
   `catalogueTrack`, `catalogueAsset`) stay distinct.
2. **HLS.** From the master playlist, pick the ALAC variant with the highest
   bandwidth. From its media playlist, read the single file URL, each
   segment's byte range, and the `EXT-X-KEY` URIs. Only **segment 0's key**
   (the shared prefetch key) and the **first other key** (the track key) are
   used; every later segment decrypts with the track key. The track key is
   usually on segment 1, but on some releases the prefetch key covers segments
   0 and 1 and the track key starts at segment 2. Require HTTPS and an
   `skd://` key.
3. **Licence gate.** On the fairplay thread, open both key contexts **before any
   audio is emitted**:
   - the prefetch key under adamID `"0"`;
   - the track key under the real track ID.

   The prefetch key decrypts segment 0 even without a licence. That is why the
   Go provider buffered the whole track: a refusal could arrive after audible
   output. Opening the track key first moves the refusal ahead of output, and
   it is reported as the existing `licenceDenied` reply, which the queue
   already shows as its own error. A prefetch key that does not open is a
   `stream` failure. *Whether a refused licence always fails at key-open, and
   never later at decrypt, is an [open question](#open-questions).*
4. **CBCS.** Stream the file with one GET (Range-resumable, below) and walk the
   top-level boxes:
   - **Init (`ftyp` + `moov`).** Rewrite it into a clear sample description:
     - collapse the two `stsd` entries into one;
     - restore the original fourcc from `frma`, replacing `enca`;
     - drop `sinf` and the encryption `sbgp`/`sgpd`;
     - add `mvex`/`trex` if they are missing;
     - fix the sizes of every ancestor box.
   - **Each fragment (`moof` + `mdat`).** Read `tfhd`, `trun` and `senc`/`saiz`
     to find each sample's protected ranges. Decrypt them in place with the
     key for that fragment's segment, truncated to a multiple of 16 bytes as
     `cbcsFullSubsampleDecrypt` does. Pattern encryption (a non-zero skip
     block) is a `stream` failure: ALAC does not use it. Sample sizes do not
     change, so `moof` offsets stay valid.
   - **Stale key context.** `HYBRIS_ERR_STALE_CONTEXT` means a lease refresh
     freed the context. Reopen it and retry that sample once.
   - **Bounds.** Every size taken from the file is checked against the buffer
     before it is used, because the file comes from the network.
5. **Output.** Write the clear init and fragments into a bounded in-memory pipe
   read by `slipmat_playback::AlacDecoder`, and play through
   `play_controlled` to `/audio/native`. Decrypted bytes exist only in that
   pipe and the decoder (rule 1). The pipe holds a few fragments, not a track.
6. **Network.** A read that makes no progress for 30 s is a stall. A stall, a
   broken-off download or a 5xx resumes with an HTTP `Range` from the start of
   the top-level box it was in, up to three times, 1 s, 2 s and 3 s apart.
   Before audio that box is often the first, so this is the Go code's
   whole-attempt retry; after audio it is the next unread fragment. A CDN that
   ignores the range and answers 200 is read past what was already fed; any
   other status fails the track as `stream`, with the status in the fault.

The `vendor/adam` loop rewrites `tfdt`, `mfhd` and `TrackID` for VLC and
ffmpeg. It is not ported: the step 5 fixture decodes to the clear original's
exact PCM without it, and Apple tracks play through the key change with it
left out.

### Wire protocol (`fairplay-wire`)

It stays JSON lines with a camel-case `type` tag; only the changes are listed
here.

| Change | Why |
| --- | --- |
| `trialFailed` → `playbackFailed { stage }` | The trial is gone; this is ordinary playback. |
| `licenceDenied` arrives before any audio | A refused licence is a user-visible state of its own (rule 4), and the reply already existed. |
| `commitProfile` / `profileCommitted` / `profilePending` leave the provider protocol | The daemon now holds the profile itself, so the provider never sees them. |
| The mode is an argument (`sign-in` / `restore`) | It replaces flag-sniffing in `launch.sh`. |
| `playbackFailed` carries a `fault`, and a `diagnosis` for one that would not decode | "The stream could not be completed" covered a dozen causes. Both are codes and counts, never text from the network or the library. |
| `checkTrack { track_id }` | Plays a track to nowhere, as fast as it downloads, for `climat check`. |

`Secret` fields keep zeroising their frames as they do today.

### The profile store moves into `slipmatd`

`profile-store.go` runs outside the sandbox, and so does the daemon. Porting it
into `crates/slipmatd/src/profile_store.rs` removes a process hop, the
second Go binary, the `profile-protocol-v2` marker and `launch.sh`'s re-exec
branch.

The daemon:

1. creates a private profile directory under `$XDG_RUNTIME_DIR` and checks that
   it is tmpfs, mode `0700`, owned by the user;
2. for `restore`, decrypts the archive into it;
3. spawns `launch <profile> sign-in|restore`;
4. after a `signedIn` that the user's client accepts, snapshots the same file
   list `profile-store.go` uses and re-encrypts it;
5. removes the directory when the provider exits.

The details carry over: each path component is opened with `O_NOFOLLOW`, there
is a 16 MiB ceiling, and the temp file is renamed over the old archive.

- **Crypto:** AES-256-GCM with a 32-byte key from Secret Service. This adds
  `aes-gcm` (RustCrypto) to `slipmatd` and is the one new dependency in this
  spec outside the provider.
- **Key storage:** keep using `secret-tool` through `tokio::process`, so no
  D-Bus Secret Service client is added. The I/O runs off the GTK thread
  because it is in the daemon (rule 8).
- **No re-sign-in:** the archive format, its version string
  (`slipmat-fairplay-profile-v2`) and the keyring attributes are the Go
  helper's, so a session it saved restores unchanged. A unit test opens an
  archive in the exact JSON shape Go's `encoding/json` writes.
- **No runtime tmpfs:** if `$XDG_RUNTIME_DIR` is not tmpfs, or there is no
  state directory, the daemon warns and launches without a profile. `launch`
  then gives the provider a fresh tmpfs: sign-in works but is not saved.
- **Less reaches the sandbox:** `launch` no longer receives
  `DBUS_SESSION_BUS_ADDRESS` or `XDG_RUNTIME_DIR`, because only the daemon
  talks to the keyring.

### Build

`make fairplay-provider` runs `scripts/fairplay/build-provider.sh`:

```sh
make -C vendor/adam/drm HYBRIS_CORE=… libdrm_client.so   # in a scratch copy, as now
cargo build --locked --release -p fairplay-provider
# stage: fairplay-provider, libdrm_client.so, launch → target/fairplay-provider.XXXXXXXX/
```

The bundle's `launch` binds `/app/fairplay-provider` and `/app/libdrm_client.so`.
The state file `fairplay-provider` holds the path to `launch`.

The Go provider, its build script, `vendor/adam/engine` and
`fairplay-alac-output` were deleted in step 6. The Adam project upstream is
the place to get the Go engine back from, should a comparison ever be needed.

## Vocabulary

"Native" meant "not Electron". These files and modules need no prefix once the
sidecar is gone. Where one is needed, **`fairplay`** names the component and
**`provider`** names the process.

| Now | Becomes |
| --- | --- |
| `slipmatd/src/native_auth.rs` (1,316 lines) | `auth.rs` (endpoint, sign-in sessions) and `provider.rs` (spawn, supervise, the `Provider` type) |
| `slipmatd/src/native_queue.rs` | `queue.rs` |
| `scripts/fairplay/profile-store.go` | `slipmatd/src/profile_store.rs` |
| `slipmat-core/src/native_auth.rs` | `auth.rs` |
| `slipmat-core/src/native_auth_wire.rs` | `auth_wire.rs` for the client socket; the provider half moves to `fairplay-wire`; `runtime_dir` and `effective_uid` move to `paths.rs` |
| `slipmat/src/app/native_auth.rs` | `app/sign_in.rs` |
| `Event::NativeSignIn` (`nativeSignIn`) | `Event::SignInRequired` (`signInRequired`) |
| `PlaybackCapabilities.native` | removed: it is `true` at every site that sets it |
| `AppMsg::OpenNativeAuth`, `NativeAuthClosed`, `CommandMsg::NativeAuthAvailable`, `native_sign_in_required`, `native_playback` | the same names without `Native`/`native_` |
| `--native-sign-in-provider`, state file `native-provider` | `--fairplay-provider`, `fairplay-provider` |
| `--native-trial-provider`, `Mode::NativeTrial`, `climat native-play`, `TrialStage`, `Reply::TrialFailed` | removed; the queue path covers playback |
| `native-session.enc`, `slipmat-native-profile`, "Slipmat native session key" | `fairplay-session.enc`, `slipmat-fairplay-profile`, "Slipmat FairPlay session key" |
| Fixtures `native-auth-provider`, `native-queue-provider` | `auth-provider`, `queue-provider` |
| Occurrence IDs `native:{n}` | `q:{n}`, after checking whether `session.json` stores them |
| GTK "Native Sign-In" and its status strings | "Sign In to Apple Music"; the strings lose "native" |

The same pass updates the prose:

- CLAUDE.md rules 1, 6 and 7 ("native provider" → "FairPlay provider";
  "native profile helper" → "the daemon's profile store");
- this spec's link in CLAUDE.md's opening section;
- `README.md`, `crates/climat/README.md`, `vendor/fairplay/README.md`, and the
  packaging READMEs.

`vendor/adam/drm/native/` is vendored upstream layout and keeps its name.

## Work sequence

Each step is one PR and leaves `make check` green.

1. **Rename.** Apply [Vocabulary](#vocabulary) except the removals. No
   behaviour change, apart from the one re-sign-in caused by the archive name.
   The Go build output becomes `fairplay-provider-go`.
2. **Remove the trial and the constant flag.** Delete trial mode,
   `climat native-play` and `PlaybackCapabilities.native`.
3. **Profile store.** Port `profile-store.go` into `slipmatd`. Adapt the Go
   provider and `launch.sh` to the mode argument and the slimmer protocol.
   Extend `playback_contract.rs` to cover seed → commit → restore with a
   synthetic provider and a fake `secret-tool` on `PATH`.
4. **`fairplay-wire` and the provider skeleton.** `ffi`, the fairplay thread,
   sign-in, restore and catalogue credentials, with no playback yet. Verify
   live: sign-in with a verification code, then restore without a password.
5. **Playback.** Add `hls`, `cbcs`, the licence gate and streaming output.
   Offline fixture: take a clear ALAC fMP4 from `slipmat-playback`'s tests,
   encrypt it with a reversible fake cipher in `enca`/`sinf`/`senc` form, and
   check that the decoded PCM matches the clear original exactly, through a
   fake decrypt table.
6. **Qualify and delete.** Run [live qualification](#live-qualification), then
   delete `fairplay-provider-go`, `build-provider-go.sh`, `vendor/adam/engine`,
   `fairplay-alac-output` and Go from every build document.

## Verification

### Offline (CI)

- `make check` passes; the workspace builds with no `libdrm_client.so` present.
- `fairplay-wire` round-trips every message, and `Secret` is not printed by
  `Debug`.
- `cbcs.rs` passes the fixture in step 5, plus malformed input: truncated
  boxes, `senc` with too few entries, and a subsample past the sample's end.
  Each must be an error, never a panic.
- The daemon contract test drives sign-in, save, restore, play, pause, natural
  completion, `licenceDenied` and provider exit through
  the synthetic `queue-provider` fixture.

### Live qualification

Run with an active subscription. These gate step 6:

- Sign in with a verification code; restore after a daemon restart without a
  password.
- Play a full track. Record codec, sample rate and bit depth **at the decoder**,
  because an ALAC manifest entry alone does not establish the output format.
  The audio stream is opened in the decoder's own format, so the sink input's
  sample specification in `pactl list sink-inputs` is that reading.
- Pause, resume, stop, and natural advance between two occurrences of the same
  song, all reflected in MPRIS.
- Play from the GTK app and from CLImat.
- Peak resident memory during a long track, which should be far below the Go
  provider's buffer.

### Deferred: cross that bridge when we get there

These cannot be produced on demand, or cost more to stage than they are worth.
They do not block step 6. The code for each exists and is covered offline where
it can be; if one turns up in real use, it is investigated then, from what the
user saw and the provider's behaviour, rather than staged now.

| Case | What the code does | What a failure would look like |
| --- | --- | --- |
| **A licence refusal** | The track key is opened before anything downloads; a refusal is `licenceDenied`. | "Apple Music denied the playback licence." on a track that should play, or a track that plays briefly and then fails, which would mean Apple refuses at decrypt rather than at key-open. |
| **A lease refresh during playback** | `HYBRIS_ERR_STALE_CONTEXT` reopens the key and retries that sample once. | A long listening session where a track fails with "The FairPlay stream could not be opened or completed." partway through. |
| **Network loss mid-track** | A stall or a break resumes with `Range` from the last box boundary, up to three times; after that the track fails with `stream`. | The track stops with the `stream` error. The provider is its own process, so the daemon and the clients stay responsive either way. |

Network loss is the least of these: with no network, the library, search and
artwork are gone too, and a resumed track is a small part of recovering from
that.

## Open questions

- **Is key-open the licence check?** If Apple can open the track key and refuse
  at decrypt, the gate fires late. The fallback is to buffer only until the
  first track-key fragment decrypts — about one segment, not a track. Deferred
  with the licence refusal above.
- **Does Apple's CDN honour `Range` on the ALAC file?** If not, a mid-track
  network failure ends the track as `stream`. Deferred with network loss above.
- **How long is a lease?** Unknown; it only matters if a lease refresh is ever
  seen to break playback.
- **Provenance** of Apple's libraries is unchanged by this work and stays open.
  The origins of `libdrm_client.so`'s sources, including the unverified licence
  of the part taken from "wrapper", are recorded in
  [`vendor/adam/drm/README.md`](../../vendor/adam/drm/README.md).
