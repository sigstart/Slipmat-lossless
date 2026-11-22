// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Pull-based PCM decoding and local audio output for clear ALAC in an MP4 container.

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
        codecs::audio::{AudioDecoder, well_known::CODEC_ID_ALAC},
        formats::FormatReader,
        io::{MediaSourceStream, ReadOnlySource},
    },
    default::{codecs::AlacDecoder as SymphoniaAlac, formats::IsoMp4Reader},
};

use fragmented::FragmentedM4a;
pub use symphonia::core::io::MediaSource;

/// Context on an error from the ALAC stream rather than from the audio server,
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
    /// An ALAC frame did not decode.
    Audio,
    /// The demuxer ran out of audio before the stream ended.
    EndedEarly,
}

impl std::fmt::Display for Undecodable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the ALAC stream did not decode ({:?} after {} ms)",
            self.step, self.decoded_ms
        )
    }
}

/// The source PCM format, before any output conversion.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub bits_per_sample: u8,
    pub channels: usize,
}

/// Decodes the first ALAC audio track in a clear M4A/MP4 source.
pub struct AlacDecoder {
    reader: IsoMp4Reader<'static>,
    decoder: SymphoniaAlac,
    track_id: u32,
    format: PcmFormat,
    samples: Vec<i32>,
    finished: bool,
    source_eof: Option<Arc<AtomicBool>>,
    frames_decoded: u64,
}

impl AlacDecoder {
    /// Opens a clear MP4 container from a Symphonia media source.
    pub fn open(source: Box<dyn MediaSource>) -> Result<Self> {
        let stream = MediaSourceStream::new(source, Default::default());
        let reader = IsoMp4Reader::try_new(stream, Default::default()).context("reading MP4")?;
        let (track_id, params) = reader
            .tracks()
            .iter()
            .filter_map(|track| Some((track.id, track.codec_params.as_ref()?.audio()?)))
            .find(|(_, params)| params.codec == CODEC_ID_ALAC)
            .context("MP4 has no ALAC audio track")?;

        let cookie = params
            .extra_data
            .as_deref()
            .context("missing ALAC configuration")?;
        let bits_per_sample = alac_bit_depth(cookie)?;
        let decoder = SymphoniaAlac::try_new(params, &Default::default())
            .context("initializing ALAC decoder")?;
        let decoded = decoder.codec_params();
        let sample_rate = decoded
            .sample_rate
            .filter(|rate| *rate > 0)
            .context("missing ALAC sample rate")?;
        let channels = decoded
            .channels
            .as_ref()
            .context("missing ALAC channels")?
            .count();

        Ok(Self {
            reader,
            decoder,
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
            let GenericAudioBufferRef::S32(audio) = self
                .decoder
                .decode(&packet)
                .context("decoding ALAC packet")
                .context(audio_failed)?
            else {
                return Err(
                    anyhow::anyhow!("ALAC decoder returned an unexpected sample format")
                        .context(audio_failed),
                );
            };
            if audio.frames() == 0 {
                continue;
            }

            self.frames_decoded += audio.frames() as u64;
            audio.copy_to_vec_interleaved(&mut self.samples);
            // Symphonia left-aligns ALAC samples in i32. Undo that shift without
            // rescaling or converting through floating point.
            let shift = i32::BITS - u32::from(self.format.bits_per_sample);
            for sample in &mut self.samples {
                *sample >>= shift;
            }
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
