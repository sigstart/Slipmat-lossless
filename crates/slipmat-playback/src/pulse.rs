// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{cell::Cell, path::Path, rc::Rc};

use anyhow::{Context, Result, bail, ensure};
use libpulse_binding::{
    channelmap::{Map, Position as Speaker},
    context::{Context as PulseContext, FlagSet as ContextFlags, State as ContextState},
    def::BufferAttr,
    mainloop::standard::{IterateResult, Mainloop},
    operation,
    sample::{Format, Spec},
    stream::{FlagSet, SeekMode, State, Stream},
};
use symphonia::core::audio::{Channels, Position};

use crate::{Decoder, PcmFormat};

mod controlled;
pub use controlled::{PlaybackEvent, Transport, play_controlled};

const CHUNK_FRAMES: usize = 1024;

/// A blocking playback stream on a local PulseAudio-compatible server.
pub struct PulseOutput {
    stream: Stream,
    context: PulseContext,
    mainloop: Mainloop,
    format: PcmFormat,
    channels: Channels,
    encoding: Encoding,
    bytes: Vec<u8>,
}

impl PulseOutput {
    /// Opens a stream for the decoder's sample rate, bit depth, and speaker order.
    /// Supply the absolute path to the server's Unix socket, such as
    /// `$XDG_RUNTIME_DIR/pulse/native`.
    pub fn open(decoder: &Decoder, socket: &Path) -> Result<Self> {
        Self::open_with_flags(decoder, socket, FlagSet::empty())
    }

    fn open_with_flags(decoder: &Decoder, socket: &Path, flags: FlagSet) -> Result<Self> {
        let server = unix_server(socket)?;
        let format = decoder.format();
        let encoding = Encoding::for_depth(format.bits_per_sample)?;
        let spec = Spec {
            format: encoding.format,
            rate: format.sample_rate,
            channels: u8::try_from(format.channels).context("invalid PCM channel count")?,
        };
        ensure!(spec.is_valid(), "invalid PulseAudio sample specification");
        let channels = decoder
            .channels()
            .cloned()
            .context("missing channel layout")?;
        let map = channel_map(&channels, &spec)?;

        let mut mainloop = Mainloop::new().context("creating PulseAudio mainloop")?;
        let mut context =
            PulseContext::new(&mainloop, "Slipmat").context("creating PulseAudio context")?;
        context
            .connect(Some(&server), ContextFlags::NOAUTOSPAWN, None)
            .context("connecting to PulseAudio")?;
        loop {
            match context.get_state() {
                ContextState::Ready => break,
                state if state.is_good() => iterate(&mut mainloop)?,
                state => bail!("PulseAudio context entered {state:?}: {}", context.errno()),
            }
        }

        let mut stream = Stream::new(&mut context, "Slipmat", &spec, Some(&map))
            .with_context(|| format!("creating PulseAudio stream: {}", context.errno()))?;
        let target = spec.rate.div_ceil(4) * spec.frame_size() as u32;
        let buffer = BufferAttr {
            maxlength: 2 * target,
            tlength: target,
            prebuf: spec.rate.div_ceil(100) * spec.frame_size() as u32,
            minreq: u32::MAX,
            fragsize: u32::MAX,
        };
        stream
            .connect_playback(None, Some(&buffer), flags, None, None)
            .context("opening PulseAudio playback stream")?;
        while stream.get_state() == State::Creating {
            ensure!(
                context.get_state() == ContextState::Ready,
                "PulseAudio disconnected while opening the stream: {}",
                context.errno()
            );
            iterate(&mut mainloop)?;
        }
        let output = Self {
            stream,
            context,
            mainloop,
            format,
            channels,
            encoding,
            bytes: Vec::with_capacity(CHUNK_FRAMES * spec.frame_size()),
        };
        output.check_ready()?;
        Ok(output)
    }

    /// Returns whether this stream can continue with the given decoder.
    pub fn is_compatible(&self, decoder: &Decoder) -> bool {
        self.format == decoder.format() && decoder.channels() == Some(&self.channels)
    }

    /// Writes whole interleaved frames of sign-extended, right-aligned PCM
    /// from a compatible decoder. Blocks when the server buffer is full.
    pub fn write(&mut self, mut samples: &[i32]) -> Result<()> {
        self.validate_samples(samples)?;
        self.check_ready()?;
        while !samples.is_empty() {
            let written = self.write_available(samples)?;
            if written == 0 {
                self.pump()?;
            }
            samples = &samples[written..];
        }
        Ok(())
    }

    fn validate_samples(&self, samples: &[i32]) -> Result<()> {
        let channels = self.format.channels;
        ensure!(
            samples.len().is_multiple_of(channels),
            "PCM block ends with an incomplete frame"
        );
        let shift = i32::BITS - u32::from(self.format.bits_per_sample);
        ensure!(
            samples
                .iter()
                .all(|&sample| (sample << shift) >> shift == sample),
            "PCM block has samples outside the source bit depth"
        );
        Ok(())
    }

    fn write_available(&mut self, samples: &[i32]) -> Result<usize> {
        let channels = self.format.channels;
        let writable = self.stream.writable_size().with_context(|| {
            format!("checking PulseAudio buffer space: {}", self.context.errno())
        })?;
        let frames = (writable / (channels * self.encoding.width))
            .min(CHUNK_FRAMES)
            .min(samples.len() / channels);
        let written = frames * channels;
        if written > 0 {
            self.encoding.pack(&samples[..written], &mut self.bytes);
            self.stream
                .write_copy(&self.bytes, 0, SeekMode::Relative)
                .context("writing PCM to PulseAudio")?;
        }
        Ok(written)
    }

    /// Waits for queued audio to play, then closes the stream.
    pub fn drain(mut self) -> Result<()> {
        self.drain_with(|output| {
            output.pump()?;
            Ok(true)
        })
    }

    fn drain_with(&mut self, mut service: impl FnMut(&mut Self) -> Result<bool>) -> Result<()> {
        self.check_ready()?;
        let success = Rc::new(Cell::new(None));
        let result = Rc::clone(&success);
        let mut drain = self
            .stream
            .drain(Some(Box::new(move |ok| result.set(Some(ok)))));
        loop {
            let playing = match service(self) {
                Ok(playing) => playing,
                Err(error) => {
                    if success.get().is_none() {
                        drain.cancel();
                    }
                    return Err(error).context("draining PulseAudio playback");
                }
            };
            match success.get() {
                Some(true) if playing => return Ok(()),
                Some(false) => break,
                None if drain.get_state() != operation::State::Running => {
                    drain.cancel();
                    break;
                }
                _ => {}
            }
        }
        bail!("PulseAudio drain failed: {}", self.context.errno())
    }

    fn pump(&mut self) -> Result<()> {
        iterate(&mut self.mainloop)?;
        self.check_ready()
    }

    fn check_ready(&self) -> Result<()> {
        let context = self.context.get_state();
        let stream = self.stream.get_state();
        ensure!(
            context == ContextState::Ready && stream == State::Ready,
            "PulseAudio playback unavailable (context {context:?}, stream {stream:?}): {}",
            self.context.errno()
        );
        Ok(())
    }
}

fn iterate(mainloop: &mut Mainloop) -> Result<()> {
    match mainloop.iterate(true) {
        IterateResult::Success(_) => Ok(()),
        IterateResult::Err(error) => Err(error).context("running PulseAudio mainloop"),
        IterateResult::Quit(code) => bail!("PulseAudio mainloop quit ({code:?})"),
    }
}

fn unix_server(socket: &Path) -> Result<String> {
    let path = socket
        .to_str()
        .context("PulseAudio socket path must be UTF-8")?;
    ensure!(
        socket.is_absolute()
            && !path
                .bytes()
                .any(|byte| byte == 0 || byte.is_ascii_whitespace()),
        "PulseAudio requires one absolute Unix socket path without whitespace"
    );
    Ok(format!("unix:{path}"))
}

fn channel_map(channels: &Channels, spec: &Spec) -> Result<Map> {
    let Channels::Positioned(positions) = channels else {
        bail!("unsupported channel layout");
    };
    let mut map = Map::default();
    map.set_len(spec.channels);
    // Symphonia emits positioned channels in ascending mask order.
    for (slot, position) in map.get_mut().iter_mut().zip(positions.iter()) {
        *slot = match position {
            Position::FRONT_LEFT => Speaker::FrontLeft,
            Position::FRONT_RIGHT => Speaker::FrontRight,
            Position::FRONT_CENTER if spec.channels == 1 => Speaker::Mono,
            Position::FRONT_CENTER => Speaker::FrontCenter,
            Position::LFE1 => Speaker::Lfe,
            Position::REAR_LEFT => Speaker::RearLeft,
            Position::REAR_RIGHT => Speaker::RearRight,
            Position::REAR_CENTER => Speaker::RearCenter,
            Position::FRONT_LEFT_CENTER => Speaker::FrontLeftOfCenter,
            Position::FRONT_RIGHT_CENTER => Speaker::FrontRightOfCenter,
            Position::SIDE_LEFT => Speaker::SideLeft,
            Position::SIDE_RIGHT => Speaker::SideRight,
            _ => bail!("unsupported speaker position {position:?}"),
        };
    }
    ensure!(
        map.is_compatible_with_sample_spec(spec),
        "invalid PulseAudio channel map"
    );
    Ok(map)
}

struct Encoding {
    format: Format,
    width: usize,
    shift: u32,
}

impl Encoding {
    fn for_depth(depth: u8) -> Result<Self> {
        let (format, width) = match depth {
            16 => (Format::S16le, 2),
            20 | 24 => (Format::S24le, 3),
            32 => (Format::S32le, 4),
            _ => bail!("unsupported PCM bit depth {depth}"),
        };
        Ok(Self {
            format,
            width,
            shift: (width * 8) as u32 - u32::from(depth),
        })
    }

    fn pack(&self, samples: &[i32], bytes: &mut Vec<u8>) {
        bytes.clear();
        for &sample in samples {
            bytes.extend_from_slice(&(sample << self.shift).to_le_bytes()[..self.width]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::audio::layouts;

    #[test]
    fn packs_full_precision_signed_samples_in_little_endian() {
        let mut bytes = Vec::new();
        for (depth, format, width, expected) in [
            (
                16,
                Format::S16le,
                2,
                &[0x00, 0x80, 0xff, 0xff, 0x01, 0x00, 0xff, 0x7f][..],
            ),
            (
                20,
                Format::S24le,
                3,
                &[
                    0x00, 0x00, 0x80, 0xf0, 0xff, 0xff, 0x10, 0x00, 0x00, 0xf0, 0xff, 0x7f,
                ],
            ),
            (
                24,
                Format::S24le,
                3,
                &[
                    0x00, 0x00, 0x80, 0xff, 0xff, 0xff, 0x01, 0x00, 0x00, 0xff, 0xff, 0x7f,
                ],
            ),
            (
                32,
                Format::S32le,
                4,
                &[
                    0x00, 0x00, 0x00, 0x80, 0xff, 0xff, 0xff, 0xff, 0x01, 0x00, 0x00, 0x00, 0xff,
                    0xff, 0xff, 0x7f,
                ],
            ),
        ] {
            let encoding = Encoding::for_depth(depth).unwrap();
            assert_eq!(encoding.format, format);
            assert_eq!(encoding.width, width);
            let max = i32::MAX >> (32 - depth);
            encoding.pack(&[!max, -1, 1, max], &mut bytes);
            assert_eq!(bytes, expected, "{depth}-bit PCM");
        }
    }

    #[test]
    fn uses_decoder_order_for_surround_channels() {
        let spec = Spec {
            format: Format::S24le,
            rate: 48_000,
            channels: 8,
        };
        let map = channel_map(&layouts::CHANNEL_LAYOUT_MPEG_7P1_B, &spec).unwrap();
        assert_eq!(
            map.get(),
            [
                Speaker::FrontLeft,
                Speaker::FrontRight,
                Speaker::FrontCenter,
                Speaker::Lfe,
                Speaker::RearLeft,
                Speaker::RearRight,
                Speaker::FrontLeftOfCenter,
                Speaker::FrontRightOfCenter,
            ],
        );
    }

    #[test]
    fn accepts_only_one_absolute_local_socket() {
        assert_eq!(
            unix_server(Path::new("/run/example/pulse")).unwrap(),
            "unix:/run/example/pulse"
        );
        for invalid in ["tcp:localhost", "/run/pulse tcp:localhost", "/run/\0pulse"] {
            assert!(unix_server(Path::new(invalid)).is_err(), "{invalid:?}");
        }
    }
}
