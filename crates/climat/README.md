<!--
SPDX-FileCopyrightText: 2026 Miguel Rincon
SPDX-License-Identifier: GPL-3.0-or-later
-->

<p align="center">
  <img src="../../data/icons/hicolor/scalable/apps/dev.miguelrincon.Climat.svg" width="96" alt="The climat icon: the Slipmat record's pattern, pixelated, on a near-black ground">
</p>

# climat

Apple Music in a terminal, in the shape of Winamp.

`climat` is a client of `slipmatd` — the same daemon the GTK app talks to. It
draws what the daemon says and sends what the keyboard asks for, so a terminal
and a window open at once are two views of one player rather than two players.

```
  CLImat
  I Bet You Look Good on the Dancefloor  —  Arctic Monkeys
  Whatever People Say I Am, That's What I'm Not
              ▅▅▅▅▅▃▃   ▅▅▆▆ ▅▅▂   ▁       ▁▅            ▃▁▃          ▁
         ▇▇▇▇▇███████▆▆▆████████▆▇▇█▃▄▄█▆█▂██▇█▄▆▅▇▇▆▇▇▅▇███▇▅▅▆▂▁ ▄▇▂█▅▄▁▃▁▂ ▁▁
  ▄▄▄▄▄▄███████████████████████████████████████████████████████████████████████▆▄▃

  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━────────────────────────────
  ▶  Playing                                                          1:42 / 2:54
  shuffle on   repeat off   vol ████████░░

  SONGS   albums   artists   playlists   apple music   queue ───────────── [↓ title]
  ┌── filter the library ──────────────────────────────────────────────────┐
  │ mus▌                                                                   │
  └────────────────────────────────────────────────────────────────────────┘
    Addicted To a Memory (feat. Bahari)     Zedd — True Colors
    Aftershock (feat. Jacquie Lee)          Cash Cash — Blood, Sweat & 3 Years

  [space] play/pause   [↑↓] move   [↵] play/open   [⇥] tabs   [/] filter   [q] quit
```

## First run

Prepare the x86-64 provider using the [source setup instructions](../../README.md#development).
Run `climat sign-in` to sign in to the shared Apple Music session. The daemon
loads your library after sign-in. In the browser pane, use `R` to refresh,
`Shift+A` to add the selected song, and `Shift+F` to favourite it.

### Sign-in commands

When CLImat shows `Run climat sign-in`, run that command from a second terminal.
The queue starts the selected track after sign-in. Your Apple Account email and
verification code are visible as you type. The password stays hidden. Press `Ctrl+C` to cancel.

`climat sign-in-status` reports whether an Apple Music session is active,
and `climat sign-out` ends the session. Run `climat` to open the player UI.

### Track commands

`climat current` prints the playing track's Apple Music catalogue id, then its
title and artist. `climat queue` lists the whole queue the same way, with `>`
beside the current track — play a playlist, then list the queue, to get every
id in it.

`climat check [id]` downloads, decrypts and decodes a track with no sound, as
fast as it downloads, and reports whether it plays through. Without an id it
checks the current track. It stops playback first, and the stopped track starts
again from its beginning. When a track fails, the second line is what the player
saw of the file; the command exits non-zero, so it can be scripted.

## The bars

CLImat displays the visualiser by reading a monitor source, the loopback every output
exposes. That is how `cava` and every other visualiser works, and it means the
bars follow whatever is playing, including a track the GNOME window started.

**Only while Slipmat is playing.** A sink's monitor carries the whole machine
mixed together. The record stream asks to be narrowed to the FairPlay
provider's own sink-input — real PulseAudio honours that; `pipewire-pulse` does not, measured —
so what actually keeps a notification chime out of the bars is the player's own
state: no bars unless Slipmat is playing.

One case is left, and it is the honest one to leave: while Slipmat *is* playing,
anything else the machine makes is mixed into what the visualiser reads. Fixing
that properly means PipeWire's own API rather than its PulseAudio emulation,
which is a different backend for a decoration.

It talks PulseAudio rather than PipeWire, because PipeWire ships
`pipewire-pulse` and answers to it — one backend covers both servers. If there
is no audio server to listen to, the bars simply never appear and nothing else
changes.

The windows overlap, so the bars move about every 12ms while each transform
still looks at 46ms of audio — the frame rate and the frequency resolution are
separate things. Two rows rather than one, because a block character gives
eight heights and a bar has to cross an eighth of its range before anything
changes on screen — and coloured from the accent at the bottom to white at the
top. It grows taller with the window — a share of the height, the same way it
takes the width — because the gradient is only worth having once there are rows
for it to run over. Measured on a release build at 1.1% of a core playing, 0.1% paused.

The bars and the seek bar both take the width they are given, the way the lists
do. What is analysed does not change with it: the audio side produces a fixed
set of bands and the drawing side folds them into however many columns there
are, keeping the peak of each — an average is what turns a spectrum into a
smooth hump.

### The `ijkl` cluster

```
      i           move the row up
  j   k   l       queue next · move it down · queue last
```

`i` above `k` is up above down; `j` before `l` is sooner before later. Queueing
works in the library tabs, reordering in the queue — press one in the wrong
place and it says so rather than doing nothing.

## Colours

The accent is Apple Music's red, and the spectrum runs green through yellow to
orange the way Winamp's did. Both are fixed. **Everything else is mixed from your
terminal's own background**, which climat asks for on startup (`OSC 11`) — so
the greys lean the way your theme does instead of being a warm grey chosen
against somebody else's. A terminal that does not answer the query costs a
tenth of a second and gets the original palette.

## Keys

| Key | What it does |
| --- | --- |
| `space` | Play / pause |
| `z` `b` | Previous / next track |
| `←` `→` | Seek five seconds, once the daemon supports seeking |
| `s` `r` | Shuffle / repeat, likewise — both are off for now |
| `-` `=` | Volume — the same one your desktop's audio panel shows |
| `1` – `4` | Songs · albums · artists · playlists |
| `5` | All of Apple Music |
| `6` | The queue |
| `⇥` | Walk the tabs |
| `/` | Filter the current list, or search Apple Music |
| `o` | Order by the next thing this section can be sorted by |
| `t` | On Apple Music: what kinds of thing to search for |
| `O` | Turn that order round |
| `esc` | Out of a page, then out of a filter |
| `↑` `↓` | Move the cursor |
| `Home` | Put the cursor back on the playing track |
| `↵` | Sign in when prompted; otherwise play the selected track or open its album |
| `j` | Queue it to play next |
| `l` | Queue it at the end |
| `d` | Remove it from the queue |
| `i` `k` | Move it up / down in the queue |
| `q`, `Ctrl+C` | Close CLImat and keep playback running. Use `Ctrl+C` while typing in a filter. |

CLImat reopens on your last browsing tab or detail page and restores your
filter and highlighted row.

A `▸` marks a row that opens a page rather than playing. A rule runs to the far end of the strip, carrying whatever
that tab has to say about itself — the order on a library section, the number of
tracks on the queue, nothing on Apple Music.

**Each section keeps its own order**, because the keys differ with the data: an
album has a year and a date added, a playlist has neither an artist nor a year,
and a library artist carries only a name — so `o` there says there is nothing
else to sort by rather than pretending. Songs have no date added at all, which
is measured rather than assumed: 0 of 541 carry one.

`j` and `l` put a track in the queue without disturbing what is playing —
straight after the current track, or at the end. They work on songs; an album
has no playable id of its own, so open it first and queue from inside.

**Each tab keeps its own text.** A filter belongs to the list it was typed for,
the same way a sort does — so narrowing Songs leaves Albums alone, and coming
back finds it as you left it. Apple Music keeps its query too, because that one
is a question already answered and losing it would mean asking again. The queue
has no field at all.

`/` opens a field under the tabs, the width of the window, whose own border says
what typing there will do — filter the library on a library tab, search Apple
Music on that one. Two different questions, and the answer changes with the tab.

Searching Apple Music asks for everything by default — a few artists, playlists
and albums, then songs — and `t` narrows it to one kind, shown where a library
section shows its order. More results arrive as you scroll: a page is asked for
before the cursor reaches the end, because a round trip to Apple takes long
enough that asking at the last row means stopping at the last row.

**Who answers decides what a keystroke costs.** Over the library the list
narrows as you type, which is affordable because it never reaches Apple — the
daemon replies from the library it already holds, a round trip to a local
socket. Over Apple Music every query is a real request to somebody else's API,
so typing only edits the text and `↵` is what sends it. Same box, same key, two
rules, and the bottom row says which one is in force.

Each key is bracketed, so it reads as something to press rather than as the
first word of its own label. The bottom
row changes with which pane has focus — only the queue reorders,
only the library filters — and shows the keys that fit, dropping the least essential first —
so a narrow window loses the reorder hints first. The quit hint stays visible.

**One pane, six tabs.** The queue is somewhere you go rather than something
always on screen — a permanently visible queue costs rows every moment nobody
is reading it, and on a short window it and the library were both too small to
use. `6` goes there and any other tab key comes back — it gets no hint of its own,
because `1-6` already says it.

Nothing here edits the queue directly. A key sends a request and the rows move
when the daemon echoes, which is what keeps a terminal and a GTK window from
disagreeing about what is playing. The cursor is the exception: it is where
*this* terminal is looking, so it moves the instant you press a key.

## Running it

```
cargo build -p slipmatd
cargo run -p climat
```

CLImat starts the daemon on the first connection, including when you run
`climat sign-in` before opening the player. It looks for `slipmatd` beside the
CLImat binary and then on `PATH`.
