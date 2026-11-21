// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Owns the native queue and serializes selections with the audio worker.

use std::rc::Rc;
use std::time::Duration;

use slipmat_core::ipc::Event;
use slipmat_core::native_auth::{Reply, TrialStage};
use slipmat_core::player::protocol::{
    Command, Event as PlayerEvent, Item, PlaybackState, Queue, QueueChange,
};
use tokio::sync::mpsc::{self, UnboundedReceiver as Receiver, UnboundedSender as Sender};

use crate::native_auth::{PlaybackControl, Service};
use crate::serve::{Daemon, fetch_artwork, save_session};

struct Active {
    control: Sender<PlaybackControl>,
    events: Receiver<Reply>,
    stopping: bool,
}

#[derive(Default)]
struct QueuePlayer {
    items: Vec<Item>,
    index: usize,
    occurrence: u64,
    playing: bool,
    active: Option<Active>,
}

pub(super) async fn run(
    daemon: Rc<Daemon>,
    service: Option<Rc<Service>>,
    mut commands: Receiver<Command>,
) {
    let mut player = QueuePlayer::default();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            cmd = commands.recv() => {
                let Some(cmd) = cmd else { break };
                player.command(&daemon, cmd);
            }
            reply = receive(&mut player.active) => player.reply(&daemon, reply),
            _ = tick.tick(), if player.playing && player.active.is_none() => {}
        }
        player.start(&daemon, service.as_ref());
    }
}

async fn receive(active: &mut Option<Active>) -> Option<Reply> {
    match active {
        Some(active) => active.events.recv().await,
        None => std::future::pending().await,
    }
}

impl QueuePlayer {
    fn items(&mut self, daemon: &Daemon, songs: Vec<String>) -> Vec<Item> {
        daemon
            .native_items(songs)
            .into_iter()
            .map(|mut item| {
                self.occurrence += 1;
                item.occurrence_id = format!("native:{}", self.occurrence);
                item
            })
            .collect()
    }

    fn command(&mut self, daemon: &Rc<Daemon>, cmd: Command) {
        match cmd {
            Command::SetQueue {
                songs,
                start_position,
                start_playing,
                ..
            } => {
                self.items = self.items(daemon, songs);
                self.select(daemon, start_position, start_playing);
            }
            Command::ClearQueue => {
                self.items.clear();
                self.select(daemon, 0, false);
            }
            Command::PlayNext { songs } => self.enqueue(daemon, songs, true),
            Command::PlayLater { songs } => self.enqueue(daemon, songs, false),
            Command::RemoveFromQueue { index } => self.remove(daemon, index),
            Command::MoveInQueue { from, to } => self.move_item(daemon, from, to),
            Command::ChangeToIndex { index } if index < self.items.len() => {
                self.select(daemon, index, true);
            }
            Command::Next if self.index + 1 < self.items.len() => {
                self.select(daemon, self.index + 1, self.playing);
            }
            Command::Previous if self.index > 0 => {
                self.select(daemon, self.index - 1, self.playing);
            }
            Command::Play => self.set_playing(daemon, true),
            Command::Pause => self.set_playing(daemon, false),
            Command::PlayPause => self.set_playing(daemon, !self.playing),
            _ => {}
        }
    }

    fn set_playing(&mut self, daemon: &Daemon, playing: bool) {
        if self.playing == playing {
            return;
        }
        self.playing = playing && !self.items.is_empty();
        if !self.playing {
            daemon.require_native_sign_in(false);
        }
        if let Some(active) = self.active.as_ref().filter(|a| !a.stopping) {
            let command = if self.playing {
                PlaybackControl::Resume
            } else {
                PlaybackControl::Pause
            };
            let _ = active.control.send(command);
        }
    }

    fn enqueue(&mut self, daemon: &Rc<Daemon>, songs: Vec<String>, next: bool) {
        let new = self.items(daemon, songs);
        let index = if next && !self.items.is_empty() {
            self.index + 1
        } else {
            self.items.len()
        };
        self.items.splice(index..index, new);
        self.project(daemon);
    }

    fn remove(&mut self, daemon: &Rc<Daemon>, index: usize) {
        if index >= self.items.len() {
            return;
        }
        self.items.remove(index);
        if index == self.index {
            self.select(daemon, index, self.playing);
        } else {
            self.index -= usize::from(index < self.index);
            self.project(daemon);
        }
    }

    fn move_item(&mut self, daemon: &Rc<Daemon>, from: usize, to: usize) {
        if from >= self.items.len() || to >= self.items.len() || from == to {
            return;
        }
        let selected = self.items[self.index].occurrence_id.clone();
        let item = self.items.remove(from);
        self.items.insert(to, item);
        if let Some(index) = self
            .items
            .iter()
            .position(|item| item.occurrence_id == selected)
        {
            self.index = index;
        }
        self.project(daemon);
    }

    fn select(&mut self, daemon: &Rc<Daemon>, index: usize, play: bool) {
        self.index = index.min(self.items.len().saturating_sub(1));
        self.playing = play && !self.items.is_empty();
        if !self.playing {
            daemon.require_native_sign_in(false);
        }
        if let Some(active) = &mut self.active
            && !active.stopping
        {
            active.stopping = true;
            let _ = active.control.send(PlaybackControl::Stop);
        }
        Self::position(
            daemon,
            0,
            self.items
                .get(self.index)
                .map_or(0, |item| item.duration_ms),
        );
        let state = if self.playing {
            PlaybackState::Loading
        } else {
            PlaybackState::Paused
        };
        daemon
            .model
            .borrow_mut()
            .player
            .apply(&PlayerEvent::PlaybackState { state });
        self.project(daemon);
    }

    fn project(&self, daemon: &Rc<Daemon>) {
        let mut model = daemon.model.borrow_mut();
        let queue = Queue {
            position: self.index as i64,
            reason: QueueChange::Items,
            items: self.items.clone(),
        };
        let item = self.items.get(self.index);
        if model.player.now_playing.as_ref().map(|i| &i.occurrence_id)
            != item.map(|i| &i.occurrence_id)
        {
            model.player.apply(&PlayerEvent::NowPlaying {
                item: item.cloned(),
                queue,
            });
        } else {
            model.player.apply(&PlayerEvent::Queue(queue));
        }
        let (items, position) = model.queue();
        drop(model);
        daemon.publish(Event::Queue { items, position });
        save_session(daemon);
        fetch_artwork(daemon);
        daemon.publish_snapshot();
    }

    fn start(&mut self, daemon: &Rc<Daemon>, service: Option<&Rc<Service>>) {
        if !self.playing || self.active.is_some() {
            return;
        }
        let Some(track) = self
            .items
            .get(self.index)
            .and_then(|item| item.catalog_id.clone())
        else {
            self.fail(
                daemon,
                "This song has no playable Apple Music catalogue ID.",
            );
            return;
        };
        let Some(service) = service else {
            self.fail(daemon, "Native playback is unavailable. Prepare the FairPlay provider and restart Slipmat.");
            return;
        };
        if service.is_authenticating() {
            return;
        }
        if !service.is_signed_in() {
            if !daemon.native_sign_in_required() {
                Self::state(daemon, PlaybackState::Paused);
                daemon.require_native_sign_in(true);
            }
            return;
        }
        daemon.require_native_sign_in(false);
        let (control, controls) = mpsc::unbounded_channel();
        match service.start_queue(track, controls) {
            Ok(events) => {
                self.active = Some(Active {
                    control,
                    events,
                    stopping: false,
                });
                Self::state(daemon, PlaybackState::Loading);
            }
            Err(_) => self.fail(daemon, "The native playback worker is busy or unavailable."),
        }
    }

    fn reply(&mut self, daemon: &Rc<Daemon>, reply: Option<Reply>) {
        let stopping = self.active.as_ref().is_some_and(|active| active.stopping);
        match reply {
            Some(Reply::Playing) if !stopping && self.playing => {
                Self::state(daemon, PlaybackState::Playing)
            }
            Some(Reply::Paused) if !stopping && !self.playing => {
                Self::state(daemon, PlaybackState::Paused)
            }
            Some(Reply::Position { position_ms }) if !stopping => {
                let duration = self
                    .items
                    .get(self.index)
                    .map_or(0, |item| item.duration_ms);
                Self::position(daemon, position_ms, duration);
                daemon.publish_snapshot();
            }
            Some(Reply::Playing | Reply::Paused | Reply::Position { .. }) => {}
            terminal => {
                self.active = None;
                if stopping {
                    return;
                }
                match terminal {
                    Some(Reply::Played { .. })
                        if self.playing && self.index + 1 < self.items.len() =>
                    {
                        self.select(daemon, self.index + 1, true);
                    }
                    Some(Reply::Played { .. }) => {
                        self.playing = false;
                        Self::state(daemon, PlaybackState::Ended);
                    }
                    Some(Reply::Stopped | Reply::SignedOut) => {
                        self.playing = false;
                        Self::state(daemon, PlaybackState::Paused);
                    }
                    Some(Reply::LicenceDenied) => {
                        self.fail(daemon, "Apple Music denied the playback licence.")
                    }
                    Some(Reply::TrialFailed { stage }) => {
                        let detail = match stage {
                            TrialStage::Session => "The native session is unavailable.",
                            TrialStage::Storefront => "Apple Music storefront lookup failed.",
                            TrialStage::Catalogue => "Apple Music catalogue lookup failed.",
                            TrialStage::CatalogueAuthorization => {
                                "Apple Music rejected catalogue authorization."
                            }
                            TrialStage::CatalogueTrack => {
                                "Apple Music could not find this track in your storefront."
                            }
                            TrialStage::CatalogueAsset => {
                                "Apple Music returned no lossless asset for this track."
                            }
                            TrialStage::Stream => {
                                "The FairPlay stream could not be opened or completed."
                            }
                            TrialStage::Output => "The native audio output failed.",
                        };
                        self.fail(daemon, detail);
                    }
                    _ => self.fail(daemon, "The native playback worker stopped."),
                }
            }
        }
    }

    fn state(daemon: &Daemon, state: PlaybackState) {
        daemon
            .model
            .borrow_mut()
            .player
            .apply(&PlayerEvent::PlaybackState { state });
        daemon.publish_snapshot();
    }

    fn position(daemon: &Daemon, position_ms: u64, duration_ms: u64) {
        daemon
            .model
            .borrow_mut()
            .player
            .apply(&PlayerEvent::Position {
                position_ms,
                duration_ms,
            });
    }

    fn fail(&mut self, daemon: &Daemon, detail: &str) {
        self.playing = false;
        daemon.require_native_sign_in(false);
        Self::state(daemon, PlaybackState::Paused);
        daemon.publish(Event::Error {
            detail: detail.into(),
        });
    }
}
