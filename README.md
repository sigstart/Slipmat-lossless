<!--
SPDX-FileCopyrightText: 2026 Miguel Rincon
SPDX-License-Identifier: GPL-3.0-or-later
-->

<p align="center">
  <img src="docs/screenshots/icon.png" width="128" alt="Slipmat icon">
</p>

# Slipmat

Apple Music for Linux with a native interface.

Slipmat and its terminal client, CLImat, share one daemon and queue. Source
builds use native FairPlay playback on Linux x86-64.

<p align="center">
  <img src="docs/screenshots/library.webp" alt="Slipmat showing a library of songs with the sidebar collapsed: the playing track marked in red, two Apple cannot stream greyed out, a menu button on every row, and the Now Playing bar along the bottom with the playing track's cover behind it">
</p>

## What it does

- **Two native clients.** Use the GTK4 and libadwaita app or run `climat` in a
  terminal. Both control the same player state.
- **Your full library.** Browse and sort songs, albums, artists and playlists.
  Playing a list turns it into the queue.
- **Gapless playback.** Open the full-size player with its queue and artwork.
- **Desktop controls.** Control playback from the GNOME top bar, lock screen or
  media keys. Music keeps playing after you close the window.
- **Apple Music search.** Search for artists, albums, playlists and songs, then
  play from their detail pages.
- **Fast startup.** Slipmat opens on its cached library and restores the last
  queue without resuming playback.

<p align="center">
  <img src="docs/screenshots/climat.png" width="720" alt="Climat playing Something Comforting by Porter Robinson, with its spectrum visualizer and queue visible in a terminal">
  <br>
  <sub>Climat, the terminal player</sub>
</p>

<table>
  <tr>
    <td colspan="2" width="33%">
      <img src="docs/screenshots/albums.webp" alt="The Albums grid in a dark theme: four covers to a row, each with its title and artist beneath, and the sidebar open alongside">
    </td>
    <td colspan="2" width="33%">
      <img src="docs/screenshots/artists.webp" alt="An artist page for the Foo Fighters: a round portrait pulled from the catalogue, their genre, and their four albums listed below">
    </td>
    <td colspan="2" width="33%">
      <img src="docs/screenshots/playlist.webp" alt="A playlist page: the four-up cover Slipmat composes from the first four tracks, the song count, Play and Shuffle, and the tracks below">
    </td>
  </tr>
  <tr>
    <td colspan="2" align="center"><sub>Albums, sidebar collapsed</sub></td>
    <td colspan="2" align="center"><sub>Artists, portraits from the catalogue</sub></td>
    <td colspan="2" align="center"><sub>A playlist with a cover built from its tracks</sub></td>
  </tr>
  <tr>
    <td colspan="3" width="50%">
      <img src="docs/screenshots/player.webp" alt="The expanded player: large artwork, transport beneath it, a 522-track queue alongside, and the cover blurred behind the whole surface">
    </td>
    <td colspan="3" width="50%">
      <img src="docs/screenshots/search.webp" alt="A catalogue search for the Beatles: artists, Apple Music playlists, albums and songs in one list, each opening a page of its own">
    </td>
  </tr>
  <tr>
    <td colspan="3" align="center"><sub>The player opened out, queue alongside</sub></td>
    <td colspan="3" align="center"><sub>Searching the whole catalogue</sub></td>
  </tr>
</table>

## Install

You need an **active Apple Music subscription** and **a network connection
whenever you play**. The native provider is prepared from this checkout. See
[Development](#development) for setup. Published releases that predate the
native backend use Chromium and download Widevine on first run.

### Arch and derivatives

```bash
yay -S slipmat        # latest release: the GNOME app
yay -S climat         # latest release: the terminal player
yay -S slipmat-git    # tracks main: the GNOME app
yay -S climat-git     # tracks main: the terminal player
```

Either client pulls in the shared playback daemon. The release and `-git`
packages conflict, following the AUR convention. `-git` packages build the
current `main` branch, which may contain unfinished work. The `-git` packages
support both x86_64 and aarch64. ARM64 users need them until v0.12 is released.

### Flatpak for any distribution

Download `Slipmat.flatpak` from the [latest
release](https://github.com/SoftARV/Slipmat/releases/latest), then:

```bash
flatpak install ./Slipmat.flatpak
flatpak run dev.miguelrincon.Slipmat
```

Starting with v0.12, choose `Slipmat-x86_64.flatpak` or
`Slipmat-aarch64.flatpak` to match your CPU.

A bundle contains the app but fetches its runtime during installation. Accept
the prompt to download GNOME 49 from Flathub. Declining it reports that the
matching `org.gnome.Platform` runtime was not found.
If your machine has no Flathub remote at all:

```bash
flatpak remote-add --if-not-exists --user flathub \
  https://dl.flathub.org/repo/flathub.flatpakrepo
```

Slipmat has no Flathub listing. Download its bundle from GitHub Releases.

The bundle includes the GNOME app, engine and sidecar. Install `climat` from the
AUR or build it from source.

### From source

```bash
sudo pacman -S --needed base-devel pkgconf rust gtk4 libadwaita librsvg \
                        nodejs npm libpulse
make install     # build + install to ~/.local, no sudo
```

The build requires Rust ≥ 1.93, the MSRV for relm4 0.11, and Node. I verified it
with Node 26. Launch **Slipmat** from the app grid or run `slipmat`. If your
shell cannot find that command, add `~/.local/bin` to your `PATH`.

`climat` installs alongside Slipmat and includes its own desktop entry. It opens
in a terminal but still needs a graphical session because the daemon runs
Chromium. A plain SSH connection to a headless machine cannot provide the
display server Chromium requires.

### First run

The first client you start opens Apple's sign-in window. The daemon hides it
after authentication and shares that session with both clients. Install the app
before testing notifications; GNOME needs its `.desktop` entry to display them.

## Limitations

- **Online playback only.** Apple Music requires a network connection to
  authorize and stream each track.
- **Native playback requires Linux x86-64.** This checkout's native provider
  uses the x86-64 FairPlay libraries.
- **Library browsing requires a desktop session.** The Chromium library sidecar
  needs a Wayland or X11 display, including when using CLImat.
- **Apple can change `music.apple.com`.** Slipmat uses a small hook to drive
  MusicKit. If Apple changes that interface, Slipmat reports the error instead
  of leaving you at a spinner.
- **A few library tracks may be unplayable.** Apple delists tracks while leaving
  them in your library. Slipmat remembers failed tracks across sessions and
  dims them so they cannot break a queue.

Known bugs live in [the issue tracker](https://github.com/SoftARV/Slipmat/issues).

## How it works

Both clients send playback and queue commands to `slipmatd`. The daemon
holds the queue, advances it after each track, and sends state changes to both
clients and MPRIS. A signed-in FairPlay provider opens the licensed ALAC stream.
Symphonia decodes the audio and sends PCM to the local PulseAudio-compatible
server. Chromium is muted and continues to supply web library sign-in and
MusicKit library operations.

Native and library sign-ins are separate. Slipmat opens the native sign-in
dialog when playback needs it. CLImat prompts you to run `climat sign-in`
from a second terminal. The selected queue item starts after successful native
sign-in. The native session ends when the daemon exits.

Native playback buffers each licensed track before opening the audio output.
Queue playback, pause, and skips work across both clients. Seeking, shuffle,
repeat, and gapless transitions are deferred. On restart, the daemon restores
the queue paused at the start of the selected track.

## Development

Prepare the provider from a desktop session with a local PulseAudio-compatible
socket. The script verifies the staged FairPlay libraries, builds the provider
and Symphonia output, and registers the launcher in the user's Slipmat state
directory. Run it again after changing the provider or moving the checkout.

```bash
make sidecar
sh scripts/fairplay-trial/prepare.sh
cargo build --locked --workspace
target/debug/slipmat                        # the GNOME app
target/debug/climat                         # the terminal player
```

Use `cargo clippy --all-targets -- -D warnings` and `make check` to verify
changes. `RUST_LOG=slipmatd=debug target/debug/slipmatd` shows daemon events
when troubleshooting a fresh run.

Both clients reuse a running daemon. Run `pgrep -x slipmatd` before testing a
new daemon build so an older process does not mask your changes.

Module headers in `src/` describe each file's responsibility. Longer comments
record edge cases and measurements. Merged PRs contain the reasoning behind
each change.

## Related

[Sidra](https://github.com/wimpysworld/sidra) and [Cider](https://cider.sh)
pioneered the castLabs Electron approach for Apple Music on Linux. Slipmat uses
their groundwork with a different interface.

## Licence

GPL-3.0-or-later. See [COPYING](COPYING).
