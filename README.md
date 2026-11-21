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

Slipmat needs Linux x86-64, an active Apple Music subscription, and a network
connection. Prepare the native provider from the checkout before launching
either client. See [Development](#development) and the
[FairPlay staging guide](vendor/fairplay/README.md).

On Arch, the development AUR packages are `slipmat-git` and `climat-git`.
Either client pulls in the shared daemon. The native provider still needs the
local preparation step.

### From source

Install Rust ≥ 1.93, the GTK and libadwaita development packages, libpulse,
a C/C++ toolchain, Go, Git, and Bubblewrap. Linux Secret Service and
`secret-tool` enable session restoration. Run `make install` to install GTK,
CLImat, and the daemon under `~/.local`.

### First run

Sign in from the GTK dialog or run `climat sign-in` in a terminal. Both clients
use the same session for playback, search, and library access. CLImat uses
**Shift+A** to add a song and **Shift+F** to favourite it.

## How it works

`slipmatd` owns the queue, catalogue and library requests, and MPRIS. The native
provider opens the licensed ALAC stream, and Symphonia decodes it for the local
PulseAudio-compatible server. With Secret Service available, the provider can
restore an encrypted session profile after a daemon restart.

Queue playback, pause, and skips work across both clients. Seeking, shuffle,
repeat, gapless transitions, removing library songs, and unfavouriting are
currently unavailable. The Flatpak is a build artifact pending a native
provider setup for that distribution.

## Development

Prepare the provider from a desktop session with a local PulseAudio-compatible
socket. The script verifies the staged FairPlay libraries, builds the provider
and Symphonia output, and registers the launcher in the user's Slipmat state
directory. Run it again after changing the provider or moving the checkout.

```bash
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

Slipmat builds on [Adam](vendor/fairplay/README.md)'s native
playback work. [Sidra](https://github.com/wimpysworld/sidra) and
[Cider](https://cider.sh) helped establish Apple Music clients on Linux.

## Licence

GPL-3.0-or-later. See [COPYING](COPYING).
