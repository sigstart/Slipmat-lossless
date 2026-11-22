// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! One track, from the catalogue lookup to the last sample at the audio server.
//!
//! 1. **Catalogue and HLS.** The track's lossless asset, the ALAC variant,
//!    and the media playlist's file, segments and keys.
//! 2. **The licence gate.** Every key is opened before anything is
//!    downloaded. The prefetch key decrypts the first segment of any track, so
//!    without this a refused licence could arrive after audible output. A track
//!    key that does not open is `licenceDenied`, before any sound.
//! 3. **Streaming.** The file downloads with one GET, cut into whole boxes,
//!    made clear fragment by fragment on the fairplay thread, and written into
//!    a pipe of a few fragments that the decoder reads on its own thread.
//!    Decrypted audio exists only in that pipe and in the decoder.
//!
//! A download that stalls for 30 s or breaks off resumes with a `Range`
//! request from the start of the box it was in, up to three times. A CDN that
//! refuses ranges fails the track as `stream`.

use std::io::{self, Read};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use fairplay_wire::{Catalogue, PlaybackStage, Reply};
use reqwest::StatusCode;
use slipmat_playback::{AlacDecoder, PcmFormat, PlaybackEvent, Transport, play_controlled};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, timeout};

use crate::catalogue;
use crate::cbcs::{Boxes, Clearer, Piece};
use crate::fairplay::{Fairplay, KeyRequest};
use crate::hls::{self, Media, PREFETCH_KEY};

const AUDIO_SOCKET: &str = "/audio/native";
const STALL: Duration = Duration::from_secs(30);
const RETRIES: u32 = 3;
/// Fragments buffered ahead of the decoder.
const PIPE_FRAGMENTS: usize = 4;

pub enum Control {
    Pause,
    Resume,
    Stop,
}

pub enum Update {
    /// `playing`, `paused` or `position`, while the track plays.
    Playback(Reply),
    /// How the track ended; nothing follows it.
    Finished(Reply),
}

pub struct Player {
    fairplay: Arc<Fairplay>,
    catalogue: Catalogue,
    api: reqwest::Client,
    cdn: reqwest::Client,
}

impl Player {
    pub fn new(fairplay: Arc<Fairplay>, catalogue: Catalogue) -> Result<Self> {
        Ok(Self {
            fairplay,
            catalogue,
            api: catalogue::client()?,
            cdn: reqwest::Client::builder()
                .connect_timeout(STALL)
                .redirect(reqwest::redirect::Policy::limited(3))
                .build()?,
        })
    }
}

/// Starts a track. Its updates arrive on `updates`, ending with `Finished`.
pub fn spawn(
    player: Arc<Player>,
    track_id: String,
    updates: mpsc::UnboundedSender<Update>,
) -> mpsc::UnboundedSender<Control> {
    let (controls, inbox) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let reply = player.play(&track_id, inbox, &updates).await;
        let _ = updates.send(Update::Finished(reply));
    });
    controls
}

fn failed(stage: PlaybackStage) -> Reply {
    Reply::PlaybackFailed { stage }
}

fn is_track_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 20
        && id.bytes().all(|byte| byte.is_ascii_digit())
        && id.bytes().any(|byte| byte != b'0')
}

impl Player {
    async fn play(
        &self,
        track_id: &str,
        mut controls: mpsc::UnboundedReceiver<Control>,
        updates: &mpsc::UnboundedSender<Update>,
    ) -> Reply {
        let mut paused = false;
        let mut prepared = std::pin::pin!(self.prepare(track_id));
        let media = loop {
            tokio::select! {
                media = &mut prepared => break media,
                control = controls.recv() => match control {
                    Some(Control::Pause) => paused = true,
                    Some(Control::Resume) => paused = false,
                    Some(Control::Stop) | None => return Reply::Stopped,
                },
            }
        };
        match media {
            Ok(media) => self.stream(&media, paused, controls, updates).await,
            Err(reply) => reply,
        }
    }

    /// Everything before audio: the lookups, then the licence gate.
    async fn prepare(&self, track_id: &str) -> Result<Media, Reply> {
        if !is_track_id(track_id) {
            return Err(failed(PlaybackStage::Catalogue));
        }
        let asset = catalogue::lossless_asset(&self.api, &self.catalogue, track_id)
            .await
            .map_err(failed)?;
        let media = hls::load(&self.cdn, &asset)
            .await
            .map_err(|_| failed(PlaybackStage::Stream))?;
        let keys = media
            .keys
            .iter()
            .map(|uri| KeyRequest {
                adam: if uri == PREFETCH_KEY { "0" } else { track_id }.to_owned(),
                uri: uri.clone(),
            })
            .collect();
        match self.fairplay.open_keys(keys).await {
            Ok(Ok(())) => Ok(media),
            Ok(Err(refused)) if media.keys[refused] != PREFETCH_KEY => Err(Reply::LicenceDenied),
            _ => Err(failed(PlaybackStage::Stream)),
        }
    }

    async fn stream(
        &self,
        media: &Media,
        paused: bool,
        mut controls: mpsc::UnboundedReceiver<Control>,
        updates: &mpsc::UnboundedSender<Update>,
    ) -> Reply {
        let (pipe, pieces) = mpsc::channel(PIPE_FRAGMENTS);
        let Ok(mut audio) = Audio::start(PipeReader::new(pieces), paused) else {
            return failed(PlaybackStage::Output);
        };
        let mut feed: Option<Feeding<'_>> = Some(Box::pin(self.feed(media, pipe)));
        let mut feed_failed = false;
        loop {
            tokio::select! {
                biased;
                fed = async {
                    match feed.as_mut() {
                        Some(feeding) => feeding.await,
                        None => std::future::pending().await,
                    }
                }, if feed.is_some() => {
                    feed = None;
                    if fed.is_err() {
                        // The decoder would take a cut-off stream for its end.
                        feed_failed = true;
                        audio.stop();
                    }
                }
                Some(event) = audio.events.recv() => forward(updates, event),
                control = controls.recv() => match control {
                    Some(Control::Pause) => audio.send(Transport::Pause),
                    Some(Control::Resume) => audio.send(Transport::Resume),
                    Some(Control::Stop) | None => {
                        audio.stop();
                        drop(feed.take());
                        let _ = timeout(Duration::from_secs(2), &mut audio.done).await;
                        return Reply::Stopped;
                    }
                },
                done = &mut audio.done => {
                    while let Ok(event) = audio.events.try_recv() {
                        forward(updates, event);
                    }
                    return match done {
                        Ok(Ok(format)) if !feed_failed => played(format),
                        _ if feed_failed => failed(PlaybackStage::Stream),
                        _ => failed(PlaybackStage::Output),
                    };
                }
            }
        }
    }

    /// Downloads, clears and pipes the whole file, resuming after breaks.
    async fn feed(&self, media: &Media, pipe: mpsc::Sender<Vec<u8>>) -> Result<(), Fault> {
        let mut boxes = Boxes::default();
        let mut clearer = Clearer::default();
        let mut retries = 0;
        loop {
            let offset = boxes.resume_at();
            match self
                .download(media, offset, &mut boxes, &mut clearer, &pipe)
                .await
            {
                Err(Fault::Network) if retries < RETRIES => {
                    retries += 1;
                    sleep(Duration::from_secs(u64::from(retries))).await;
                }
                done => return done,
            }
        }
    }

    async fn download(
        &self,
        media: &Media,
        offset: u64,
        boxes: &mut Boxes,
        clearer: &mut Clearer,
        pipe: &mpsc::Sender<Vec<u8>>,
    ) -> Result<(), Fault> {
        let mut request = self.cdn.get(media.file.clone());
        let expected = if offset == 0 {
            StatusCode::OK
        } else {
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
            StatusCode::PARTIAL_CONTENT
        };
        let mut response = timeout(STALL, request.send())
            .await
            .map_err(|_| Fault::Network)?
            .map_err(|_| Fault::Network)?;
        if response.status() != expected {
            return Err(Fault::Other);
        }
        loop {
            let chunk = timeout(STALL, response.chunk())
                .await
                .map_err(|_| Fault::Network)?
                .map_err(|_| Fault::Network)?;
            let Some(chunk) = chunk else {
                return if boxes.is_between_boxes() {
                    Ok(())
                } else {
                    Err(Fault::Network)
                };
            };
            boxes.push(&chunk);
            while let Some(whole) = boxes.next().map_err(|_| Fault::Other)? {
                let piece = clearer.push(whole).map_err(|_| Fault::Other)?;
                let bytes = match piece {
                    None => continue,
                    Some(Piece::Init(bytes)) => bytes,
                    Some(Piece::Fragment { at, fragment }) => {
                        let key = media.key_at(at).ok_or(Fault::Other)?;
                        self.fairplay
                            .decrypt(key, fragment)
                            .await
                            .map_err(|_| Fault::Other)?
                            .bytes
                    }
                };
                pipe.send(bytes).await.map_err(|_| Fault::Other)?;
            }
        }
    }
}

/// Why the download stopped: a break worth resuming, or anything else.
enum Fault {
    Network,
    Other,
}

/// The download in progress, dropped to cancel it and close the pipe.
type Feeding<'a> = Pin<Box<dyn Future<Output = Result<(), Fault>> + Send + 'a>>;

fn played(format: PcmFormat) -> Reply {
    Reply::Played {
        sample_rate: format.sample_rate,
        bits_per_sample: format.bits_per_sample,
        channels: format.channels,
    }
}

fn forward(updates: &mpsc::UnboundedSender<Update>, event: PlaybackEvent) {
    let (state, position_ms) = match event {
        PlaybackEvent::Playing(position) => (Some(Reply::Playing), position),
        PlaybackEvent::Paused(position) => (Some(Reply::Paused), position),
        PlaybackEvent::Position(position) => (None, position),
    };
    let _ = updates.send(Update::Playback(Reply::Position { position_ms }));
    if let Some(state) = state {
        let _ = updates.send(Update::Playback(state));
    }
}

/// The decoder and audio output, on their own thread.
struct Audio {
    commands: std::sync::mpsc::Sender<Result<Transport>>,
    events: mpsc::UnboundedReceiver<PlaybackEvent>,
    done: oneshot::Receiver<Result<PcmFormat>>,
}

impl Audio {
    fn start(source: PipeReader, start_paused: bool) -> io::Result<Self> {
        let (commands, inbox) = std::sync::mpsc::channel();
        let (report, events) = mpsc::unbounded_channel();
        let (finished, done) = oneshot::channel();
        std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                let result = (|| {
                    let decoder = AlacDecoder::open_fragmented(source)?;
                    let format = decoder.format();
                    play_controlled(
                        decoder,
                        Path::new(AUDIO_SOCKET),
                        start_paused,
                        inbox,
                        |event| {
                            report
                                .send(event)
                                .map_err(|_| anyhow!("the track was abandoned"))
                        },
                    )?;
                    Ok(format)
                })();
                let _ = finished.send(result);
            })?;
        Ok(Self {
            commands,
            events,
            done,
        })
    }

    fn send(&self, transport: Transport) {
        let _ = self.commands.send(Ok(transport));
    }

    /// The player ends at its next transport check, paused or not.
    fn stop(&self) {
        let _ = self.commands.send(Err(anyhow!("stopped")));
    }
}

/// The decoder's end of the pipe. It blocks, so only the audio thread reads it.
struct PipeReader {
    pieces: std::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    current: Vec<u8>,
    at: usize,
}

impl PipeReader {
    fn new(pieces: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            pieces: std::sync::Mutex::new(pieces),
            current: Vec::new(),
            at: 0,
        }
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at == self.current.len() {
            let next = self
                .pieces
                .get_mut()
                .map_err(|_| io::Error::other("the pipe was poisoned"))?
                .blocking_recv();
            match next {
                Some(piece) => {
                    self.current = piece;
                    self.at = 0;
                }
                None => return Ok(0),
            }
        }
        let len = buf.len().min(self.current.len() - self.at);
        buf[..len].copy_from_slice(&self.current[self.at..self.at + len]);
        self.at += len;
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_ids_are_catalogue_numbers() {
        assert!(is_track_id("6817024685"));
        for id in ["", "0", "000", "12a", "-1", "123456789012345678901"] {
            assert!(!is_track_id(id), "{id}");
        }
    }

    #[test]
    fn the_pipe_reads_pieces_in_order_and_ends_when_closed() {
        let (send, receive) = mpsc::channel(4);
        send.try_send(vec![1, 2, 3]).unwrap();
        send.try_send(vec![4]).unwrap();
        drop(send);
        let mut reader = PipeReader::new(receive);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4]);
    }

    #[test]
    fn a_position_comes_before_the_state_it_describes() {
        let (updates, mut received) = mpsc::unbounded_channel();
        forward(&updates, PlaybackEvent::Paused(1250));
        forward(&updates, PlaybackEvent::Position(1500));
        let replies: Vec<_> = std::iter::from_fn(|| received.try_recv().ok())
            .map(|update| match update {
                Update::Playback(reply) | Update::Finished(reply) => reply,
            })
            .collect();
        assert_eq!(
            replies,
            [
                Reply::Position { position_ms: 1250 },
                Reply::Paused,
                Reply::Position { position_ms: 1500 }
            ]
        );
    }
}
