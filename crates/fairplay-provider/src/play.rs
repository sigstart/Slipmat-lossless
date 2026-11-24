// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-FileCopyrightText: 2026 Jan Vermeulen
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
//! A download that stalls for 30 s, breaks off or meets a 5xx resumes with a
//! `Range` request from the start of the box it was in, up to three times. A
//! CDN that ignores the range and answers 200 is read past what we have.
//!
//! Every failure carries a [`PlaybackFault`], so the daemon can say which of
//! these it was rather than "the stream could not be completed".

use std::io::{self, Read};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use fairplay_wire::{
    Catalogue, DecodeStep, DescriptionChange, Diagnosis, FragmentShape, KeyChange, MAX_CHANGES,
    NamedKey, PlaybackFault, PlaybackStage, Reply, StreamCodec,
};
use reqwest::StatusCode;
use slipmat_playback::{
    Decoder, PcmFormat, PlaybackEvent, Transport, Undecodable, play_controlled,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, timeout};

use crate::catalogue::{self, Source};
use crate::cbcs::{Boxes, Clearer, Piece};
use crate::fairplay::{Fairplay, KeyRequest};
use crate::hls::{self, KeySystem, Media, PREFETCH_KEY};
use crate::mp4::{AlacConfig, Protection, Shape};
use crate::widevine::{self, Widevine};

const AUDIO_SOCKET: &str = "/audio/native";
const STALL: Duration = Duration::from_secs(30);
const RETRIES: u32 = 3;
/// Apple's key server refuses a track key now and then, and grants the same
/// key minutes later on the same session. A real denial outlasts these.
const KEY_RETRIES: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(4)];
/// A key open takes under a second; this is a wedged library, not a slow one.
const KEY_OPEN_LIMIT: Duration = Duration::from_secs(60);
/// The provider's exit status when it ends itself over a wedged key open.
const KEY_OPEN_HUNG: i32 = 70;
/// Fragments buffered ahead of the decoder.
const PIPE_FRAGMENTS: usize = 4;

/// How a track's samples are decrypted, once its licence is granted.
enum Keys {
    /// Contexts open on the fairplay thread, by index into `Media::keys`.
    FairPlay,
    /// A CDM session holding the track's key, locked because decrypting
    /// borrows it across awaits.
    Widevine {
        host: Box<tokio::sync::Mutex<Widevine>>,
        key_id: [u8; 16],
    },
}

/// Where the decoded audio goes.
#[derive(Clone, Copy)]
pub enum Output {
    Speakers,
    /// Nowhere: the track is decoded as fast as it arrives, to check it.
    /// Pause and resume do nothing; stopping still works, by closing the pipe.
    Discard,
}

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
    /// Set once a Widevine licence has taken the account's active session,
    /// so the next FairPlay track renews the lease first.
    lease_taken: std::sync::atomic::AtomicBool,
    catalogue: Catalogue,
    api: reqwest::Client,
    cdn: reqwest::Client,
}

impl Player {
    pub fn new(fairplay: Arc<Fairplay>, catalogue: Catalogue) -> Result<Self> {
        Ok(Self {
            fairplay,
            lease_taken: std::sync::atomic::AtomicBool::new(false),
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
    output: Output,
    updates: mpsc::UnboundedSender<Update>,
) -> mpsc::UnboundedSender<Control> {
    let (controls, inbox) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let reply = player.play(&track_id, output, inbox, &updates).await;
        let _ = updates.send(Update::Finished(reply));
    });
    controls
}

fn failed(stage: PlaybackStage) -> Reply {
    Reply::PlaybackFailed {
        stage,
        fault: None,
        diagnosis: None,
    }
}

fn faulted(stage: PlaybackStage, fault: PlaybackFault) -> Reply {
    Reply::PlaybackFailed {
        stage,
        fault: Some(fault),
        diagnosis: None,
    }
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
        output: Output,
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
            Ok((codec, media, keys)) => {
                let _ = updates.send(Update::Playback(Reply::Streaming { codec }));
                self.stream(&media, &keys, output, paused, controls, updates)
                    .await
            }
            Err(reply) => reply,
        }
    }

    /// Everything before audio: the lookups, then the licence gate.
    async fn prepare(&self, track_id: &str) -> Result<(StreamCodec, Media, Keys), Reply> {
        if !is_track_id(track_id) {
            return Err(failed(PlaybackStage::Catalogue));
        }
        let playlist_fault = || faulted(PlaybackStage::Stream, PlaybackFault::Playlist);
        match self.locate(track_id).await? {
            Source::Master(master) => {
                let (codec, media) = hls::load(&self.cdn, &master)
                    .await
                    .map_err(|_| playlist_fault())?;
                if media.system != KeySystem::FairPlay {
                    return Err(playlist_fault());
                }
                self.open_fairplay(track_id, &media).await?;
                Ok((codec, media, Keys::FairPlay))
            }
            Source::Widevine {
                playlist,
                certificate,
            } => {
                if !widevine::installed() {
                    return Err(faulted(
                        PlaybackStage::Stream,
                        PlaybackFault::WidevineMissing,
                    ));
                }
                let media = hls::load_media(&self.cdn, &playlist)
                    .await
                    .map_err(|_| playlist_fault())?;
                if media.system != KeySystem::Widevine {
                    return Err(playlist_fault());
                }
                let keys = self
                    .license_widevine(track_id, &media, &certificate)
                    .await?;
                Ok((StreamCodec::Aac, media, keys))
            }
        }
    }

    /// The licence gate for FairPlay: every key the playlist names opens.
    async fn open_fairplay(&self, track_id: &str, media: &Media) -> Result<(), Reply> {
        if self
            .lease_taken
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let renewed = self.fairplay.renew_lease().await;
            eprintln!("[play] key lease renewed after a Widevine track: {renewed}");
        }
        let keys = media
            .keys
            .iter()
            .map(|uri| KeyRequest {
                adam: if uri == PREFETCH_KEY { "0" } else { track_id }.to_owned(),
                uri: uri.clone(),
            })
            .collect();
        match self.open_keys(keys).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(refused)) if media.keys[refused] != PREFETCH_KEY => Err(Reply::LicenceDenied),
            Ok(Err(_)) => Err(faulted(PlaybackStage::Stream, PlaybackFault::PrefetchKey)),
            Err(_) => Err(faulted(PlaybackStage::Stream, PlaybackFault::Worker)),
        }
    }

    /// The licence gate for Widevine: a CDM session whose key Apple licensed.
    async fn license_widevine(
        &self,
        track_id: &str,
        media: &Media,
        certificate: &reqwest::Url,
    ) -> Result<Keys, Reply> {
        let playlist_fault = || faulted(PlaybackStage::Stream, PlaybackFault::Playlist);
        let host_fault = |err: anyhow::Error| {
            eprintln!("[widevine] {err:#}");
            faulted(PlaybackStage::Stream, PlaybackFault::Widevine)
        };
        let key_uri = media.keys.first().ok_or_else(playlist_fault)?;
        let key_id = widevine::key_id(key_uri).map_err(|_| playlist_fault())?;
        let certificate = self
            .small_file(certificate)
            .await
            .map_err(|_| playlist_fault())?;
        let mut host = Widevine::start().await.map_err(host_fault)?;
        // Why Apple refused, kept apart from the host's own failures.
        let refused = std::sync::Mutex::new(None);
        let licensed = host
            .license(&certificate, &key_id, |challenge| {
                let refused = &refused;
                async move {
                    catalogue::widevine_license(
                        &self.api,
                        &self.catalogue,
                        track_id,
                        key_uri,
                        &challenge,
                    )
                    .await
                    .map_err(|stage| {
                        if let Ok(mut refused) = refused.lock() {
                            *refused = Some(stage);
                        }
                        anyhow!("the licence request failed")
                    })
                }
            })
            .await;
        let refused = refused.lock().ok().and_then(|refused| *refused);
        match (licensed, refused) {
            (Ok(()), _) => {
                self.lease_taken
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(Keys::Widevine {
                    host: Box::new(tokio::sync::Mutex::new(host)),
                    key_id,
                })
            }
            (Err(_), Some(PlaybackStage::CatalogueAuthorization)) => Err(Reply::LicenceDenied),
            (Err(_), Some(stage)) => Err(failed(stage)),
            (Err(err), None) => Err(host_fault(err)),
        }
    }

    /// A certificate or similar: one small GET from the CDN.
    async fn small_file(&self, url: &reqwest::Url) -> Result<Vec<u8>> {
        let response = self.cdn.get(url.clone()).send().await?;
        anyhow::ensure!(response.status() == StatusCode::OK, "refused");
        let bytes = response.bytes().await?;
        anyhow::ensure!(bytes.len() <= 1 << 16, "too large");
        Ok(bytes.to_vec())
    }

    /// Where a track's stream is: the catalogue lossless master, else the
    /// webPlayback AAC flavor for a track with no lossless master.
    async fn locate(&self, track_id: &str) -> Result<Source, Reply> {
        match catalogue::lossless_asset(&self.api, &self.catalogue, track_id).await {
            Ok(master) => Ok(Source::Master(master)),
            Err(PlaybackStage::CatalogueAsset) => {
                catalogue::web_playback_aac(&self.api, &self.catalogue, track_id)
                    .await
                    .map_err(failed)
            }
            Err(stage) => Err(failed(stage)),
        }
    }

    async fn open_keys(&self, keys: Vec<KeyRequest>) -> Result<Result<(), usize>> {
        let mut opened = self.open_keys_once(keys.clone()).await;
        for delay in KEY_RETRIES {
            let Ok(Err(refused)) = opened else { break };
            eprintln!("[play] key {refused} refused; retrying in {delay:?}");
            sleep(delay).await;
            opened = self.open_keys_once(keys.clone()).await;
        }
        opened
    }

    /// A key open that never returns means the fairplay thread is stuck in
    /// Apple's code, where nothing can cancel it: a lock its own SVError left
    /// held has done this. Only a new process recovers, and the daemon
    /// restarts this one and restores the session (rule 6).
    async fn open_keys_once(&self, keys: Vec<KeyRequest>) -> Result<Result<(), usize>> {
        match timeout(KEY_OPEN_LIMIT, self.fairplay.open_keys(keys)).await {
            Ok(opened) => opened,
            Err(_) => {
                eprintln!("[play] key open hung; restarting the provider");
                std::process::exit(KEY_OPEN_HUNG);
            }
        }
    }

    async fn stream(
        &self,
        media: &Media,
        keys: &Keys,
        output: Output,
        paused: bool,
        mut controls: mpsc::UnboundedReceiver<Control>,
        updates: &mpsc::UnboundedSender<Update>,
    ) -> Reply {
        let (pipe, pieces) = mpsc::channel(PIPE_FRAGMENTS);
        let Ok(mut audio) = Audio::start(PipeReader::new(pieces), output, paused) else {
            return failed(PlaybackStage::Output);
        };
        let seen = Seen::new(Fed {
            shapes: vec![None; media.segments.len()],
            configs: [None; 2],
            protections: [None; 2],
            key_ages: vec![None; media.segments.len()],
        });
        let mut feed: Option<Feeding<'_>> = Some(Box::pin(self.feed(media, keys, pipe, &seen)));
        let mut feed_fault = None;
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
                    if let Err(fault) = fed {
                        // The decoder would take a cut-off stream for its end.
                        feed_fault = Some(fault);
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
                    return match (feed_fault, done) {
                        (Some(fault), _) => faulted(PlaybackStage::Stream, fault),
                        (None, Ok(Ok(format))) => played(format),
                        (None, Ok(Err(error))) => output_failure(&error, media, &seen),
                        (None, Err(_)) => failed(PlaybackStage::Output),
                    };
                }
            }
        }
    }

    /// Downloads, clears and pipes the whole file, resuming after breaks.
    async fn feed(
        &self,
        media: &Media,
        keys: &Keys,
        pipe: mpsc::Sender<Vec<u8>>,
        seen: &Seen,
    ) -> Result<(), PlaybackFault> {
        let mut boxes = Boxes::default();
        let mut clearer = Clearer::default();
        let mut retries = 0;
        loop {
            let offset = boxes.resume_at();
            match self
                .download(media, keys, offset, &mut boxes, &mut clearer, &pipe, seen)
                .await
            {
                Err(fault) if resumable(fault) && retries < RETRIES => {
                    retries += 1;
                    sleep(Duration::from_secs(u64::from(retries))).await;
                }
                done => return done,
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn download(
        &self,
        media: &Media,
        keys: &Keys,
        offset: u64,
        boxes: &mut Boxes,
        clearer: &mut Clearer,
        pipe: &mpsc::Sender<Vec<u8>>,
        seen: &Seen,
    ) -> Result<(), PlaybackFault> {
        let mut request = self.cdn.get(media.file.clone());
        if offset > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
        }
        let mut response = timeout(STALL, request.send())
            .await
            .map_err(|_| PlaybackFault::Stalled)?
            .map_err(|_| PlaybackFault::Dropped)?;
        let mut skip = match response.status() {
            StatusCode::OK => offset,
            StatusCode::PARTIAL_CONTENT if offset > 0 => 0,
            status => {
                return Err(PlaybackFault::Refused {
                    status: status.as_u16(),
                    resume: offset > 0,
                });
            }
        };
        loop {
            let chunk = timeout(STALL, response.chunk())
                .await
                .map_err(|_| PlaybackFault::Stalled)?
                .map_err(|_| PlaybackFault::Dropped)?;
            let Some(mut chunk) = chunk else {
                return if skip == 0 && boxes.is_between_boxes() {
                    Ok(())
                } else {
                    Err(PlaybackFault::Dropped)
                };
            };
            if skip > 0 {
                let skipped = chunk.len().min(usize::try_from(skip).unwrap_or(usize::MAX));
                skip -= skipped as u64;
                chunk = chunk.slice(skipped..);
            }
            boxes.push(&chunk);
            while let Some(whole) = boxes.next().map_err(|_| PlaybackFault::Container)? {
                let piece = clearer.push(whole).map_err(|_| PlaybackFault::Container)?;
                let bytes = match piece {
                    None => continue,
                    Some(Piece::Init(bytes)) => {
                        if let (Ok(mut seen), Some(track)) = (seen.lock(), clearer.track()) {
                            seen.configs = track.configs;
                            seen.protections = track.protections;
                        }
                        bytes
                    }
                    Some(Piece::Fragment { at, fragment }) => {
                        let segment = media.segment_at(at).ok_or(PlaybackFault::Container)?;
                        if let Ok(mut seen) = seen.lock()
                            && let Some(slot) = seen.shapes.get_mut(segment)
                        {
                            *slot = Some(fragment.shape);
                        }
                        let (fragment, key_age) = match keys {
                            Keys::FairPlay => {
                                let key = media.segments[segment].key;
                                self.fairplay.decrypt(key, fragment).await?
                            }
                            Keys::Widevine { host, key_id } => {
                                let mut fragment = fragment;
                                host.lock()
                                    .await
                                    .decrypt(key_id, &mut fragment)
                                    .await
                                    .map_err(|err| {
                                        eprintln!("[widevine] {err:#}");
                                        PlaybackFault::Widevine
                                    })?;
                                (fragment, Duration::ZERO)
                            }
                        };
                        if let Ok(mut seen) = seen.lock()
                            && let Some(slot) = seen.key_ages.get_mut(segment)
                        {
                            *slot = u64::try_from(key_age.as_millis()).ok();
                        }
                        fragment.bytes
                    }
                };
                // The decoder stopped reading: its own result says why.
                if pipe.send(bytes).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// A break worth resuming from the box it was in.
fn resumable(fault: PlaybackFault) -> bool {
    match fault {
        PlaybackFault::Stalled | PlaybackFault::Dropped => true,
        PlaybackFault::Refused { status, .. } => status >= 500,
        _ => false,
    }
}

fn output_failure(error: &anyhow::Error, media: &Media, seen: &Seen) -> Reply {
    let Some(failed) = error.downcast_ref::<Undecodable>() else {
        return faulted(PlaybackStage::Output, PlaybackFault::AudioServer);
    };
    let seen = seen.lock().ok();
    let shapes = seen.as_ref().map_or(&[][..], |seen| &seen.shapes[..]);
    let configs = seen.as_ref().map_or([None; 2], |seen| seen.configs);
    let protections = seen.as_ref().map_or([None; 2], |seen| seen.protections);
    let shape_of = |index: usize| -> Option<FragmentShape> {
        let shape = (*shapes.get(index)?)?;
        Some(FragmentShape {
            index: u32::try_from(index).ok()?,
            starts_at_ms: media.segments.get(index)?.starts_at_ms,
            samples: shape.samples,
            split_samples: shape.split_samples,
            tfhd_flags: shape.tfhd_flags,
            trun_flags: shape.trun_flags,
            description: shape.description,
            key_age_ms: seen.as_ref()?.key_ages.get(index).copied().flatten(),
        })
    };
    let first = shapes.iter().position(Option::is_some);
    Reply::PlaybackFailed {
        stage: PlaybackStage::Output,
        fault: Some(PlaybackFault::Decoder {
            step: Some(match failed.step {
                slipmat_playback::DecodeStep::Container => DecodeStep::Container,
                slipmat_playback::DecodeStep::Audio => DecodeStep::Audio,
                slipmat_playback::DecodeStep::EndedEarly => DecodeStep::EndedEarly,
            }),
            decoded_ms: Some(failed.decoded_ms),
            ignored_keys: media.ignored.count,
            first_ignored_at_ms: media.ignored.first_at_ms,
        }),
        diagnosis: Some(Box::new(Diagnosis {
            fragment: failing_fragment(shapes, failed.decoded_frames).and_then(shape_of),
            first_fragment: first.and_then(shape_of),
            first_config: configs[0].map(wire_config),
            second_config: configs[1].map(wire_config),
            first_protection: protections[0].map(wire_protection),
            second_protection: protections[1].map(wire_protection),
            key_changes: changes(media.segments.iter().map(|segment| Some(segment.named)))
                .map(|(segment, key)| KeyChange {
                    segment,
                    key: match key {
                        hls::NamedKey::Prefetch => NamedKey::Prefetch,
                        hls::NamedKey::Track => NamedKey::Track,
                        hls::NamedKey::Other => NamedKey::Other,
                    },
                })
                .collect(),
            description_changes: changes(
                shapes
                    .iter()
                    .map(|shape| shape.map(|shape| shape.description)),
            )
            .map(|(segment, description)| DescriptionChange {
                segment,
                description,
            })
            .collect(),
        })),
    }
}

/// The fragment the decoder stopped in: counted in frames, because the
/// playlist's segment durations are rounded and drift across a long track.
fn failing_fragment(shapes: &[Option<Shape>], decoded_frames: u64) -> Option<usize> {
    let mut start = 0;
    for (index, shape) in shapes.iter().enumerate() {
        let frames = (*shape)?.frames;
        if decoded_frames < start + frames {
            return Some(index);
        }
        start += frames;
    }
    None
}

/// Each segment where the value differs from the one before, until the first
/// unknown, at most [`MAX_CHANGES`] of them.
fn changes<T: Copy + PartialEq>(
    values: impl Iterator<Item = Option<T>>,
) -> impl Iterator<Item = (u32, T)> {
    let mut previous = None;
    values
        .map_while(|value| value)
        .enumerate()
        .filter_map(move |(index, value)| {
            let changed = previous != Some(value);
            previous = Some(value);
            changed.then_some((u32::try_from(index).ok()?, value))
        })
        .take(MAX_CHANGES)
}

fn wire_protection(protection: Protection) -> fairplay_wire::Protection {
    let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    fairplay_wire::Protection {
        key_id: hex(&protection.key_id),
        iv_size: protection.iv_size,
        constant_iv: protection
            .constant_iv
            .map(|(len, iv)| hex(&iv[..usize::from(len).min(iv.len())])),
    }
}

fn wire_config(config: AlacConfig) -> fairplay_wire::AlacConfig {
    fairplay_wire::AlacConfig {
        frame_length: config.frame_length,
        bit_depth: config.bit_depth,
        channels: config.channels,
        max_frame_bytes: config.max_frame_bytes,
        sample_rate: config.sample_rate,
    }
}

/// What the feed saw of the file, kept to explain a decoder failure.
type Seen = std::sync::Mutex<Fed>;

struct Fed {
    /// Each segment's fragment, once fed.
    shapes: Vec<Option<Shape>>,
    configs: [Option<AlacConfig>; 2],
    protections: [Option<Protection>; 2],
    /// How old each segment's key context was when it decrypted the fragment.
    key_ages: Vec<Option<u64>>,
}

/// The download in progress, dropped to cancel it and close the pipe.
type Feeding<'a> = Pin<Box<dyn Future<Output = Result<(), PlaybackFault>> + Send + 'a>>;

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
    fn start(source: PipeReader, output: Output, start_paused: bool) -> io::Result<Self> {
        let (commands, inbox) = std::sync::mpsc::channel();
        let (report, events) = mpsc::unbounded_channel();
        let (finished, done) = oneshot::channel();
        std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                let result = (|| {
                    let mut decoder = Decoder::open_fragmented(source)?;
                    let format = decoder.format();
                    if matches!(output, Output::Discard) {
                        while decoder.next_block()?.is_some() {}
                        return Ok(format);
                    }
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
    fn the_failing_fragment_is_the_one_holding_the_next_frame() {
        let fed = |frames| {
            Some(Shape {
                frames,
                ..Shape::default()
            })
        };
        let shapes = [fed(10), fed(10), fed(10), None];
        // Exactly two fragments decoded: the next frame is the third's first.
        assert_eq!(failing_fragment(&shapes, 20), Some(2));
        assert_eq!(failing_fragment(&shapes, 19), Some(1));
        assert_eq!(failing_fragment(&shapes, 0), Some(0));
        // Past everything fed: nothing to name.
        assert_eq!(failing_fragment(&shapes, 30), None);
    }

    #[test]
    fn changes_list_each_new_value_until_the_first_unknown() {
        let values = [Some(0), Some(0), Some(1), Some(1), Some(2), None, Some(3)];
        assert_eq!(
            changes(values.into_iter()).collect::<Vec<_>>(),
            [(0, 0), (2, 1), (4, 2)]
        );
        let many = (0..100).map(Some);
        assert_eq!(changes(many).count(), MAX_CHANGES);
    }

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
