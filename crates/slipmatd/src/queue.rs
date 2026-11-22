// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Owns the queue and serializes selections with the audio worker.

use std::rc::Rc;
use std::time::Duration;

use slipmat_core::auth::{DecodeStep, PlaybackFault, PlaybackStage, Reply};
use slipmat_core::ipc::Event;
use slipmat_core::player::protocol::{
    Command, Event as PlayerEvent, Item, PlaybackState, Queue, QueueChange,
};
use tokio::sync::mpsc::{self, UnboundedReceiver as Receiver, UnboundedSender as Sender};

use crate::auth::{PlaybackControl, Service};
use crate::serve::{Daemon, fetch_artwork, save_session};

struct Active {
    control: Sender<PlaybackControl>,
    events: Receiver<Reply>,
    stopping: bool,
}

/// A check holding the provider's playback slot.
struct Checking {
    id: String,
    /// Dropping it stops the check.
    _control: Sender<PlaybackControl>,
    events: Receiver<Reply>,
}

#[derive(Default)]
struct QueuePlayer {
    items: Vec<Item>,
    index: usize,
    occurrence: u64,
    playing: bool,
    active: Option<Active>,
    /// A check asked for, waiting for the track it stopped to let go.
    pending_check: Option<String>,
    checking: Option<Checking>,
}

pub(super) async fn run(
    daemon: Rc<Daemon>,
    service: Option<Rc<Service>>,
    mut commands: Receiver<(u64, Command)>,
) {
    let mut player = QueuePlayer::default();
    let mut active_epoch = daemon.queue_epoch.get();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            cmd = commands.recv() => {
                let Some((epoch, cmd)) = cmd else { break };
                if epoch == daemon.queue_epoch.get() {
                    active_epoch = epoch;
                    player.command(&daemon, cmd);
                } else {
                    continue;
                }
            }
            reply = receive(&mut player.active), if active_epoch == daemon.queue_epoch.get() => {
                player.reply(&daemon, reply);
            }
            reply = receive_check(&mut player.checking) => {
                player.checked(&daemon, service.as_ref(), reply);
            }
            _ = tick.tick(), if active_epoch == daemon.queue_epoch.get() && player.playing && player.active.is_none() => {}
        }
        player.start_check(&daemon, service.as_ref());
        player.start(&daemon, service.as_ref());
    }
}

async fn receive(active: &mut Option<Active>) -> Option<Reply> {
    match active {
        Some(active) => active.events.recv().await,
        None => std::future::pending().await,
    }
}

async fn receive_check(checking: &mut Option<Checking>) -> Option<Reply> {
    match checking {
        Some(checking) => checking.events.recv().await,
        None => std::future::pending().await,
    }
}

impl QueuePlayer {
    fn number(&mut self, items: Vec<Item>) -> Vec<Item> {
        items
            .into_iter()
            .map(|mut item| {
                self.occurrence += 1;
                item.occurrence_id = format!("q:{}", self.occurrence);
                item
            })
            .collect()
    }

    fn command(&mut self, daemon: &Rc<Daemon>, cmd: Command) {
        match cmd {
            Command::SetQueue {
                items,
                start_position,
                start_playing,
                ..
            } => {
                self.items = self.number(items);
                self.select(daemon, start_position, start_playing);
            }
            Command::ClearQueue => {
                self.items.clear();
                self.select(daemon, 0, false);
            }
            Command::PlayNext { songs } => self.enqueue(daemon, songs, true),
            Command::PlayLater { songs } => self.enqueue(daemon, songs, false),
            Command::RemoveFromQueue { index } => self.remove(daemon, index),
            Command::Check { id } => self.check(daemon, id),
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

    /// Stops the track holding the slot, not pausing it: a paused track keeps
    /// the slot, and the check needs it. The check starts once it is free.
    fn check(&mut self, daemon: &Daemon, id: String) {
        if self.pending_check.is_some() || self.checking.is_some() {
            publish_checked(daemon, id, false, "Another check is running.".into(), None);
            return;
        }
        self.playing = false;
        if let Some(active) = &mut self.active
            && !active.stopping
        {
            active.stopping = true;
            let _ = active.control.send(PlaybackControl::Stop);
        }
        Self::state(daemon, PlaybackState::Paused);
        self.pending_check = Some(id);
    }

    fn start_check(&mut self, daemon: &Daemon, service: Option<&Rc<Service>>) {
        if self.active.is_some() || self.checking.is_some() {
            return;
        }
        let Some(id) = self.pending_check.take() else {
            return;
        };
        let Some(service) = service else {
            let outcome = "Playback is unavailable, so nothing can be checked.".into();
            publish_checked(daemon, id, false, outcome, None);
            return;
        };
        tracing::info!(id, "checking a track");
        let (control, controls) = mpsc::unbounded_channel();
        match service.start_check(id.clone(), controls) {
            Ok(events) => {
                self.checking = Some(Checking {
                    id,
                    _control: control,
                    events,
                });
            }
            Err(Reply::SignedOut) => {
                let outcome = "Sign in before checking a track.".into();
                publish_checked(daemon, id, false, outcome, None);
            }
            Err(_) => {
                let outcome = "The playback worker is busy or unavailable.".into();
                publish_checked(daemon, id, false, outcome, None);
            }
        }
    }

    fn checked(&mut self, daemon: &Daemon, service: Option<&Rc<Service>>, reply: Option<Reply>) {
        if reply.is_some_and(Reply::is_playback_update) {
            return;
        }
        let Some(checking) = self.checking.take() else {
            return;
        };
        let (played, outcome) = match reply {
            Some(Reply::Played {
                sample_rate,
                bits_per_sample,
                channels,
            }) => (
                true,
                format!(
                    "Played through: {bits_per_sample}-bit, {sample_rate} Hz, {channels} channel(s)."
                ),
            ),
            Some(Reply::PlaybackFailed { stage, fault }) => (false, failure_text(stage, fault)),
            Some(Reply::LicenceDenied) => {
                (false, "Apple Music denied the playback licence.".into())
            }
            Some(Reply::Stopped) => (false, "The check was stopped.".into()),
            Some(Reply::SignedOut) => (false, "Signed out during the check.".into()),
            _ => (false, "The playback worker stopped.".into()),
        };
        let detail = service
            .and_then(|service| service.take_diagnosis())
            .map(|diagnosis| format!("{diagnosis:?}"));
        tracing::info!(id = checking.id, played, outcome, ?detail, "check finished");
        publish_checked(daemon, checking.id, played, outcome, detail);
    }

    fn set_playing(&mut self, daemon: &Daemon, playing: bool) {
        if self.playing == playing {
            return;
        }
        self.playing = playing && !self.items.is_empty();
        if !self.playing {
            daemon.require_sign_in(false);
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
        let new = self.number(daemon.queue_items(songs));
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
            daemon.require_sign_in(false);
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
        let checking = self.pending_check.is_some() || self.checking.is_some();
        if !self.playing || self.active.is_some() || checking {
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
            self.fail(
                daemon,
                "Playback is unavailable. Prepare the FairPlay provider and restart Slipmat.",
            );
            return;
        };
        if service.is_authenticating() {
            return;
        }
        if !service.is_signed_in() {
            if !daemon.sign_in_required() {
                Self::state(daemon, PlaybackState::Paused);
                daemon.require_sign_in(true);
            }
            return;
        }
        daemon.require_sign_in(false);
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
            Err(_) => self.fail(daemon, "The playback worker is busy or unavailable."),
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
                    Some(Reply::PlaybackFailed { stage, fault }) => {
                        let (position_ms, duration_ms) = {
                            let model = daemon.model.borrow();
                            (model.player.position_ms, model.player.duration_ms)
                        };
                        tracing::warn!(
                            ?stage,
                            ?fault,
                            track = ?self.items.get(self.index).and_then(|item| item.catalog_id.as_deref()),
                            position_ms,
                            duration_ms,
                            "playback failed",
                        );
                        self.fail(daemon, &failure_text(stage, fault));
                    }
                    _ => self.fail(daemon, "The playback worker stopped."),
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
        daemon.require_sign_in(false);
        Self::state(daemon, PlaybackState::Paused);
        daemon.publish(Event::PlaybackFailed {
            detail: detail.into(),
        });
    }
}

/// The toast for a track the provider could not play: the fault when it named
/// one, since "the stream could not be completed" covers a dozen causes.
fn failure_text(stage: PlaybackStage, fault: Option<PlaybackFault>) -> String {
    let Some(fault) = fault else {
        return stage_text(stage).to_owned();
    };
    match fault {
        PlaybackFault::Playlist => "The track's HLS playlist could not be loaded.".into(),
        PlaybackFault::PrefetchKey => "FairPlay would not open the shared prefetch key.".into(),
        PlaybackFault::Worker => "The FairPlay library stopped responding.".into(),
        PlaybackFault::Stalled => "The download stalled and could not be resumed.".into(),
        PlaybackFault::Dropped => "The download broke off and could not be resumed.".into(),
        PlaybackFault::Refused {
            status,
            resume: true,
        } => format!("Apple's CDN refused to resume the download (HTTP {status})."),
        PlaybackFault::Refused {
            status,
            resume: false,
        } => format!("Apple's CDN refused the download (HTTP {status})."),
        PlaybackFault::Container => "The track's file was not in the expected format.".into(),
        PlaybackFault::KeyReopen => {
            "The track's key expired during playback and could not be renewed.".into()
        }
        PlaybackFault::Decrypt { status } => {
            format!("FairPlay could not decrypt part of the track (status {status}).")
        }
        PlaybackFault::Decoder {
            step,
            decoded_ms,
            ignored_keys,
            first_ignored_at_ms,
            ..
        } => {
            let at = decoded_ms.map_or_else(String::new, |ms| format!(" at {}", clock(ms)));
            let what = match step {
                Some(DecodeStep::Container) => format!("The track's file could not be read{at}."),
                Some(DecodeStep::Audio) => format!("The track's audio could not be decoded{at}."),
                Some(DecodeStep::EndedEarly) => format!("The track's audio ended early{at}."),
                None => "The decrypted audio could not be decoded.".to_owned(),
            };
            if ignored_keys == 0 {
                return what;
            }
            format!(
                "{what} The track changes key {ignored_keys} time(s), first at {}, and \
                 Slipmat does not follow key changes.",
                first_ignored_at_ms.map_or_else(|| "an unknown point".to_owned(), clock)
            )
        }
        PlaybackFault::AudioServer => "The audio server stopped the stream.".into(),
    }
}

fn publish_checked(
    daemon: &Daemon,
    id: String,
    played: bool,
    outcome: String,
    detail: Option<String>,
) {
    daemon.publish(Event::Checked {
        id,
        played,
        outcome,
        detail,
    });
}

/// `m:ss`, the way the Now Playing clock shows it.
fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn stage_text(stage: PlaybackStage) -> &'static str {
    match stage {
        PlaybackStage::Session => "The session is unavailable.",
        PlaybackStage::Storefront => "Apple Music storefront lookup failed.",
        PlaybackStage::Catalogue => "Apple Music catalogue lookup failed.",
        PlaybackStage::CatalogueAuthorization => "Apple Music rejected catalogue authorization.",
        PlaybackStage::CatalogueTrack => {
            "Apple Music could not find this track in your storefront."
        }
        PlaybackStage::CatalogueAsset => "Apple Music returned no lossless asset for this track.",
        PlaybackStage::Stream => "The FairPlay stream could not be opened or completed.",
        PlaybackStage::Output => "The FairPlay audio output failed.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_fault_replaces_the_stage_in_the_toast() {
        assert_eq!(
            failure_text(PlaybackStage::Stream, None),
            "The FairPlay stream could not be opened or completed."
        );
        assert_eq!(
            failure_text(
                PlaybackStage::Stream,
                Some(PlaybackFault::Refused {
                    status: 403,
                    resume: true
                })
            ),
            "Apple's CDN refused to resume the download (HTTP 403)."
        );
        assert_eq!(
            failure_text(
                PlaybackStage::Stream,
                Some(PlaybackFault::Decrypt { status: -2 })
            ),
            "FairPlay could not decrypt part of the track (status -2)."
        );
        assert_eq!(
            failure_text(
                PlaybackStage::Output,
                Some(PlaybackFault::Decoder {
                    step: Some(DecodeStep::Audio),
                    decoded_ms: Some(987_012),
                    ignored_keys: 2,
                    first_ignored_at_ms: Some(986_000)
                })
            ),
            "The track's audio could not be decoded at 16:27. The track changes key 2 \
             time(s), first at 16:26, and Slipmat does not follow key changes."
        );
        assert_eq!(
            failure_text(
                PlaybackStage::Output,
                Some(PlaybackFault::Decoder {
                    step: None,
                    decoded_ms: None,
                    ignored_keys: 0,
                    first_ignored_at_ms: None
                })
            ),
            "The decrypted audio could not be decoded."
        );
    }
}
