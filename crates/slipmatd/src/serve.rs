// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Playback and Apple Music services over the daemon socket.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use slipmat_core::artwork;
use slipmat_core::auth_wire::{Reader, verify_peer};
use slipmat_core::catalog;
use slipmat_core::entry::Entry;
use slipmat_core::ipc::{
    self, CatalogFilter, Event, PageKind, Request, Stage, Transport, WriteAction,
};
use slipmat_core::music::client::Client;
use slipmat_core::music::types::Artwork;
use slipmat_core::player::protocol::{Command, Item};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixStream, unix::OwnedWriteHalf};
use tokio::sync::broadcast;

use crate::bus;
use crate::library::Library;
use crate::state::Model;

/// How many events a slow client may fall behind before it is dropped.
///
/// A client that cannot keep up with a 500ms tick is not going to catch up, and
/// holding the backlog for it would make every other client's memory its
/// problem. `broadcast` tells it how far it lagged; it can ask for a snapshot.
const BACKLOG: usize = 64;
const PROVIDER_UNAVAILABLE: &str = "The FairPlay provider is unavailable. Check the launcher path in Slipmat’s fairplay-provider setting.";

/// Position ticks while playing. The same cadence the GTK client uses.
const TICK_MS: u64 = 500;

pub struct Daemon {
    pub model: RefCell<Model>,
    queue: tokio::sync::mpsc::UnboundedSender<(u64, Command)>,
    auth: Option<Rc<crate::auth::Service>>,
    pub(super) queue_epoch: std::cell::Cell<u64>,
    sign_out_epoch: std::cell::Cell<u64>,
    catalogue_items: RefCell<std::collections::HashMap<String, Item>>,
    sign_in_required: std::cell::Cell<bool>,
    pub events: broadcast::Sender<Event>,
    /// The bus. Filled in after the daemon exists, because it needs one.
    pub mpris: RefCell<Option<slipmat_core::mpris::Mpris>>,
    /// Whether a library fetch is in flight, so a reload button cannot stack
    /// four of them. The generation lets a new account refresh without waiting
    /// for the old account's request to finish.
    pub refreshing: std::cell::Cell<Option<u64>>,
    /// Changes whenever authorization crosses an account boundary.
    pub authorization_generation: std::cell::Cell<u64>,
    /// The authorization generation whose automatic freshness check started.
    pub automatic_refresh: std::cell::Cell<Option<u64>>,
    /// The artwork template already fetched, so a 500ms tick does not ask again
    /// for a file that is on disk.
    pub art_for: RefCell<Option<String>>,
    pub clients: std::cell::Cell<usize>,
    /// The desktop's own volume for this application, or `None` where there is
    /// no audio server to talk to.
    pub mixer: Option<crate::mixer::Mixer>,
    /// Rung by [`Request::Quit`], and answered where SIGTERM is — so leaving on
    /// purpose and being stopped by a service manager take the same path.
    pub quitting: tokio::sync::Notify,
}

impl Daemon {
    pub fn send(&self, cmd: Command) {
        if self.queue.send((self.queue_epoch.get(), cmd)).is_err() {
            self.publish(Event::Error {
                detail: "The queue worker stopped.".into(),
            });
        }
    }

    pub(super) fn sign_in_required(&self) -> bool {
        self.sign_in_required.get()
    }

    pub(super) fn require_sign_in(&self, required: bool) {
        if self.sign_in_required.replace(required) != required {
            self.publish(Event::SignInRequired { required });
        }
    }

    pub fn queue_items(&self, songs: Vec<String>) -> Vec<Item> {
        let model = self.model.borrow();
        let library: std::collections::HashMap<_, _> = model
            .library
            .tracks
            .iter()
            .filter_map(|track| track.catalog_id.as_deref().map(|id| (id, track)))
            .collect();
        let catalogue = self.catalogue_items.borrow();
        songs
            .into_iter()
            .map(|id| {
                catalogue
                    .get(&id)
                    .cloned()
                    .or_else(|| library.get(id.as_str()).map(|track| queue_item(track)))
                    .unwrap_or_else(|| Item {
                        title: id.clone(),
                        catalog_id: Some(id),
                        ..Item::default()
                    })
            })
            .collect()
    }

    pub fn publish(&self, event: Event) {
        if let Event::Results { entries, .. } | Event::Page { entries, .. } = &event {
            let mut catalogue = self.catalogue_items.borrow_mut();
            for entry in entries {
                if let Entry::Song(track) = entry
                    && let Some(id) = &track.catalog_id
                {
                    catalogue.insert(id.clone(), queue_item(track));
                }
            }
        }
        // An error here means nobody is listening, which is the ordinary state
        // of a daemon with no client attached.
        let _ = self.events.send(event);
    }

    fn publish_stage(&self, stage: Stage) {
        self.model.borrow_mut().stage = stage.clone();
        self.publish(Event::Stage(stage));
    }

    fn client(&self) -> Option<Client> {
        let model = self.model.borrow();
        let tokens = model.tokens.as_ref()?;
        Client::new(
            tokens.developer_token.clone(),
            tokens.music_user_token.clone()?,
            tokens.storefront.clone(),
        )
        .ok()
    }

    pub(super) fn publish_snapshot(&self) {
        self.publish(Event::Snapshot(self.model.borrow().snapshot()));
        if let Some(mpris) = self.mpris.borrow().as_ref() {
            mpris.update(bus::state(self));
        }
    }
}

fn queue_item(track: &slipmat_core::music::types::Track) -> Item {
    Item {
        id: track
            .library_id
            .clone()
            .or_else(|| Some(track.id.0.clone())),
        catalog_id: track.catalog_id.clone(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        duration_ms: track.duration_ms,
        track_number: track.track_number,
        artwork_template: track.artwork.as_ref().map(|art| art.template().to_owned()),
        ..Item::default()
    }
}

pub async fn run(provider: Option<std::path::PathBuf>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = ipc::socket_path()?;
    let listener = crate::auth::bind_listener(&path).await?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket = %path.display(), "listening");

    let auth = match provider {
        Some(provider) => match crate::auth::Endpoint::bind(provider.clone()).await {
            Ok(endpoint) => Some(endpoint),
            Err(err) => {
                tracing::warn!(launcher = %provider.display(), %err, "sign-in unavailable");
                None
            }
        },
        None => None,
    };
    let (events, _) = broadcast::channel(BACKLOG);
    let (queue_sender, queue_receiver) = tokio::sync::mpsc::unbounded_channel();
    let daemon = Rc::new(Daemon {
        model: RefCell::new(Model::new()),
        queue: queue_sender,
        auth: auth.as_ref().map(crate::auth::Endpoint::service),
        queue_epoch: Default::default(),
        sign_out_epoch: Default::default(),
        catalogue_items: RefCell::new(Default::default()),
        sign_in_required: std::cell::Cell::new(false),
        events,
        mpris: RefCell::new(None),
        refreshing: std::cell::Cell::new(None),
        authorization_generation: std::cell::Cell::new(0),
        automatic_refresh: std::cell::Cell::new(None),
        art_for: RefCell::new(None),
        clients: std::cell::Cell::new(0),
        quitting: tokio::sync::Notify::new(),
        mixer: crate::mixer::Mixer::start(),
    });

    *daemon.mpris.borrow_mut() = Some(bus::start(&daemon));
    tokio::task::spawn_local(crate::queue::run(
        daemon.clone(),
        daemon.auth.clone(),
        queue_receiver,
    ));

    // Accept loop: one task per client, each holding a handle to the daemon.
    let accepting = daemon.clone();
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(32));
    tokio::task::spawn_local(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    if verify_peer(&stream).is_err() {
                        continue;
                    }
                    let Ok(permit) = permits.clone().try_acquire_owned() else {
                        continue;
                    };
                    let daemon = accepting.clone();
                    tokio::task::spawn_local(async move {
                        let _permit = permit;
                        daemon.clients.set(daemon.clients.get() + 1);
                        if let Err(err) = client(stream, daemon.clone()).await {
                            tracing::debug!(?err, "client gone");
                        }
                        daemon.clients.set(daemon.clients.get().saturating_sub(1));
                        tracing::debug!(clients = daemon.clients.get(), "client left");
                    });
                }
                Err(err) => {
                    tracing::error!(?err, "accept failed");
                    return;
                }
            }
        }
    });

    // Position ticks. Only while playing — a paused player's position is a
    // fact, not something to extrapolate from.
    let ticking = daemon.clone();
    tokio::task::spawn_local(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
        loop {
            tick.tick().await;
            sync_catalogue(&ticking);
            // **Whoever moved it, everyone hears.** The desktop's audio panel
            // can change this without Slipmat being asked, and a client still
            // showing the old number is the disagreement this whole thing
            // moved to fix.
            let outside = ticking.mixer.as_ref().and_then(|m| m.current());
            let moved = outside.is_some_and(|v| (v - ticking.model.borrow().volume).abs() > 0.005);
            if moved {
                ticking.model.borrow_mut().volume = outside.unwrap_or_default();
            }
            if moved {
                ticking.publish_snapshot();
            }
        }
    });

    // **Leaving is a thing to do properly.** A service manager stops this with
    // SIGTERM, and that is the last moment the position is accurate — after it,
    // the process is gone and the session file still says where the last track
    // change left off.
    let leaving = daemon.clone();
    let socket = path.clone();
    tokio::task::spawn_local(async move {
        let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            return;
        };
        let mut interrupt =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
                Ok(sig) => sig,
                Err(_) => return,
            };
        let why = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
            _ = leaving.quitting.notified() => "a client quit",
        };
        tracing::info!(why, "shutting down");
        if let Some(service) = &leaving.auth {
            service.wait_for_profile_save().await;
        }
        if leaving.model.borrow().stage != Stage::Connecting {
            save_session(&leaving);
        }
        // The socket outlives the process that made it, and a leftover one
        // answers for a daemon that is not running.
        let _ = std::fs::remove_file(&socket);
        drop(auth);
        std::process::exit(0);
    });

    std::future::pending().await
}

pub(super) fn save_session(daemon: &Daemon) {
    let model = daemon.model.borrow();
    let songs: Vec<String> = model
        .player
        .queue
        .iter()
        .filter_map(|item| item.catalog_id.clone().or_else(|| item.id.clone()))
        .collect();

    if songs.is_empty() {
        slipmat_core::session::clear();
        return;
    }

    slipmat_core::session::save(&slipmat_core::session::Session {
        start: model
            .player
            .queue_position
            .min(songs.len().saturating_sub(1)),
        position_ms: model.player.position_ms,
        items: saved_details(&model.player.queue, songs.len()),
        songs,
    });
}

/// The queue's details, or none when any are the id-titled stand-ins
/// `queue_items` makes for a song it knows nothing about: saving those would
/// make every later restore show ids instead of looking the songs up again.
fn saved_details(queue: &[Item], songs: usize) -> Vec<Item> {
    let items: Vec<Item> = queue
        .iter()
        .filter(|item| item.catalog_id.is_some() || item.id.is_some())
        .cloned()
        .collect();
    let placeholder = |item: &Item| item.catalog_id.as_deref() == Some(item.title.as_str());
    if items.len() != songs || items.iter().any(placeholder) {
        return Vec::new();
    }
    items
}

fn clear_playback(daemon: &Daemon) {
    daemon
        .queue_epoch
        .set(daemon.queue_epoch.get().wrapping_add(1));
    daemon.send(Command::ClearQueue);
    let mut model = daemon.model.borrow_mut();
    model.player = slipmat_core::player::PlayerState::new();
    model.art_path = None;
    drop(model);
    daemon.art_for.borrow_mut().take();
    daemon.require_sign_in(false);
    slipmat_core::session::clear();
    daemon.publish(Event::Queue {
        items: Vec::new(),
        position: 0,
    });
    daemon.publish_snapshot();
}

fn sync_catalogue(daemon: &Rc<Daemon>) {
    let next = daemon.auth.as_ref().and_then(|service| service.catalogue());
    let stage = if next.is_some() {
        Stage::Ready
    } else if daemon.model.borrow().stage == Stage::Connecting
        && daemon
            .auth
            .as_ref()
            .is_some_and(|service| service.is_authenticating())
    {
        Stage::Connecting
    } else if daemon.auth.is_some() {
        Stage::SignedOut
    } else {
        Stage::Broken {
            detail: PROVIDER_UNAVAILABLE.into(),
        }
    };

    let mut model = daemon.model.borrow_mut();
    let signed_out = daemon.auth.as_ref().is_some_and(|service| {
        daemon.sign_out_epoch.replace(service.sign_out_epoch()) != service.sign_out_epoch()
    });
    let changed = signed_out || model.tokens != next;
    let ended = signed_out || (model.tokens.is_some() && changed);
    let startup_failed =
        model.stage == Stage::Connecting && next.is_none() && stage != Stage::Connecting;
    let restored = next.is_some()
        && daemon
            .auth
            .as_ref()
            .is_some_and(|service| service.take_restored());
    if changed {
        model.tokens = next;
        model.library = Library::default();
    }
    drop(model);
    if ended {
        clear_playback(daemon);
    } else if restored {
        restore_session(daemon);
    } else if startup_failed || (changed && daemon.model.borrow().player.queue.is_empty()) {
        slipmat_core::session::clear();
    }
    if startup_failed {
        slipmat_core::library_cache::clear();
    }
    if changed {
        daemon.catalogue_items.borrow_mut().clear();
        slipmat_core::library_cache::clear();
        daemon
            .authorization_generation
            .set(daemon.authorization_generation.get().saturating_add(1));
        if daemon.refreshing.take().is_some() {
            daemon.publish(Event::LibraryRefreshing { refreshing: false });
        }
        daemon.publish(Event::LibraryChanged);
    }
    if daemon.model.borrow().stage != stage {
        daemon.publish_stage(stage);
    }
    if changed {
        maybe_refresh_library(daemon);
    }
}

async fn client(stream: UnixStream, daemon: Rc<Daemon>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut frames = Reader::with_limit(read, 256 * 1024);
    let mut feed: Option<broadcast::Receiver<Event>> = None;
    let mut sent_capabilities = false;
    let mut first = true;
    let deadline = tokio::time::Instant::now() + slipmat_core::auth_wire::FIRST_REQUEST_TIMEOUT;

    loop {
        // Either the client says something, or — once subscribed — the daemon
        // does. `biased` so a request is never starved by a busy event stream.
        let outgoing = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline), if first => return Ok(()),
            frame = frames.frame() => match frame? {
                Some(request) => {
                    first = false;
                    sync_catalogue(&daemon);
                    answer(request, &daemon, &mut feed)
                },
                None => return Ok(()), // client hung up
            },
            event = recv(&mut feed) => Some(event?),
        };

        if let Some(event) = outgoing {
            write_event(&mut write, &event).await?;
        }
        if feed.is_some() && !sent_capabilities {
            write_event(
                &mut write,
                &Event::PlaybackCapabilities {
                    seek: false,
                    shuffle: false,
                    repeat: false,
                },
            )
            .await?;
            write_event(
                &mut write,
                &Event::SignInRequired {
                    required: daemon.sign_in_required(),
                },
            )
            .await?;
            sent_capabilities = true;
        }
    }
}

async fn write_event(write: &mut OwnedWriteHalf, event: &Event) -> Result<()> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    tokio::time::timeout(std::time::Duration::from_secs(5), write.write_all(&line)).await??;
    Ok(())
}

/// Wait on the event feed, or forever if this client has not subscribed.
///
/// `select!` needs both arms to be futures; a client that never subscribes
/// simply has one that never completes.
async fn recv(feed: &mut Option<broadcast::Receiver<Event>>) -> Result<Event> {
    match feed {
        Some(rx) => Ok(rx.recv().await?),
        None => std::future::pending().await,
    }
}

fn answer(
    request: Request,
    daemon: &Rc<Daemon>,
    feed: &mut Option<broadcast::Receiver<Event>>,
) -> Option<Event> {
    match request {
        Request::Subscribe => {
            if feed.is_none() {
                *feed = Some(daemon.events.subscribe());
            }
            // The first thing a subscriber gets is where things stand, so it
            // never has to draw an empty bar waiting for a change.
            Some(Event::Snapshot(daemon.model.borrow().snapshot()))
        }
        Request::Snapshot => Some(Event::Snapshot(daemon.model.borrow().snapshot())),
        Request::Stage => Some(Event::Stage(daemon.model.borrow().stage.clone())),
        Request::Queue => {
            let (items, position) = daemon.model.borrow().queue();
            Some(Event::Queue { items, position })
        }
        Request::JumpTo { index } => {
            daemon.send(Command::ChangeToIndex { index });
            None
        }
        Request::Browse {
            view,
            query,
            offset,
            limit,
            sort,
            reverse,
        } => {
            let model = daemon.model.borrow();
            let (entries, total) = model
                .library
                .browse(view, &query, offset, limit, sort, reverse);
            Some(Event::Rows {
                view,
                entries,
                total,
            })
        }
        Request::SignIn => Some(if daemon.auth.is_some() {
            Event::SignInRequired { required: true }
        } else {
            Event::Error {
                detail: PROVIDER_UNAVAILABLE.into(),
            }
        }),
        Request::SignOut => {
            if let Some(service) = daemon.auth.clone() {
                let daemon = daemon.clone();
                tokio::task::spawn_local(async move {
                    if service.sign_out().await == slipmat_core::auth::Reply::SignedOut {
                        sync_catalogue(&daemon);
                    } else {
                        daemon.publish(Event::Error {
                            detail: "Native sign-out failed.".into(),
                        });
                    }
                });
            }
            None
        }
        Request::Search {
            query,
            filter,
            offset,
        } => {
            search(daemon, query, filter, offset);
            None
        }
        Request::Open { kind, id } => {
            // Answered off this task: opening a page is a network round trip,
            // and a client waiting on one must not stop the daemon answering
            // everyone else.
            open_page(daemon, kind, id);
            None
        }
        Request::Quit => {
            // Counted around the connection, so this client is in the total.
            // Anyone else attached is a window that would lose its player.
            if daemon.clients.get() > 1 {
                return Some(Event::Error {
                    detail: "Another Slipmat client is still open".into(),
                });
            }
            tracing::info!("a client asked to quit");
            // **Through the same door a service manager uses.** Saving the
            // session and clearing the socket are already done properly on
            // SIGTERM; a second copy of that here is a second one to keep
            // right. Raising it on ourselves also lets this return first, so
            // the client is not writing into a socket that is already gone.
            daemon.quitting.notify_one();
            None
        }
        Request::Play { ids, index, .. } => {
            play(daemon, &ids, index);
            None
        }
        Request::Enqueue { ids, next } => {
            // Filtered, because a dead id rejects the whole insert the same way
            // it rejects a whole queue.
            let dead = daemon.model.borrow().dead_ids.clone();
            let songs: Vec<String> = ids.into_iter().filter(|id| !dead.contains(id)).collect();
            if songs.is_empty() {
                return Some(Event::Error {
                    detail: "Nothing here can be streamed".into(),
                });
            }
            tracing::info!(count = songs.len(), next, "enqueueing");
            daemon.send(if next {
                Command::PlayNext { songs }
            } else {
                Command::PlayLater { songs }
            });
            None
        }
        Request::RemoveFromQueue { index } => {
            daemon.send(Command::RemoveFromQueue { index });
            None
        }
        Request::MoveInQueue { from, to } => {
            daemon.send(Command::MoveInQueue { from, to });
            None
        }
        Request::ClearQueue => {
            daemon.send(Command::ClearQueue);
            None
        }
        Request::Write { action, id } => {
            write(daemon, action, id);
            None
        }
        Request::Refresh => {
            refresh_library(daemon);
            None
        }
        Request::Transport(transport) => {
            route_transport(daemon, transport);
            None
        }
    }
}

pub(crate) fn route_transport(daemon: &Rc<Daemon>, transport: Transport) {
    if let Some(command) = command_for(transport) {
        tracing::debug!(?command, "transport");
        daemon.send(command);
    }
    if let Transport::SetVolume { volume } = transport {
        let volume = volume.clamp(0.0, 1.0);
        if let Some(mixer) = &daemon.mixer {
            mixer.set(volume);
        }
        daemon.model.borrow_mut().volume = volume;
        daemon.publish_snapshot();
    }
}

/// Fetch the current track's cover, if we do not already have it.
///
/// **At most once per template.** A snapshot goes out twice a second and the
/// cover changes at most once a track; refetching on every one would be a
/// request per tick for a file already on disk.
pub(super) fn fetch_artwork(daemon: &Rc<Daemon>) {
    let template = daemon
        .model
        .borrow()
        .current_item()
        .and_then(|item| item.artwork_template.clone());

    if template == *daemon.art_for.borrow() {
        return;
    }
    *daemon.art_for.borrow_mut() = template.clone();

    let Some(template) = template else {
        daemon.model.borrow_mut().art_path = None;
        return;
    };

    let art = Artwork::new(template);
    // Already on disk from an earlier play, or from the GTK client — the cache
    // is shared, which is the point of it living in core.
    if let Some(path) = artwork::cache_path(&art, artwork::ART_SIZE)
        && path.is_file()
    {
        daemon.model.borrow_mut().art_path = Some(path);
        return;
    }

    // Cleared first: a stale cover under a new track is worse than none, and
    // this is a network round trip.
    daemon.model.borrow_mut().art_path = None;
    let art_template = art.template().to_owned();
    let epoch = daemon.queue_epoch.get();
    let daemon = daemon.clone();
    tokio::task::spawn_local(async move {
        match artwork::fetch(art, artwork::ART_SIZE).await {
            Ok(path)
                if epoch == daemon.queue_epoch.get()
                    && *daemon.art_for.borrow() == Some(art_template) =>
            {
                daemon.model.borrow_mut().art_path = Some(path);
                daemon.publish_snapshot();
            }
            Ok(_) => {}
            // Cosmetic: a missing cover is not worth telling anyone about.
            Err(err) => tracing::warn!(?err, "artwork not fetched"),
        }
    });
}

/// Change what Apple holds for this account.
///
/// Adding and favouriting go over REST. Removing and un-favouriting have no
/// route: the matching REST calls answer `400 Insufficient Permissions`.
fn write(daemon: &Rc<Daemon>, action: WriteAction, id: String) {
    if matches!(
        action,
        WriteAction::RemoveFromLibrary | WriteAction::Unfavorite
    ) {
        daemon.publish(Event::Error {
            detail: "Slipmat can add and favourite songs, but not remove them.".into(),
        });
        return;
    }

    let Some(client) = daemon.client() else {
        daemon.publish(Event::Error {
            detail: "Not signed in yet".into(),
        });
        return;
    };
    let generation = daemon.authorization_generation.get();
    let daemon = daemon.clone();
    tokio::task::spawn_local(async move {
        let result = match action {
            WriteAction::Favorite => client.add_to_favorites("songs", &id).await,
            WriteAction::AddToLibrary => client.add_to_library("songs", &id).await,
            _ => unreachable!("handled above"),
        };
        if generation != daemon.authorization_generation.get() {
            return;
        }
        match result {
            // Apple answers 202 Accepted with an empty body — "acceptable, may
            // not have completed" — so this is *sent*, not done. The refresh is
            // what makes it true, which is why nothing here edits the mirror.
            Ok(()) => {
                tracing::info!(?action, %id, "library write sent");
                refresh_library(&daemon);
            }
            Err(err) => {
                tracing::warn!(?err, ?action, %id, "library write failed");
                daemon.publish(Event::Error {
                    detail: format!("{err}"),
                });
            }
        }
    });
}

/// Re-read the library from Apple and tell clients it moved.
///
/// Spawned, and at most one at a time: a client hammering a reload button must
/// not queue up four full library fetches behind it.
fn maybe_refresh_library(daemon: &Rc<Daemon>) {
    let generation = daemon.authorization_generation.get();
    if daemon.automatic_refresh.get() == Some(generation) {
        return;
    }
    if refresh_library(daemon) {
        daemon.automatic_refresh.set(Some(generation));
    }
}

fn refresh_library(daemon: &Rc<Daemon>) -> bool {
    let Some(client) = daemon.client() else {
        return false;
    };
    let Some(generation) = begin_library_refresh(daemon) else {
        tracing::debug!("library refresh already running");
        return true;
    };

    let daemon = daemon.clone();
    tokio::task::spawn_local(async move {
        const MAX: usize = 10_000;
        let fetched = tokio::join!(
            client.all_library_songs(MAX),
            client.all_library_albums(MAX),
            client.all_library_artists(MAX),
            client.all_library_playlists(MAX),
        );
        if daemon.authorization_generation.get() != generation {
            tracing::debug!(generation, "discarded stale library refresh");
            return;
        }

        match fetched {
            (Ok(songs), Ok(albums), Ok(artists), Ok(playlists)) => {
                let next = Library {
                    tracks: songs,
                    albums,
                    artists,
                    playlists,
                };
                if commit_library(&daemon, generation, next, |library| {
                    slipmat_core::library_cache::save(
                        &library.tracks,
                        &library.albums,
                        &library.artists,
                        &library.playlists,
                    )
                }) {
                    let library = &daemon.model.borrow().library;
                    tracing::info!(
                        songs = library.tracks.len(),
                        albums = library.albums.len(),
                        artists = library.artists.len(),
                        playlists = library.playlists.len(),
                        "library refreshed"
                    );
                }
            }
            _ => tracing::warn!("library refresh failed; keeping what was cached"),
        }
        finish_library_refresh(&daemon, generation);
    });
    true
}

fn commit_library(
    daemon: &Daemon,
    generation: u64,
    next: Library,
    persist: impl FnOnce(&Library) -> bool,
) -> bool {
    if daemon.authorization_generation.get() != generation {
        return false;
    }
    if daemon.model.borrow().library == next {
        tracing::info!("library unchanged");
        return false;
    }
    if !persist(&next) {
        tracing::warn!("library refresh could not be persisted; keeping the cache");
        return false;
    }
    daemon.model.borrow_mut().library = next;
    daemon.publish(Event::LibraryChanged);
    true
}

fn begin_library_refresh(daemon: &Daemon) -> Option<u64> {
    let generation = daemon.authorization_generation.get();
    if daemon.refreshing.get() == Some(generation) {
        return None;
    }
    daemon.refreshing.set(Some(generation));
    daemon.publish(Event::LibraryRefreshing { refreshing: true });
    Some(generation)
}

fn finish_library_refresh(daemon: &Daemon, generation: u64) -> bool {
    let current = daemon.authorization_generation.get() == generation;
    if daemon.refreshing.get() == Some(generation) {
        daemon.refreshing.set(None);
        if current {
            daemon.publish(Event::LibraryRefreshing { refreshing: false });
        }
    }
    current
}

/// Tracks as rows.
fn songs(tracks: Vec<slipmat_core::music::types::Track>) -> Vec<Entry> {
    tracks.into_iter().map(Entry::Song).collect()
}

/// Search the catalog and announce what came back.
///
/// Spawned like `open_page`, and for the same reason: this is a network round
/// trip, and a client waiting on one must not stop the daemon answering anyone
/// else. The answer carries the query it belongs to, because somebody types
/// faster than Apple replies.
fn search(daemon: &Rc<Daemon>, query: String, filter: CatalogFilter, offset: usize) {
    let Some(client) = daemon.client() else {
        daemon.publish(Event::Error {
            detail: "Not signed in yet".into(),
        });
        return;
    };

    let generation = daemon.authorization_generation.get();
    let daemon = daemon.clone();
    tokio::task::spawn_local(async move {
        let found = client
            .search(&query, filter.types(), catalog::CATALOG_LIMIT, offset)
            .await;
        if generation != daemon.authorization_generation.get() {
            return;
        }
        match found {
            Ok(results) => {
                let (entries, paged) = catalog::catalog_rows(filter, results, offset == 0);
                tracing::info!(?filter, offset, rows = entries.len(), "searched");
                daemon.publish(Event::Results {
                    query,
                    entries,
                    offset,
                    // A short page is the last page: Apple gave us fewer than
                    // it caps at, so there is nothing behind it.
                    more: paged >= catalog::CATALOG_LIMIT as usize,
                });
            }
            Err(err) => {
                tracing::warn!(?filter, offset, "search failed");
                daemon.publish(Event::Error {
                    detail: format!("{err}"),
                });
            }
        }
    });
}

/// Fetch an album, artist or playlist and announce it.
///
/// Spawned rather than awaited: this is a network round trip, and a client
/// waiting on one must not stop the daemon answering anybody else. The answer
/// arrives as an [`Event::Page`] on every subscriber, which is also what lets a
/// second client show a page the first one opened.
fn open_page(daemon: &Rc<Daemon>, kind: PageKind, id: String) {
    let Some(client) = daemon.client() else {
        daemon.publish(Event::Error {
            detail: "Not signed in yet".into(),
        });
        return;
    };

    let generation = daemon.authorization_generation.get();
    let daemon = daemon.clone();
    tokio::task::spawn_local(async move {
        // **An artist page is their albums, not their tracks**, which is why
        // this is not one call with a different id. Catalog and library are
        // separate for the other reason: the two id spaces are not
        // interchangeable, and a catalog id 404s against `/me/library`.
        let fetched = match kind {
            PageKind::Album => client
                .album(&id)
                .await
                .map(|(album, tracks)| (Entry::Album(album), songs(tracks))),
            PageKind::LibraryAlbum => client
                .library_album(&id)
                .await
                .map(|(album, tracks)| (Entry::Album(album), songs(tracks))),
            PageKind::Playlist => client
                .playlist(&id)
                .await
                .map(|(list, tracks)| (Entry::Playlist(list), songs(tracks))),
            PageKind::LibraryPlaylist => client
                .library_playlist(&id)
                .await
                .map(|(list, tracks)| (Entry::Playlist(list), songs(tracks))),
            PageKind::Artist => client.artist_albums(&id).await.map(|(artist, albums)| {
                (
                    Entry::Artist(artist),
                    albums.into_iter().map(Entry::Album).collect(),
                )
            }),
            PageKind::LibraryArtist => {
                client
                    .library_artist_albums(&id)
                    .await
                    .map(|(artist, albums)| {
                        (
                            Entry::Artist(artist),
                            albums.into_iter().map(Entry::Album).collect(),
                        )
                    })
            }
        };

        if generation != daemon.authorization_generation.get() {
            return;
        }
        match fetched {
            Ok((header, entries)) => daemon.publish(Event::Page {
                kind,
                id,
                header,
                entries,
            }),
            Err(err) => {
                tracing::warn!(?err, %id, "opening a page failed");
                daemon.publish(Event::Error {
                    detail: format!("{err}"),
                });
            }
        }
    });
}

/// Build a queue from ids a client drew, and start it. Dead ids drop out, and
/// `index` is re-counted past them so it still names the clicked song.
fn play(daemon: &Daemon, ids: &[String], index: usize) {
    let model = daemon.model.borrow();
    let playable = |id: &&String| !model.dead_ids.contains(*id);
    let songs: Vec<_> = ids.iter().filter(playable).cloned().collect();
    let start_position = ids.iter().take(index).filter(playable).count();
    drop(model);
    if songs.is_empty() {
        daemon.publish(Event::Error {
            detail: "Nothing here can be streamed".into(),
        });
        return;
    }
    daemon.send(Command::SetQueue {
        start_position: start_position.min(songs.len() - 1),
        items: daemon.queue_items(songs),
        start_playing: true,
    });
}

/// The saved details go with the ids: a restore runs before the library has
/// loaded, so looking the songs up would find nothing and title them by id.
fn restore_session(daemon: &Daemon) {
    let Some(session) = slipmat_core::session::load() else {
        return;
    };
    let start = session.start.min(session.songs.len().saturating_sub(1));
    let items = if session.items.len() == session.songs.len() {
        session.items
    } else {
        // Written before the session kept details.
        daemon.queue_items(session.songs)
    };
    daemon.send(Command::SetQueue {
        items,
        start_position: start,
        start_playing: false,
    });
}

fn command_for(transport: Transport) -> Option<Command> {
    Some(match transport {
        Transport::Play => Command::Play,
        Transport::Pause => Command::Pause,
        Transport::PlayPause => Command::PlayPause,
        Transport::Next => Command::Next,
        Transport::Previous => Command::Previous,
        Transport::Seek { .. }
        | Transport::SetVolume { .. }
        | Transport::SetShuffle { .. }
        | Transport::SetRepeat { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use slipmat_core::music::types::{Track, TrackId};
    use slipmat_core::player::protocol::{Item, Tokens};

    fn daemon() -> Rc<Daemon> {
        let (events, _) = broadcast::channel(8);
        Rc::new(Daemon {
            model: RefCell::new(Model::new()),
            queue: tokio::sync::mpsc::unbounded_channel().0,
            auth: None,
            queue_epoch: Default::default(),
            sign_out_epoch: Default::default(),
            catalogue_items: RefCell::new(Default::default()),
            sign_in_required: std::cell::Cell::new(false),
            events,
            mpris: RefCell::new(None),
            refreshing: std::cell::Cell::new(None),
            authorization_generation: std::cell::Cell::new(0),
            automatic_refresh: std::cell::Cell::new(None),
            art_for: RefCell::new(None),
            clients: std::cell::Cell::new(0),
            mixer: None,
            quitting: tokio::sync::Notify::new(),
        })
    }

    fn usable_tokens() -> Tokens {
        Tokens {
            developer_token: "developer".into(),
            music_user_token: Some("user".into()),
            storefront: "us".into(),
            authorized: true,
        }
    }

    fn refresh_events(events: &mut broadcast::Receiver<Event>) -> Vec<bool> {
        let mut refreshing = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let Event::LibraryRefreshing { refreshing: state } = event {
                refreshing.push(state);
            }
        }
        refreshing
    }

    fn library_with_track(title: &str) -> crate::library::Library {
        crate::library::Library {
            tracks: vec![Track {
                date_added: String::new(),
                year: String::new(),
                favorite: false,
                in_library: true,
                library_id: None,
                id: TrackId(title.into()),
                catalog_id: Some(title.into()),
                title: title.into(),
                artist: String::new(),
                album: String::new(),
                duration_ms: 0,
                track_number: 0,
                artwork: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn an_automatic_trigger_coalesces_with_an_in_flight_manual_refresh() {
        let daemon = daemon();
        daemon.model.borrow_mut().stage = Stage::Ready;
        daemon.model.borrow_mut().tokens = Some(usable_tokens());
        let mut events = daemon.events.subscribe();
        let local = tokio::task::LocalSet::new();
        let _entered = local.enter();

        assert!(refresh_library(&daemon));
        maybe_refresh_library(&daemon);
        assert_eq!(daemon.automatic_refresh.get(), Some(0));

        finish_library_refresh(&daemon, 0);
        maybe_refresh_library(&daemon);

        assert_eq!(daemon.refreshing.get(), None);
        assert_eq!(refresh_events(&mut events), [true, false]);
    }

    #[test]
    fn an_unchanged_library_is_not_persisted_or_published() {
        let daemon = daemon();
        daemon.model.borrow_mut().library = crate::library::Library::default();
        let persisted = std::cell::Cell::new(0);
        let mut events = daemon.events.subscribe();

        assert!(!commit_library(
            &daemon,
            0,
            crate::library::Library::default(),
            |_| {
                persisted.set(persisted.get() + 1);
                true
            },
        ));

        assert_eq!(persisted.get(), 0);
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn a_changed_library_is_persisted_then_published_once() {
        let daemon = daemon();
        daemon.model.borrow_mut().library = crate::library::Library::default();
        let persisted = std::cell::Cell::new(0);
        let mut events = daemon.events.subscribe();

        assert!(commit_library(
            &daemon,
            0,
            library_with_track("new"),
            |_| {
                persisted.set(persisted.get() + 1);
                true
            },
        ));

        assert_eq!(persisted.get(), 1);
        assert_eq!(daemon.model.borrow().library.tracks[0].title, "new");
        assert!(matches!(events.try_recv(), Ok(Event::LibraryChanged)));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn a_persistence_failure_keeps_the_known_good_library() {
        let daemon = daemon();
        daemon.model.borrow_mut().library = library_with_track("old");
        let mut events = daemon.events.subscribe();

        assert!(!commit_library(
            &daemon,
            0,
            library_with_track("new"),
            |_| false,
        ));

        assert_eq!(daemon.model.borrow().library.tracks[0].title, "old");
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn a_stale_library_result_changes_nothing() {
        let daemon = daemon();
        daemon.model.borrow_mut().library = library_with_track("current");
        daemon.authorization_generation.set(1);
        daemon.refreshing.set(Some(1));
        let mut events = daemon.events.subscribe();

        assert!(!commit_library(
            &daemon,
            0,
            library_with_track("stale"),
            |_| panic!("a stale result must not reach persistence"),
        ));

        assert_eq!(daemon.model.borrow().library.tracks[0].title, "current");
        assert_eq!(daemon.refreshing.get(), Some(1));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn stale_refresh_cannot_finish_or_release_the_current_guard() {
        let daemon = daemon();
        assert_eq!(daemon.authorization_generation.get(), 0);

        let signed_out = daemon.authorization_generation.get();
        let current = signed_out + 1;
        daemon.authorization_generation.set(current);

        let mut events = daemon.events.subscribe();
        daemon.refreshing.set(Some(signed_out));
        assert_eq!(begin_library_refresh(&daemon), Some(current));
        assert!(matches!(
            events.try_recv(),
            Ok(Event::LibraryRefreshing { refreshing: true })
        ));
        assert_eq!(begin_library_refresh(&daemon), None);
        assert!(!finish_library_refresh(&daemon, signed_out));
        assert!(events.try_recv().is_err());
        assert_eq!(daemon.refreshing.get(), Some(current));
        assert!(finish_library_refresh(&daemon, current));
        assert!(matches!(
            events.try_recv(),
            Ok(Event::LibraryRefreshing { refreshing: false })
        ));
        assert_eq!(daemon.refreshing.get(), None);
    }

    #[test]
    fn a_selected_queue_item_can_start_an_artwork_fetch() {
        let daemon = daemon();
        let template = "https://example.test/restored/{w}x{h}.{f}";
        daemon.model.borrow_mut().player.queue = vec![Item {
            artwork_template: Some(template.into()),
            ..Default::default()
        }];
        let local = tokio::task::LocalSet::new();
        let _entered = local.enter();

        fetch_artwork(&daemon);

        assert_eq!(daemon.art_for.borrow().as_deref(), Some(template));
        assert!(daemon.model.borrow().art_path.is_none());
    }

    #[test]
    fn a_queue_of_stand_ins_is_not_saved_as_details() {
        let daemon = daemon();
        // Nothing is known yet, as at a restore: the song is titled by its id.
        let stand_ins = daemon.queue_items(vec!["1440857781".into()]);
        assert_eq!(stand_ins[0].title, "1440857781");
        assert!(saved_details(&stand_ins, 1).is_empty());

        let known = vec![Item {
            catalog_id: Some("1440857781".into()),
            title: "Roundabout".into(),
            ..Item::default()
        }];
        assert_eq!(saved_details(&known, 1), known);
    }
}
