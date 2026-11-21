// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    env,
    fs::File,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::mpsc::{self, Receiver},
    thread,
};

use anyhow::{Context, Result, anyhow};
use slipmat_playback::{
    AlacDecoder, PcmFormat, PlaybackEvent, PulseOutput, Transport, play_controlled,
};

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let socket = args.next().map(PathBuf::from);
    let flags = match (args.next().as_deref(), args.next().as_deref(), args.next()) {
        (None, None, None) => Some(None),
        (Some(flag), None, None) if flag == "--controlled" => Some(Some(false)),
        (Some(flag), Some(initial), None) if flag == "--controlled" && initial == "--paused" => {
            Some(Some(true))
        }
        _ => None,
    };
    let (Some(socket), Some(controlled)) = (socket.filter(|path| path.is_absolute()), flags) else {
        eprintln!("Usage: fairplay-alac-output <absolute-pulse-socket> [--controlled [--paused]]");
        return ExitCode::from(2);
    };

    match play(&socket, controlled) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fairplay-alac-output: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn play(socket: &Path, controlled: Option<bool>) -> Result<()> {
    let commands = controlled.map(|_| read_commands()).transpose()?;
    let decoder = AlacDecoder::open_fragmented(io::stdin())?;
    let format = decoder.format();
    let mut stdout = io::stdout().lock();
    if let (Some(start_paused), Some(commands)) = (controlled, commands) {
        play_controlled(decoder, socket, start_paused, commands, |event| {
            let (kind, position_ms) = match event {
                PlaybackEvent::Playing(position) => ("playing", position),
                PlaybackEvent::Paused(position) => ("paused", position),
                PlaybackEvent::Position(position) => ("position", position),
            };
            writeln!(
                stdout,
                "{{\"type\":\"{kind}\",\"position_ms\":{position_ms}}}"
            )?;
            stdout.flush()?;
            Ok(())
        })?;
    } else {
        play_uncontrolled(decoder, socket)?;
    }
    write_format(stdout, format)
}

fn read_commands() -> Result<Receiver<Result<Transport>>> {
    let pipe = File::open("/proc/self/fd/3").context("opening control pipe on fd 3")?;
    let (send, receive) = mpsc::sync_channel(16);
    thread::Builder::new()
        .name("alac-control".into())
        .spawn(move || {
            for line in BufReader::new(pipe).lines() {
                let command = line
                    .context("reading playback control pipe")
                    .and_then(|line| match line.as_str() {
                        "pause" => Ok(Transport::Pause),
                        "resume" => Ok(Transport::Resume),
                        _ => Err(anyhow!("unknown playback control command")),
                    });
                let failed = command.is_err();
                if send.send(command).is_err() || failed {
                    break;
                }
            }
        })
        .context("starting playback control thread")?;
    Ok(receive)
}

fn play_uncontrolled(mut decoder: AlacDecoder, socket: &Path) -> Result<()> {
    let first_block = decoder
        .next_block()?
        .context("ALAC track is empty")?
        .to_vec();
    let mut output = PulseOutput::open(&decoder, socket).context("opening PulseAudio output")?;
    output.write(&first_block)?;
    drop(first_block);

    while let Some(block) = decoder.next_block()? {
        output.write(block)?;
    }
    output.drain()
}

fn write_format(mut stdout: impl Write, format: PcmFormat) -> Result<()> {
    writeln!(
        stdout,
        "{{\"sample_rate\":{},\"bits_per_sample\":{},\"channels\":{}}}",
        format.sample_rate, format.bits_per_sample, format.channels
    )?;
    stdout.flush()?;
    Ok(())
}
