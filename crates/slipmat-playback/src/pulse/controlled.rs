// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use libpulse_binding::time::MicroSeconds;

use super::*;
use crate::{DecodeStep, Undecodable};

const POSITION_INTERVAL: Duration = Duration::from_millis(100);
const POLL_INTERVAL: MicroSeconds = MicroSeconds(10_000);

#[derive(Clone, Copy)]
pub enum Transport {
    Pause,
    Resume,
}

pub enum PlaybackEvent {
    Playing(u64),
    Paused(u64),
    Position(u64),
}

/// Plays one clear track, reporting the PulseAudio clock and accepting
/// transport commands through the final drain.
pub fn play_controlled(
    mut decoder: AlacDecoder,
    socket: &Path,
    start_paused: bool,
    commands: Receiver<Result<Transport>>,
    mut report: impl FnMut(PlaybackEvent) -> Result<()>,
) -> Result<()> {
    // The decoder marks its own errors with where they happened.
    let mut block = decoder
        .next_block()?
        .context("ALAC track is empty")
        .context(Undecodable {
            step: DecodeStep::EndedEarly,
            decoded_ms: 0,
            decoded_frames: 0,
        })?
        .to_vec();
    let mut output = PulseOutput::open_with_flags(
        &decoder,
        socket,
        FlagSet::START_CORKED | FlagSet::AUTO_TIMING_UPDATE | FlagSet::INTERPOLATE_TIMING,
    )
    .context("opening PulseAudio output")?;

    // Reading a fragmented packet can block on stdin. Keep transport and the
    // audio mainloop responsive with at most one waiting packet.
    let (send, packets) = mpsc::sync_channel(0);
    thread::Builder::new()
        .name("alac-decoder".into())
        .spawn(move || {
            loop {
                match decoder.next_block() {
                    Ok(Some(samples)) if send.send(Ok(samples.to_vec())).is_ok() => {}
                    Err(error) => {
                        let _ = send.send(Err(error));
                        break;
                    }
                    _ => break,
                }
            }
        })
        .context("starting ALAC decoder thread")?;

    let mut transport = Playback {
        commands,
        report: &mut report,
        paused: true,
        submitted_frames: 0,
        position_ms: 0,
        next_position: Instant::now() + POSITION_INTERVAL,
    };
    if start_paused {
        (transport.report)(PlaybackEvent::Paused(0))?;
    } else {
        transport.set_paused(&mut output, false)?;
    }

    output.validate_samples(&block)?;
    let mut offset = 0;
    loop {
        transport.service(&mut output)?;
        if !transport.paused {
            if offset == block.len() {
                match packets.try_recv() {
                    Ok(packet) => {
                        block = packet?;
                        output.validate_samples(&block)?;
                        offset = 0;
                    }
                    Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {}
                }
            }
            if offset < block.len() {
                let written = output.write_available(&block[offset..])?;
                offset += written;
                transport.submitted_frames += (written / output.format.channels) as u64;
                if written > 0 {
                    continue;
                }
            }
        }
        poll(&mut output, POLL_INTERVAL)?;
    }

    output.drain_with(|output| {
        transport.service(output)?;
        poll(output, POLL_INTERVAL)?;
        transport.service(output)?;
        Ok(!transport.paused)
    })?;
    let end = transport.duration_ms(&output);
    if end != transport.position_ms {
        (transport.report)(PlaybackEvent::Position(end))?;
    }
    Ok(())
}

struct Playback<'a, F> {
    commands: Receiver<Result<Transport>>,
    report: &'a mut F,
    paused: bool,
    submitted_frames: u64,
    position_ms: u64,
    next_position: Instant,
}

impl<F: FnMut(PlaybackEvent) -> Result<()>> Playback<'_, F> {
    fn service(&mut self, output: &mut PulseOutput) -> Result<()> {
        while let Ok(command) = self.commands.try_recv() {
            let paused = matches!(command?, Transport::Pause);
            if paused != self.paused {
                self.set_paused(output, paused)?;
            }
        }
        let now = Instant::now();
        if !self.paused && now >= self.next_position {
            self.next_position = now + POSITION_INTERVAL;
            let previous = self.position_ms;
            self.read_position(output)?;
            if self.position_ms != previous {
                (self.report)(PlaybackEvent::Position(self.position_ms))?;
            }
        }
        Ok(())
    }

    fn set_paused(&mut self, output: &mut PulseOutput, paused: bool) -> Result<()> {
        output.check_ready()?;
        let success = Rc::new(Cell::new(None));
        let result = Rc::clone(&success);
        let mut cork = output
            .stream
            .set_corked_state(paused, Some(Box::new(move |ok| result.set(Some(ok)))));
        while success.get().is_none() && cork.get_state() == operation::State::Running {
            if let Err(error) = output.pump() {
                if success.get().is_none() {
                    cork.cancel();
                }
                return Err(error).context("changing PulseAudio playback state");
            }
        }
        if success.get().is_none() {
            cork.cancel();
        }
        ensure!(
            success.get() == Some(true),
            "changing PulseAudio playback state failed: {}",
            output.context.errno()
        );

        self.paused = paused;
        if paused {
            self.read_position(output)?;
            (self.report)(PlaybackEvent::Paused(self.position_ms))
        } else {
            self.next_position = Instant::now() + POSITION_INTERVAL;
            (self.report)(PlaybackEvent::Playing(self.position_ms))
        }
    }

    fn read_position(&mut self, output: &PulseOutput) -> Result<()> {
        if let Some(time) = output
            .stream
            .get_time()
            .context("reading PulseAudio playback clock")?
        {
            self.position_ms = self
                .position_ms
                .max((time.0 / 1000).min(self.duration_ms(output)));
        }
        Ok(())
    }

    fn duration_ms(&self, output: &PulseOutput) -> u64 {
        self.submitted_frames * 1000 / u64::from(output.format.sample_rate)
    }
}

fn poll(output: &mut PulseOutput, timeout: MicroSeconds) -> Result<()> {
    output
        .mainloop
        .prepare(Some(timeout))
        .context("preparing PulseAudio poll")?;
    output.mainloop.poll().context("polling PulseAudio")?;
    output
        .mainloop
        .dispatch()
        .context("dispatching PulseAudio events")?;
    output.check_ready()
}
