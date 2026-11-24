// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-FileCopyrightText: 2026 Jan Vermeulen
// SPDX-License-Identifier: GPL-3.0-or-later

//! Pull-based PCM decoding and local audio output for clear ALAC or AAC-LC in
//! an MP4 container.

mod fragmented;
mod pulse;

pub use pulse::{PlaybackEvent, PulseOutput, Transport, play_controlled};

use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, ensure};
use symphonia::{
    core::{
        audio::{Audio, GenericAudioBufferRef},
        codecs::audio::{
            AudioDecoder,
            well_known::{CODEC_ID_AAC, CODEC_ID_ALAC},
        },
        formats::FormatReader,
        io::{MediaSourceStream, ReadOnlySource},
    },
    default::{
        codecs::{AacDecoder as SymphoniaAac, AlacDecoder as SymphoniaAlac},
        formats::IsoMp4Reader,
    },
};

use fragmented::FragmentedM4a;
pub use symphonia::core::io::MediaSource;

/// Context on an error from the audio stream rather than from the audio server,
/// so a caller can tell the two apart with `downcast_ref`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Undecodable {
    pub step: DecodeStep,
    /// Audio decoded before the failure: where in the track it happened, which
    /// the playback clock only approximates from behind the output buffer.
    pub decoded_ms: u64,
    /// The same, exactly, in sample frames.
    pub decoded_frames: u64,
}

/// Which part of decoding failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeStep {
    /// The MP4 around the audio could not be read.
    Container,
    /// An audio frame did not decode.
    Audio,
    /// The demuxer ran out of audio before the stream ended.
    EndedEarly,
}

impl std::fmt::Display for Undecodable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the audio stream did not decode ({:?} after {} ms)",
            self.step, self.decoded_ms
        )
    }
}

/// The PCM the decoder hands the output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub bits_per_sample: u8,
    pub channels: usize,
}

/// What the track was encoded as.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Codec {
    Alac,
    /// AAC-LC. Decoded to floating point, then to [`AAC_BITS`]-bit PCM.
    Aac,
}

/// AAC decodes to floating point; 24 bits holds it with room to spare.
pub const AAC_BITS: u8 = 24;

/// Decodes the first ALAC or AAC-LC audio track in a clear M4A/MP4 source.
pub struct Decoder {
    reader: IsoMp4Reader<'static>,
    decoder: Box<dyn AudioDecoder>,
    codec: Codec,
    track_id: u32,
    format: PcmFormat,
    samples: Vec<i32>,
    finished: bool,
    source_eof: Option<Arc<AtomicBool>>,
    frames_decoded: u64,
}

impl Decoder {
    /// Opens a clear MP4 container from a Symphonia media source.
    pub fn open(source: Box<dyn MediaSource>) -> Result<Self> {
        let stream = MediaSourceStream::new(source, Default::default());
        let reader = IsoMp4Reader::try_new(stream, Default::default()).context("reading MP4")?;
        let (track_id, params) = reader
            .tracks()
            .iter()
            .filter_map(|track| Some((track.id, track.codec_params.as_ref()?.audio()?)))
            .find(|(_, params)| matches!(params.codec, CODEC_ID_ALAC | CODEC_ID_AAC))
            .context("MP4 has no ALAC or AAC audio track")?;

        let (codec, decoder, bits_per_sample): (_, Box<dyn AudioDecoder>, _) =
            if params.codec == CODEC_ID_ALAC {
                let cookie = params
                    .extra_data
                    .as_deref()
                    .context("missing ALAC configuration")?;
                let decoder = SymphoniaAlac::try_new(params, &Default::default())
                    .context("initializing ALAC decoder")?;
                (Codec::Alac, Box::new(decoder), alac_bit_depth(cookie)?)
            } else {
                let decoder = SymphoniaAac::try_new(params, &Default::default())
                    .context("initializing AAC decoder")?;
                (Codec::Aac, Box::new(decoder), AAC_BITS)
            };
        let decoded = decoder.codec_params();
        let sample_rate = decoded
            .sample_rate
            .filter(|rate| *rate > 0)
            .context("missing sample rate")?;
        let channels = decoded
            .channels
            .as_ref()
            .context("missing channels")?
            .count();

        Ok(Self {
            reader,
            decoder,
            codec,
            track_id,
            format: PcmFormat {
                sample_rate,
                bits_per_sample,
                channels,
            },
            samples: Vec::new(),
            finished: false,
            source_eof: None,
            frames_decoded: 0,
        })
    }

    /// Opens a forward-only fragmented M4A stream with sized top-level boxes.
    /// A clean end requires the underlying reader to reach EOF between boxes.
    pub fn open_fragmented(source: impl Read + Send + Sync + 'static) -> Result<Self> {
        let eof = Arc::new(AtomicBool::new(false));
        let source = FragmentedM4a::new(source, Arc::clone(&eof));
        let mut decoder =
            Self::open(Box::new(ReadOnlySource::new(source))).context(Undecodable {
                step: DecodeStep::Container,
                decoded_ms: 0,
                decoded_frames: 0,
            })?;
        decoder.source_eof = Some(eof);
        Ok(decoder)
    }

    fn undecodable(&self, step: DecodeStep) -> Undecodable {
        Undecodable {
            step,
            decoded_ms: self.frames_decoded * 1000 / u64::from(self.format.sample_rate.max(1)),
            decoded_frames: self.frames_decoded,
        }
    }

    pub fn format(&self) -> PcmFormat {
        self.format
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub(crate) fn channels(&self) -> Option<&symphonia::core::audio::Channels> {
        self.decoder.codec_params().channels.as_ref()
    }

    /// Borrows one nonempty packet of frame-interleaved PCM in Symphonia channel order.
    /// Samples are sign-extended, right-aligned i32 values at the source bit depth.
    /// The decoder reuses its buffer and retains at most one packet of interleaved PCM.
    /// End of stream returns `Ok(None)`. A decode or read error ends the stream.
    pub fn next_block(&mut self) -> Result<Option<&[i32]>> {
        if self.finished {
            return Ok(None);
        }

        match self.decode_next() {
            Ok(true) => Ok(Some(&self.samples)),
            result => {
                self.finished = true;
                result.map(|_| None)
            }
        }
    }

    fn decode_next(&mut self) -> Result<bool> {
        loop {
            let container = self.undecodable(DecodeStep::Container);
            let Some(packet) = self
                .reader
                .next_packet()
                .context("reading MP4 packet")
                .context(container)?
            else {
                break;
            };
            if packet.track_id != self.track_id {
                continue;
            }

            let audio_failed = self.undecodable(DecodeStep::Audio);
            let decoded = self
                .decoder
                .decode(&packet)
                .context("decoding audio packet")
                .context(audio_failed)?;
            let frames = match (self.codec, decoded) {
                (Codec::Alac, GenericAudioBufferRef::S32(audio)) => {
                    audio.copy_to_vec_interleaved(&mut self.samples);
                    // Symphonia left-aligns ALAC samples in i32. Undo that shift
                    // without rescaling or converting through floating point.
                    let shift = i32::BITS - u32::from(self.format.bits_per_sample);
                    for sample in &mut self.samples {
                        *sample >>= shift;
                    }
                    audio.frames()
                }
                (Codec::Aac, GenericAudioBufferRef::F32(audio)) => {
                    let mut floats = Vec::new();
                    audio.copy_to_vec_interleaved(&mut floats);
                    self.samples.clear();
                    self.samples
                        .extend(floats.iter().map(|&sample| aac_sample(sample)));
                    audio.frames()
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "the decoder returned an unexpected sample format"
                    )
                    .context(audio_failed));
                }
            };
            if frames == 0 {
                continue;
            }
            self.frames_decoded += frames as u64;
            return Ok(true);
        }
        if let Some(eof) = &self.source_eof
            && !eof.load(Ordering::Relaxed)
        {
            return Err(anyhow::anyhow!(
                "MP4 demuxer stopped before a complete fragmented stream reached EOF"
            )
            .context(self.undecodable(DecodeStep::EndedEarly)));
        }
        Ok(false)
    }
}

/// A float sample as right-aligned [`AAC_BITS`]-bit PCM. AAC can overshoot
/// full scale between samples, so it is clipped rather than wrapped.
fn aac_sample(sample: f32) -> i32 {
    const FULL_SCALE: f32 = ((1 << (AAC_BITS - 1)) - 1) as f32;
    (sample.clamp(-1.0, 1.0) * FULL_SCALE).round() as i32
}

fn alac_bit_depth(mut cookie: &[u8]) -> Result<u8> {
    // Symphonia 0.6.1 leaves bits_per_sample unset for ALAC in MP4.
    // Its ALAC configuration reader accepts these optional QuickTime prefixes.
    for atom in [b"frma", b"alac"] {
        if cookie.get(4..8) == Some(atom) {
            cookie = cookie.get(12..).context("truncated ALAC configuration")?;
        }
    }
    let depth = *cookie.get(5).context("missing ALAC bit depth")?;
    ensure!(
        matches!(depth, 16 | 20 | 24 | 32),
        "unsupported ALAC bit depth {depth}"
    );
    Ok(depth)
}
