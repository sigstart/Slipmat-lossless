# CLAUDE.md

Project instructions for Claude Code. Read this fully before writing code.

A personal `CLAUDE.local.md` sits beside this file, untracked. It carries author
context and machine specifics; this file carries the project and its rules.

## What this is

**Slipmat** — an Apple Music client for Linux, written in Rust. The name is the
felt disc between the platter and the record: the layer that sits between the
mechanism and the music, and lets the record turn on its own terms.

**Why this project exists.** Apple ships no Linux client. Slipmat draws the
interface natively and plays licensed ALAC through Apple's Android FairPlay
libraries. GTK and CLImat share the same playback and account session in
`slipmatd`. See the [FairPlay provider spec](docs/specs/SPEC-fairplay-provider.md)
for the Rust provider. The phase 1 spec it replaced, and the Electron and
MusicKit JS player before that, are in the git history (`2995eeb`, `600f022`).

### Engine and clients

The engine and the interface are separate concerns, and the interface is no
longer only GTK:

- **The FairPlay provider** owns the authorized session and audio output.
  The shared daemon owns the queue and advances it on playback completion.
- **The GTK client** targets GNOME and should be indistinguishable from a
  first-party GNOME application. That is a rule *about this client*, not about
  the project: a design decision that would make it look like an Electron app or
  a generic Qt tool is the wrong decision **here**.
- **Other frontends** are legitimate. A terminal client is planned. Anything the
  GTK client knows that is not about drawing belongs lower down, where the next
  frontend can reach it.

The practical test for new code: **would a client that draws nothing still need
this?** If yes, it does not belong in `components/` or the reducer.
## Stack

Keep the existing dependencies pinned. The FairPlay prototype may add its own
playback dependencies.

| Layer          | Crate                  | Version                                    |
| -------------- | ---------------------- | ------------------------------------------ |
| UI framework   | `relm4`                | 0.11 (features: `libadwaita`, `gnome_49`)  |
| Widgets        | `gtk4`, `libadwaita`   | via relm4 (do **not** add directly)        |
| MPRIS          | `mpris-server`         | 0.10                                       |
| HTTP           | `reqwest`              | 0.12 (`json`, `rustls-tls`, no default features) |
| Serde          | `serde`, `serde_json`  | 1                                          |
| Async runtime  | `tokio`                | 1 (`rt-multi-thread`, `process`, `io-util`)|
| Logging        | `tracing`              | 0.1                                        |

Rust edition 2024, plus `anyhow` 1 (rule 5) and `tracing-subscriber` 0.3 with
`env-filter` for `RUST_LOG`. Toolchain ≥ 1.93 (relm4 0.11's MSRV); libadwaita
≥ 1.8 / GTK ≥ 4.20 (the `gnome_49` floor).

**Keep this machine on the same stable as CI.** Nothing pins a toolchain, so
both follow stable — and when they drift, `make check` passes here and the
identical command fails there, on a lint that does not exist locally. It cost a
red PR when `clippy::chunks_exact_to_as_chunks` arrived in 1.98 while this
machine was on 1.97.

**relm4 0.11's docs.rs build is broken.** Read the vendored source, which is the
exact version we compile against:

```bash
ls ~/.cargo/registry/src/*/relm4-0.11.0/src/
```

**relm4, not raw gtk4-rs.** Every component is a relm4 `Component` or
`FactoryComponent`. Reaching for `Rc<RefCell<>>` to share widget state is a sign
the state belongs in a model and the change belongs in an `update()`.

## Hard rules

### 1. Licensed playback

Playback requires an active Apple Music subscription and valid licences.
Slipmat may use Apple's Android playback libraries through libhybris, a
compatibility layer for Android code, and Google's Widevine CDM for songs with
no lossless master, hosted the way a browser hosts it. The playback backend can decode and
buffer audio in memory for immediate output. GTK, CLImat, and other clients
exchange playback commands, metadata, and status with the backend.

Preserve normal account authentication, subscription checks, and DRM licence
enforcement. Keep content keys private to the DRM component. Keep decrypted
tracks out of persistent caches, export interfaces, and client IPC.

### 2. Check the versioned MPRIS API before adding a call

Check the installed `mpris-server` source for the version in `Cargo.lock`.

### 3. The daemon owns and advances the queue

Clients send playback commands and reconcile the daemon's queue events by
occurrence ID. Native playback completion advances the queue in the daemon.

### 4. Report provider failures

Sign-in, authorization, and playback failures must surface as user-visible
state or errors.

### 5. No `.unwrap()` / `.expect()` outside `main.rs` and tests

The provider can die, the network can drop, Apple can 403 a stale token. Every
failure becomes an `adw::Toast` or a state, never a crash. `anyhow::Result`
internally.

### 6. Supervise the FairPlay provider

Keep the daemon responsive when the provider stops and let users sign in again.

### 7. Keep credentials private

Session credentials travel over the provider's private pipe and stay in daemon
memory. Keep them out of client events, logs, and error text. The daemon's
profile store encrypts saved session data using a key held by Secret Service.

### 8. Never block the GTK main thread

Use relm4 commands for HTTP and daemon I/O. Keep `update()` synchronous and fast.

### 9. Apple types must not leak into the UI

Map `api.music.apple.com` JSON into our own `Track` / `Album` / `Artist` /
`Playlist` / `Artwork` in `music/types.rs` at the boundary — "parse, don't
validate". The `view!` macro and `components/` see our types, never raw JSON
and never `reqwest`.

**This is about Apple's shapes, not about the module a type lives in.** A plain
enum like `RepeatMode` is ours; it sits in `protocol.rs` because that is where
the wire contract lives, and `components/` using it directly is fine. It used to
have a twin in `components/` for this rule's sake, and two identical enums with
a conversion at every boundary cost more than the rule saved.

### 10. Comments earn their place

The code says *what*. A comment says **why**, and only when the why is not
evident from the code beside it.

- **Six inline lines is the ceiling.** Past that it is usually two explanations,
  or one that belongs in the module header.
- **If the comment is longer than the code it explains, it is a commit message.**
  Measurements, log excerpts and what-was-tried-first belong in the commit, where
  `git blame` still finds them and the file stays readable.
- **One explanation per fact.** When a decision changes, *edit* the comment that
  is there. Never stack a new paragraph beside the old one.
- **Measured facts and traps stay** — stated once, in the fewest lines that carry
  the number.
- `//!` module headers carry architecture and are exempt from the ceiling.

The failure mode is sediment, not length. `queue_view.rs` once reached
**thirty-seven lines** saying one thing three times, the first ten describing
behaviour that no longer existed.

### 11. Make the code not need the comment

Rule 10 removes the crutch; this is why it is not missed.

- One concern per function. A function with section headings inside it is two
  functions.
- **Encode the rule in a type.** `Start` in `app/queue.rs` is the shape to copy:
  the enum is what stopped shuffle leaking between queues, not the paragraph
  above it.
- Name for the domain, not the mechanism.
- No abstraction before the third use.

## Current architecture

GTK and CLImat use a Unix socket to browse and control playback. The daemon
handles Apple Music API requests, queue state, and MPRIS. It starts the FairPlay
provider over private pipes. The provider uses the authorized Apple Music
session for licensed playback and returns catalogue credentials to the daemon.

### Where the ids come from, and why it matters

Apple has **two** id spaces and they are not interchangeable — a library id
(`l.…` for albums, `r.…` for artists) 404s against `/catalog`, and a catalog id
404s against `/me/library`. Every `Album` and `Artist` therefore carries a
`library: bool` set **at the parse site** by whichever client method fetched it,
never sniffed from the id's shape later. `PageKind` has separate variants for
the two, so the compiler asks which one you have before a page can be opened.

The same distinction already existed one level down: a library *song*'s own id
is not playable, and its `playParams.catalogId` is. That is why `Track` carries
both.

### Writing to the library — verified against Apple's docs, 2026-07-26

All three exist, all take the **Music User Token** we already hold, and all
answer **202 Accepted with an empty body** — "acceptable, may not have
completed", so the UI must not treat 202 as "it is now in your library" and
re-read rather than assume.

| Want | Call |
| --- | --- |
| Add to library | `POST /v1/me/library?ids[songs]=…&ids[albums]=…` — several types in one request |
| Favourite (the star) | `POST /v1/me/favorites?ids[<type>]=…` — **add only**, see below |
| Love / dislike | `PUT /v1/me/ratings/{type}/{id}`, body `{"type":"rating","attributes":{"value": 1 \| -1}}` |

**Un-favouriting is not possible over REST with this token.** Apple documents no
counterpart to the add, and `DELETE /v1/me/favorites?ids[songs]=…` — the obvious
inverse — answers `400 Insufficient Permissions`:
`'Favorites:DELETE:IdsQuery' entities require permissions that are not in the
request`. So the row menu offers no removal.

Worth knowing before anyone concludes it is impossible: **`music.apple.com` and
Apple's Android app can un-favourite**, so a route exists — just not this one.
The MusicKit JS route went with the Electron player. What is left to try is
whatever the Android client sends, through the provider's session. Untried.

**Favourites and ratings are different things.** `favorites` is the modern star;
`ratings` is the older love/dislike pair, whose only legal values are `1` and
`-1`, with *absent* meaning unrated — so removing a rating is `DELETE`, not a
value of `0`. Ratings come in catalog and library flavours per type (`songs`,
`albums`, `playlists`, `music-videos`, `stations`), which is the same
two-id-spaces trap as everything else here.

Add-to-library and favourites are wired to the row context menu. Ratings are
not — they are a third mechanism nobody has asked for yet.

**Nothing shows state.** A 202 means accepted, so a star drawn from it would be
a star that might be lying; showing it truthfully means reading it back per
track, which is a request per row. The menu therefore *acts* and toasts, and
says "Sent to your library" rather than "Added".

### Library song attributes, and what you have to ask for

`LibrarySongs.Attributes` is a **documented, closed list**, and two things about
it matter:

- **`dateAdded` cannot be had per song.** It is not in the dictionary, and
  `extend=dateAdded` does not produce it either — **measured as 0 of 541**
  against a real library. There is therefore no "Recently Added" *sort*:
  offering one that silently orders by title is worse than not offering it.

  There *is* `GET /v1/me/library/recently-added`, which returns a
  `ResourceCollectionResponse` — a mixed list of **albums and playlists** in
  recently-added order, the same thing Apple Music's own "Recently Added"
  screen shows. That is a plausible future *view*, not a sort of the songs
  list: it never promises individual songs, and it documents no `limit` or
  `offset`. Untried.
- **`inFavorites` *is* on it**, with `&extend=inFavorites` — **41 of 541**.
  Whether a track is starred comes back with the library, so a row shows it
  without a read-back or a request per row. This is the exception to "202 means
  you cannot know the state": for favourites on library songs, you can.

Both numbers came from a counter left in `all_library_songs`, which still logs
`starred` on every load. Two rounds were lost to guessing which attributes Apple
honours before anyone measured; the counter stays so the next question is
settled the same way.

Library membership is the same shape of fact: a track that came from
`/me/library/…` is in the library by definition, so `Track::in_library` is set
by the client method that fetched it — never guessed from the id.

**Apple now draws a cover for most playlists you made.** Measured on
2026-11-22 against a real library of 64 playlists:

| Artwork | Count | What it is |
| --- | --- | --- |
| Generated | 28 | `…/image/thumb/gen/1200x1200AM.PDCXS11.jpg?c1=…&t=…` |
| Uploaded | 6 | a picture you set, as a presigned `*.blobstore.apple.com` URL |
| Curated | 16 | Apple's and editorial playlists, as an ordinary template |
| None | 14 | still nothing, on both endpoints |

Until recently **every user-created playlist carried none**, on both the list
*and* `/me/library/playlists/{id}`, so this changed under unchanged code. A
generated cover is Apple's own gradient with the name printed on it: the colours
are `c1`–`c4`, the name is base64 in `t`, and the text colour is `tc`. **Every
generated cover has the same path**, so the query is the image. That is the
opposite of an uploaded cover, whose query is a signature that changes on every
fetch. `Artwork::cache_key` drops the query for blobstore URLs only; dropping it
everywhere once gave all 28 generated tiles one cache file, and they all showed
whichever playlist was fetched last.

For the playlists that still have none, the 2×2 mosaic of four covers that
`music.apple.com` shows is drawn by the web player, not fetched.
`include=catalog` cannot rescue it either, the way it rescues a library
artist's portrait: a playlist you made has no catalog twin to borrow from.

`all_library_playlists` logs both lists on every load, **named rather than
counted**, because a bare count answered the wrong question — it described the
list endpoint while the bug was on the details endpoint, and the two are
different requests that can carry different attributes.

Two traps here, both of which cost a round:

- **A screenshot in `docs/screenshots/playlist.webp` shows Slipmat rendering
  that mosaic for "Game on" in 0.2.0.** So Apple returned it at least once and
  no longer did, on identical code — `git diff v0.2.0..` over `client.rs`,
  `types.rs` and `pages.rs` is the rename and nothing else — and has since
  started sending generated covers instead. Treat playlist artwork as
  *inconsistent between accounts and over time*, and never conclude from one
  absence that a thing was never there.
- **Three separate silent exits** hid this: `fetch` and `decode` were `.ok()`
  and `and_then`, and `fetch_page_art` returned early on `None` artwork saying
  nothing. A page holding its placeholder therefore looked identical whether
  the fetch failed, the file was undecodable, or there was never a URL at all.
  All three now warn. **Cosmetic is not the same as unexplained.**

Drawing the mosaic ourselves means a compositor: take the first four tracks'
covers, tile them, cache under the playlist's own key. On a detail page the
tracks are already loaded and it costs nothing extra; in the grid it costs one
request per artless playlist, because a tile knows no tracks. With most of your
playlists now generated, it would cover the 14 that are left.

### Restoring the last session

Four files, three homes, and the distinction is not pedantry:

| What | Where | Why |
| --- | --- | --- |
| Preferences | `~/.config/slipmat/settings.ini` | somebody chose them |
| Unplayable ids | `~/.cache/slipmat/unplayable.json` | rederivable — Apple will tell us again |
| The library | `~/.cache/slipmat/library.json` | likewise, and 430 KB of it |
| The last queue | `$XDG_STATE_HOME/slipmat/session.json` | neither chosen nor rederivable |

The daemon writes and restores the session and the library cache. GTK only
reads them.

Two rules the restore follows:

- **It never resumes playing.** `Command::SetQueue` carries `start_playing`, and
  `restore_session` in `slipmatd/src/serve.rs` is the only place that sends
  `false`. An app that starts making noise because it was launched is hostile;
  the point is to remove the work of finding your place, not to take the
  decision away.
- **It saves on every track change as well as on shutdown.** Shutdown might not
  run — a crash, a `SIGKILL`, a session ending badly. Saving per track means the
  worst case is restoring the right track rather than restoring nothing.

**It restores the track, not the position within it.** `session.json` still
records `position_ms`, but the provider cannot seek yet, so the restored queue
starts its current track from the beginning.

**It saves each song's details, not only its id.** The restore runs before the
library has loaded, so a queue rebuilt from ids alone came back titled by its
own catalogue numbers — the Now Playing bar showed `693269578`. A queue still
holding those stand-ins saves no details, so they cannot become permanent.

**A loaded-but-never-started queue has no now-playing item**, and rendering that
faithfully left the Now Playing bar empty beside a full queue. So `showing()`
falls back to the queue's own current entry, which is the honest answer to
"what is this player on".

### Opening on the library rather than on a spinner

Once authorized, the daemon fetches all four collections at once and writes
them to `~/.cache/slipmat/library.json` through `library_cache.rs`. It publishes
`Event::LibraryChanged` only when the result differs from what it holds.
GTK reads the cache when the daemon first reports `Ready` and again on every
`LibraryChanged`, so a restored session shows the last launch's library while
the refresh runs.

Two things make it work, and both are load-bearing:

- **`showing_library()` is not gated on `Stage::Ready`.** Content already in the
  model is real content; the daemon still starting is a fact about the
  transport. `controls_live()` gates the sidebar, search and sort, so nothing
  can be *requested* with no session.
- **A refresh that changed nothing must not rebuild.** The daemon's
  `commit_library` skips the write and the event when nothing changed. GTK's
  `replace_changed` compares with `PartialEq` before it drops a section's built
  widgets, so the widgets and the scroll position stay as they are.

**Clearing the queue** is daemon state. It stops playback, empties the queue
and drops the saved session, because there is nothing to come back to.

**Clicking a track in the queue plays it.** `Command::ChangeToIndex` starts
playback even when the queue is loaded but idle. Moving the cursor silently
looked like nothing happened.

## UI shape

- `adw::ApplicationWindow` > `adw::ToolbarView` > `adw::HeaderBar`, inside an
  `adw::BottomSheet` whose **bottom bar** is the Now Playing strip:
  artwork, title + artist, a clock, prev / play-pause / next, and volume, with
  the track's progress as a thin line across the top of the bar. That line is
  **information, not a control** — a scale needs a grabbable handle and room to
  aim in, and the bar is what runs out of room first when the window is tiled
  narrow. Scrubbing lives in the drawer, which has the width for it; it stays
  insensitive while the daemon reports `PlaybackCapabilities { seek: false }`,
  as do shuffle and repeat. The bar is visible on every page — it is the app.
- Main content: `adw::NavigationView` over an `adw::ViewStack` (clamped) —
  **Library** (playlists / albums / songs) and **Search**. The queue lives in
  the drawer the bar opens into, not on a page.
- Rows are `adw::ActionRow` with artwork as prefix; activating a row sends
  `PlayTrack` with the **whole containing list** as the queue and the row's index
  as `start` — never a single-track queue (rule 3).
- **Lists are `gtk::ListView` via relm4's `TypedListView`, never
  `gtk::ListBox`.** A `ListBox` builds one real widget per row, so a
  541-track library plus a 530-track queue meant over a thousand live
  `AdwActionRow`s and the app felt heavy. `ListView` recycles — it keeps about
  as many widgets as fit on screen. The consequence: `RelmListItem::bind` must
  set **every** property it cares about, because the widget it is handed was
  showing a different track a moment ago, and anything left unset keeps the old
  value. Disconnect signal handlers in `unbind` or they stack up on reuse —
  unless nothing is captured that can go stale, which is what the queue's rows
  do instead: connect once in `setup` and read `ListItem::position()` at event
  time. **Except where the event is a person.** A popover or a drag is spent
  seconds later, by which time the list has moved, so those carry the row's
  identity and resolve it again.
- **Inside `bind`, touch only widgets you own.** Not `root.parent()` — that is
  GTK's `GtkListItemWidget`, and during a splice it is being recycled, so the
  reference you are handed can be the last one alive. Dropping it finalises the
  widget, which emits `unbind` on the factory, which asks relm4 for the item
  `bind` is *still holding mutably*: `RefCell already borrowed`, and the app
  aborts. Found from a line whose whole job was removing a CSS class.
- **A `#[watch]` on a control that also reports user changes is a two-way
  binding, and it will cycle unless you break it.** This one froze a desktop
  hard enough to need a forced power-off, so it is worth understanding rather
  than pattern-matching.

  GTK emits `value-changed` for a programmatic `set_value` exactly as it does
  for a drag — a widget cannot tell you which it was. And relm4 runs
  `update_view` after **every** message, including the one the widget just
  sent. So the sequence is: user moves the slider → the handler fires → the
  very next view update writes the *old* model value back into the widget →
  GTK emits for that → the two values ping-pong through the reducer forever.
  Each lap was one player command, one MPRIS property change on the bus and
  one journal line; it managed 5,721 of them.

  **Block the handler while you write.** That is the fix, and comparing values
  is not — which cost a whole evening to establish, because comparing values
  *looks* like it should work and even reduces the traffic.

  `sender.input` **queues**; it does not run inline. So the `value-changed` GTK
  emits for your own write is processed a lap later, against a model that has
  already adopted some other value. Every comparison then passes honestly,
  because by then the two really do differ. Measured on a held `Ctrl`+`Down`
  with the value comparison in place: **495,000 laps, one write per message**,
  the two values a `page_increment` apart, the adjustment provably the same
  object throughout. The component's task never yields, so the *app's* task
  never runs again — the window dies and commands stop reaching the daemon
  entirely, which is why the log goes quiet rather than filling up.

  So a two-way control needs the handler's `SignalHandlerId` kept, and
  `block_signal` / `unblock_signal` around the write — which means connecting
  in `init` rather than in `view!`, since the macro discards what `connect_*`
  returns. `now_playing::post_view` and `player_view::transport::refresh` are
  the two, and they must stay in step.

  Two things that are **true and not sufficient**, both of which shipped a
  version of this bug: "it only emits when the value actually changed" (the
  stale write *is* a change), and "only write when the widget disagrees" (it
  disagrees every lap, one message behind).

  Neither clippy nor the test suite can see this: the cycle only exists once
  GTK, `update_view` and the reducer are wired together. Verify a two-way
  control by counting what reaches the stream mixer —

  ```bash
  RUST_LOG=slipmatd=debug cargo run -p slipmatd >/tmp/vol.log 2>&1
  sed 's/\x1b\[[0-9;]*m//g' /tmp/vol.log | grep -c "mixer: set"
  ```

  Log to a file rather than the journal: a regression can produce thousands of
  writes before the app stops responding, and take journald down with it.

  Then hold the key down for several seconds and let go. A healthy run is one
  command per change and a **bounded** total — 207 for a long session — and the
  app still answering afterwards.

  **A frozen app does not fill the log, it stops writing to it.** The loop
  starves the app's own task, so commands stop reaching the daemon altogether:
  the signature is a burst that ends mid-stream and never resumes, not a count
  that climbs. When a log goes quiet, take a core dump (`ptrace_scope` is 1
  here, so `kill -ABRT` and `coredumpctl debug` — see the wedge recipe in the
  animation section) rather than concluding nothing happened.
- **A property that drives an `AdwAnimation` is written on an edge, never on a
  level.** This is the trap next door to the one above, and the one above is
  what makes it easy to get wrong: that rule is about a *feedback loop*, and
  this has no loop at all.

  `#[watch]` re-asserts its value after **every** message, and during playback
  those never stop — the daemon sends a snapshot twice a second. Free for a
  label. For an animated property it is a request to start-or-re-aim an
  animation, forever, at whatever rate messages happen to arrive. It wedged the
  app at 100% CPU inside `adw_spring_animation_set_value_to`, reached from
  `update_view` while handling a routine token refresh.

  Three properties are governed by this, and all three are written only from
  `sync_animated`: the sidebar's `show-sidebar`, the drawer's `open`, the bar's
  `reveal-bottom-bar`. Two details make it actually hold:

  - Compared against **what was last written**, not against the widget. A
    widget and a model that disagree persistently disagree on every message
    too, so asking the widget is the level trigger again wearing a guard's
    clothes.
  - Synced on **both** update paths. Daemon events arrive through
    `update_cmd_with_view`, not `update_with_view`, and one of them is what
    moves `stage` to `Ready` and reveals the Now Playing bar. Syncing on one
    path left the bar hidden for a whole session.

  **Diagnosing a wedge, and the one measurement that splits it:** a GTK layout
  loop logs *nothing*, because no message is being processed. A runaway reducer
  (#37) logs a dispatch per lap. One symptom, opposite causes, and the log
  tells them apart before you touch anything else.

  `ptrace_scope` is 1 on this machine, so a debugger cannot attach to a process
  it did not start. Make the kernel write the core instead:

  ```bash
  kill -ABRT $(busctl --user call org.freedesktop.DBus /org/freedesktop/DBus \
    org.freedesktop.DBus GetConnectionUnixProcessID s dev.miguelrincon.Slipmat \
    | awk '{print $2}')
  coredumpctl debug --debugger=gdb --debugger-arguments="-batch -ex 'bt 30'"
  ```

  Both diagnoses of this bug came from that, and neither from reasoning.
- **The app stylesheet is `style.rs`, and it is the sanctioned CSS.** An accent
  colour is not a widget: libadwaita 1.6 exposes it only as CSS variables
  (`--accent-bg-color` and friends), so a `CssProvider` is the only route.
  Two providers, kept apart — a **base** one replaced when the accent
  preference changes, and a **backdrop** one carrying the cover behind the
  player, replaced on every track — so a new cover does not reparse the accent
  rules. Anything else wanting CSS still has to argue for itself first.
- **The player's backdrop is the cover, and the blur is the upscale.**
  `artwork::backdrop` writes a 48px copy beside the cover and CSS stretches it
  across the surface. GTK interpolates when it scales a texture, so it arrives
  soft on its own: no blur pass, no shader, nothing per-frame. A real Gaussian
  would be a CPU convolution per track change for a result nobody could tell
  apart once it is behind a scrim.

  The scrim is `@window_bg_color`, **not black**. Every label and icon on both
  surfaces has a colour chosen for contrast against the theme, so a photograph
  behind them would be guessing — and a dark veil would only guess right in one
  theme. The bar and the drawer share one rule and differ by two numbers: the
  bar's veil is lighter, because a thin strip shows less of the record, and its
  image is sized `cover` rather than 150%, because a square already overflows a
  wide strip's height and needs nothing more. Both are centred explicitly: that
  used to fall out of an animation, and the CSS default is the top-left corner.

  **It does not move.** It drifted once — `background-position` panned across 54
  seconds, `infinite alternate` — and that cost **~20% of a core, permanently**,
  because an infinite CSS animation never lets the frame clock stop (#126). The
  effect was deliberately tuned below perception and it worked: nobody had ever
  noticed it. A fifth of a core is a lot to pay for motion designed not to be
  seen.

  Cross-faded between tracks with CSS `cross-fade()`, rewritten per frame — a
  provider that is *replaced* is not a state change GTK will transition
  between, so the interpolation has to be ours. Same clock as everything else
  that changes with the cover; two readings of one sleeve must not finish apart.

  This replaced a **tonal scrim**, a flat wash of the sleeve's dominant colour.
  The colour analysis went with it — `artwork::tint`, `dominant`, `hsl`, `rgb`
  — and is in the history if the neutral scrim ever wants tinting.
- **Use libadwaita widgets, not raw GTK.** `adw::ActionRow`,
  `adw::PreferencesGroup`, `adw::AboutDialog`, `adw::StatusPage`,
  `adw::ToastOverlay`. That's where the native feel comes from. No custom CSS
  unless there is no libadwaita widget for the job — say why before adding any.
- **First run**: a **modal that cannot be dismissed**, wearing the app's own
  icon, saying what Slipmat is and that it needs an active subscription. Its
  button opens the native sign-in dialog (`app/sign_in.rs`): Apple Account,
  password, then a six-digit verification code if Apple asks for one. The
  credentials go to the daemon and on to the provider (rule 7). Any other
  verification challenge ends the attempt with an explicit error.

  Blocking is the correct behaviour, not a nicety. Signed out, every control in
  the app is a control that cannot work: the sidebar sections fire library
  loads, the search box queries a catalog that answers 403, and the transport
  talks to a player with no session. Leaving them reachable once produced a 403
  per second against Apple, because every token update re-ran the auto-load.

  Raised and lowered from **one** place (`sync_onboarding`, called after every
  message), because several paths change `stage` and most of them would be easy
  to forget. Once the daemon restores a saved session it never shows. Never
  re-show it except on explicit Sign Out → Sign In.
- **Sign Out** lives in the primary menu, in its own section — an account action
  is not app furniture and should not sit beside About. It asks first, because
  it drops Apple's session and getting back in means the password and whatever
  verification code that involves. The daemon then forgets *everything* that
  belonged to that account, for every client: library, queue, saved session,
  the bar — **and `library.json`**, which is the same music written down. GTK
  also drops its grids, catalog results and pushed pages. The unplayable-id
  cache stays, because that is a fact about Apple's catalog rather than about
  the user.
- Starting the playback engine, connecting, signed out, playback unavailable,
  no results: distinct `adw::StatusPage`s. Errors: `adw::Toast`.

## MPRIS (the v1 bar)

`org.mpris.MediaPlayer2.Slipmat`, via `mpris-server` 0.10, exported by
`slipmatd` (`bus.rs`) so it works with no window open.

- Metadata: `mpris:trackid`, `mpris:length`, `mpris:artUrl`, `xesam:title`,
  `xesam:album`, `xesam:artist`, `xesam:trackNumber`.
- `mpris:artUrl` **must be a `file://` path** — the Shell will not fetch an
  `https://` URL reliably. Apple serves artwork as a *template* URL containing
  `{w}x{h}bb.jpg`; substitute the size, fetch, and cache under
  `~/.cache/slipmat/artwork/` keyed by catalog id. That is what `artwork.rs` is
  for.
- Bidirectional: `PlayPause` / `Next` / `Previous` / `Seek` / `SetPosition` /
  `Volume` from the Shell must reach the daemon's queue, and every playback
  event must update the exported properties. Half-working MPRIS is worse than
  none — the shell shows controls that lie. That is why `CanSeek` is false
  while the provider cannot seek, rather than accepting a seek and ignoring it.
- Emit `Seeked` on discontinuous jumps, and keep `Position` honest between
  events with a 1s tick that is **removed when paused**.

## Where it is now

`0.13.0-dev+alac`, after the move from the Electron player to the FairPlay provider.
The library, catalog search, the queue, sign-in, session restore and MPRIS work
through the daemon. Playback through the provider is verified for sign-in and
restore, and licensed output plays live — tracks past 21 minutes included, now
that key contexts are refreshed. Licence denial still needs live verification. Songs with no lossless master
play as 256 kbps AAC through Google's Widevine CDM, which is opt-in
(`make widevine`); the bar says Lossless or AAC. Seek,
shuffle, repeat and gapless transitions are not implemented yet (see
[Known issues](#known-issues)).

Packaging follows: the AUR has the `-git` split packages only, and the Flatpak
builds in CI with no provider inside it. The history of how is in the git log
and the release notes, which is where it belongs rather than in a checklist
nobody updates.

See [current architecture](#current-architecture).

**Stay lean — flag the drift, don't gatekeep.** Not the default focus: lyrics,
Discord presence, podcasts, radio, multi-account, an equaliser, scrobbling,
cross-platform. Rule 1 defines the boundary for playback and media handling.
When a change drifts, **name the cost and the direction** so it is a
conscious choice — then build it if it genuinely earns its place.

### Lists, and where they live

Each list keeps its widgets in its own registry, and they share state deliberately:

| List | Registry | Model |
| --- | --- | --- |
| Results (library or catalog) | `AppModel::library_icons` | `TypedListView`, **virtualised** |
| Queue sidebar | `QueueView`'s bound-row list | `TypedListView`, virtualised |
| Album / artist page | the page's own | `TypedListView`, **not** virtualised |
| Albums grid | `AppModel::album_art_widgets` | `TypedGridView`, virtualised |
| Artists grid | `AppModel::artist_art_widgets` | `TypedGridView`, virtualised |
| Playlists grid | `AppModel::playlist_art_widgets` | `TypedGridView`, virtualised |

`CurrentTrack` and `DeadTracks` are shared by every list **except the queue**;
the widget registry is always **per list**. Those registries are keyed by
catalog id, so one shared registry would have the same song on a page and in the
results behind it overwrite each other's entry — the marker would land on
whichever bound last. `set_row_playing` asks every registry instead.

**The queue is the exception, because a catalog id cannot address a queue row.**
A queue may hold the same track twice — Play Next and Add to Queue are what put
it there (#88) — so an id marks both copies, and "which one is playing" has no
answer in that space. Its registry is a plain list of the rows currently bound,
identified by their `GtkListItem`, and the marker is a **visible position** in a
shared cell. Same principle as `CurrentTrack`, different key, and the reason it
had to be different is the duplicate.

The grids' registries hold `gtk::Image`s rather than row widgets, and they are
keyed by the **artwork's** cache key rather than by a catalog id — a tile that
requested a cover has very likely been recycled onto a different album by the
time the file lands, so the arrival is delivered to whichever tile is showing
that artwork *now*, or to none. The disk cache (`AppModel::tile_art`) is shared
across both grids, because it is keyed by the image itself.

**Whether a view is actually virtualised is measurable, not a matter of
opinion.** `RelmListItem::setup` runs once per *widget*, not once per item, so
`components::count_widget` counts them and logs at `trace`:

```bash
RUST_LOG=slipmat=trace cargo run 2>&1 | grep "list widget built"
```

Scroll a 500-track library and watch where the count stops. A few dozen means
recycling. Five hundred means every row is real, and something upstream — a
`Clamp` in the wrong place, a `Box` between the view and its `ScrolledWindow` —
is asking the view for its full height.

**`AdwClampScrollable`, not `AdwClamp`, around a scrollable child.** A plain
clamp does not implement `GtkScrollable`, so putting one between a
`ScrolledWindow` and a `GtkListView` breaks the list's height allocation and it
stops materialising rows part way down. Moving the clamp *outside* the scroller
fixes that and costs something else: the clamp is then what the window sizes,
so the scroller is only as wide as the clamp and its scrollbar sits in the
middle of the window instead of at the edge. `AdwClampScrollable` passes the
scrollable interface through to its child and settles both — verified the only
way that matters, by counting widgets: 205 for 534 tracks, unchanged.

A `GtkScrolledWindow` that spans the window then paints the `view` background
across it, a shade darker than the window. That was invisible while it was
clamped and centred, because it read as the list's own surface.
`.plain-scroller` in `style.rs` clears it.

**And a `bottom_bar` is drawn over the content, not beside it.** The Now
Playing bar is `AdwBottomSheet`'s, so the last row of every scrollable sat
behind it — *reachable*, since the scroller had already run to its end, and
invisible, which is the worst pair because nothing suggests there is more to
see. Maximising appeared to fix it and sent one diagnosis chasing a ten-pixel
measurement discrepancy that was real and irrelevant. `bottom-bar-height`
notifies, so the content's bottom margin follows the bar rather than guessing
at a height that changes with theme and text scale. It is set once, on the
sheet's content, which is every scrollable in the app at one stroke — the queue
is the only one outside it, and it lives in the drawer, above the bar.

**A rebuild is expensive, so do not do one that changes nothing.** Every tile
that binds decodes its cover on the GTK thread — measured at **2.5ms per
cover** — and a rebuild binds far more of them than are visible. Switching
sections used to rebuild unconditionally, so returning to a section already on
screen cost ~500ms of re-decoding every time.

Each section now records the fingerprint its widgets were built for (the query,
plus sort and direction for the songs list) and returns early when it already
matches. Anything that changes the underlying data sets the fingerprint to
`None`. The first build of a section still costs its ~500ms, once, behind the
spinner that is already showing.

`RUST_LOG=slipmat=debug` reports `rebuild what=… ms=…` and `section switch`, so
this stays measurable rather than remembered.

A pushed page is not virtualised on purpose: its list sits in a `Box` under the
header so the header scrolls with the content, which means GTK asks the list for
its full height. An album is a dozen tracks. That is the right trade.

## Known issues

Kept even when empty, because a section that silently stops being updated reads
the same as a project with no bugs.

- **The provider cannot seek, shuffle, repeat or play gaplessly yet.** The
  daemon announces `seek: false, shuffle: false, repeat: false` and drops those
  transport commands. Both clients and MPRIS grey the controls out rather than
  offer ones that lie. A restored session therefore starts its track from 0.
- **Licence denial** still needs a live run (see the
  [provider spec's live qualification](docs/specs/SPEC-fairplay-provider.md#live-qualification)).
  Licensed output has had one: hours of it, past the 15-minute key expiry below.
- **No rate ceiling on client commands.** The per-kind ceiling went with the
  sidecar (#37), so a client message loop reaches the daemon at full rate, a
  journal write and a bus round trip per lap. Block the handler (UI shape) and
  count what reaches the mixer.
- **Google's terms forbid using the Widevine CDM without an agreement with
  Google.** It is fetched only on request and never packaged; see the
  [provider spec](docs/specs/SPEC-fairplay-provider.md#songs-with-no-lossless-master-widevine-aac).
- **Removing library songs and un-favouriting** have no route (see
  [Writing to the library](#writing-to-the-library--verified-against-apples-docs-2026-07-26)).

**Fixed, and left here because the reasoning is still load-bearing:**

**A list loses its scroll position when the row holding keyboard focus is the
one removed or moved** (#6). Not when any row is removed — only the focused one,
and clicking a row or starting a drag on it is what gives it focus. Measured on
GTK 4.22 by driving identical edits with the viewport parked 200 rows down:
untouched at 9505 with nothing focused, zero with the row focused, untouched
again with focus dropped first. The fix is one line before the edit —
`components::drop_focus` — and it replaced an anchor-and-restore that could only
ever correct the jump after it had shown.

Two things made this expensive to find, and both are worth remembering. The
adjustment is **correct for the first frame** and collapses ~50ms later, so every
attempt to restore the value ran too early and corrected a number that was still
right. And `set_focus_on_click(false)` on a row's own buttons cannot help,
because the focus that matters belongs to the `GtkListItemWidget` — GTK's, not
ours. The corollary is the rule `reconcile.rs` is built on: **a rebuild loses the
scroll unconditionally**, focus or no focus, so every list edit has to be a
splice.

**A track over 15 minutes died at the same second every time** — *Thick as a
Brick*, part 1, at 16:27, three runs in three. FairPlay key contexts go bad
about 15 minutes after they are opened and **say nothing**: no `LEASE_END`, no
stale status, every decrypt still returning 0 while producing noise. The first
thing to notice is the ALAC decoder, on fragment 66's first frame. The fix is
`hybris_refresh_keys` every 10 minutes (the provider spec's *Silent expiry*).

The code already retried a lease refresh, which is why the first guess — a
renewal mid-song — was set aside. It was right: that retry fires only when the
library reports a stale context, and it never does. Reopening a key without the
refresh is a no-op too, because the library hands back its cached context.

What found it: **`climat check` decrypted the same file in 5 s and it played
through.** That one comparison moved the question from the file to the clock,
after a day spent on the file — a skipped key tag, a second sample description,
samples encrypted in pieces, the CDN — each ruled out by measurement. And the
wrapper reports every lease event on the provider's stderr, which used to go to
`/dev/null`; `provider.rs` now forwards an allowlist of the wrapper's own lines,
because Apple's code shares that stream with free text nobody has read.

## Commands

```bash
sh scripts/fairplay/prepare.sh  # fetch and verify the FairPlay libraries
make widevine                   # opt-in: Google's CDM, for songs with no lossless master
cargo build --locked --workspace
target/debug/slipmat
target/debug/climat
target/debug/climat current     # the playing track's catalogue id; `queue` lists all of them
target/debug/climat check <id>  # decode a track with no sound, in seconds: does it play through?
make check
```

`tracing` writes to **stdout**, so a daemon log needs both streams:
`RUST_LOG=slipmatd=debug target/debug/slipmatd >/tmp/slipmatd.log 2>&1`.
Redirecting only stderr leaves the file empty.

## Conventions

- `cargo clippy --all-targets -- -D warnings` is the bar, not `cargo build`.
- **File size is a review question, not a check.** There was a line-counting
  ratchet; it was removed once rule 10 made it perverse — deleting a comment
  freed budget for code, which is backwards. What it was guarding against is
  real, though: `mod.rs` grew from a post-split 1500 lines back past 2800 before
  anyone looked. Rule 11's placement rule is what has to catch that now, in the
  diff that causes it.
- Commits: conventional commits (`feat:`, `fix:`, `refactor:`, `chore:`).
- **Licence: GPL-3.0-or-later.** Full text in `COPYING`; declared in
  `Cargo.toml`. Every source file carries the two-line SPDX header
  (`SPDX-FileCopyrightText` + `SPDX-License-Identifier: GPL-3.0-or-later`).
- App ID: `dev.miguelrincon.Slipmat`. It must match the `.desktop` file name, the
  GResource prefix (`/dev/miguelrincon/Slipmat/`), `RelmApp::new()`, and the
  MPRIS bus name suffix. The app is called **Slipmat** in the window title and
  `.desktop` `Name=`.
- Versioning: SemVer in `Cargo.toml`; `main` carries a `-dev` pre-release.
  Releasing is: bump to the release version, tag `vX.Y.Z` on the merge commit,
  then set `main` back to the next `-dev`. There is no CHANGELOG — the release
  notes are written from the merged PRs, which is where the reasoning already
  is.

### Comments

See **rule 10** and **rule 11**. The short version: why not what, six inline
lines, and anything longer is a commit message.

### The README is not a feature list

`## What it does` says **six** things, and that is the whole budget:

1. two native clients, GTK and the terminal, on one player
2. the library — songs, albums, artists, playlists
3. a player worth the name (gapless once the provider can do it)
4. controls from the shell, the lock screen and the media keys, and playback
   that survives closing the window
5. all of Apple Music, searchable
6. quick, and out of the way

It grew to eight dense paragraphs once — MPRIS, the queue view, and "the GNOME
furniture" (preferences, shortcuts, About) each had their own — and reading it
felt like an inventory rather than a reason to install anything. Preferences are
table stakes; nobody chooses a music player for its About dialog.

**A new feature earns a line only if it changes what the app is for.** Not "it
is new", not "it was hard" — the test is whether somebody deciding between this
and the alternatives would want to know. Most things belong in the release notes
instead, which are written from the merged PRs and where the reasoning already
is. When something does earn a line, take one out or fold it in; six is a
ceiling, not a starting point.

Say it the way a user would. **Never "MPRIS"** — that is a bus name, not a
feature. It is "play, pause and skip from the GNOME top bar, the lock screen, or
your media keys", because that is the thing they can picture.

## Agent skills

### Issue tracker

Issues and specs are tracked in GitHub Issues. See `docs/agents/issue-tracker.md`.

### Triage labels

Use the five canonical triage labels without overrides. See `docs/agents/triage-labels.md`.

### Domain docs

This repo uses a single-context domain doc layout. See `docs/agents/domain.md`.
