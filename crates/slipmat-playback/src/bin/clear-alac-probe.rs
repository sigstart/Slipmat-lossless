// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    env,
    ffi::OsString,
    fs::{self, File},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, ensure};
use slipmat_playback::{Decoder, PulseOutput};

const USAGE: &str = "Usage: clear-alac-probe <track.m4a> [<track.m4a> ...]";

fn main() -> ExitCode {
    let mut paths = env::args_os().skip(1).map(PathBuf::from).peekable();
    let Some(first) = paths.next() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if first == Path::new("--help") && paths.peek().is_none() {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match play(first, paths) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("clear-alac-probe: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn play(first: PathBuf, mut paths: impl Iterator<Item = PathBuf>) -> Result<()> {
    let mut track = PreparedTrack::open(first)?;
    let socket = pulse_socket()?;
    let mut output = PulseOutput::open(&track.decoder, &socket)
        .with_context(|| format!("opening output at {}", socket.display()))?;
    eprintln!("Opened stream for {}", track.path.display());

    loop {
        output
            .write(&track.first_block)
            .with_context(|| format!("playing {}", track.path.display()))?;
        drop(std::mem::take(&mut track.first_block));
        // Prepare one successor while playback has queued PCM available.
        let next = paths.next().map(PreparedTrack::open).transpose();
        track.play_remaining(&mut output)?;

        let next = match next {
            Ok(Some(next)) => next,
            Ok(None) => return output.drain(),
            Err(error) => {
                if let Err(drain_error) = output.drain() {
                    return Err(error)
                        .context(format!("also failed to drain audio ({drain_error:#})"));
                }
                return Err(error);
            }
        };
        if output.is_compatible(&next.decoder) {
            eprintln!("Reusing stream for {}", next.path.display());
        } else {
            output.drain().context("draining before format switch")?;
            output = PulseOutput::open(&next.decoder, &socket)
                .with_context(|| format!("opening output at {}", socket.display()))?;
            eprintln!(
                "Opened stream after format switch for {}",
                next.path.display()
            );
        }
        track = next;
    }
}

struct PreparedTrack {
    path: PathBuf,
    decoder: Decoder,
    first_block: Vec<i32>,
}

impl PreparedTrack {
    fn open(path: PathBuf) -> Result<Self> {
        let (decoder, first_block) =
            Self::prepare(&path).with_context(|| format!("preparing {}", path.display()))?;
        Ok(Self {
            path,
            decoder,
            first_block,
        })
    }

    fn prepare(path: &Path) -> Result<(Decoder, Vec<i32>)> {
        ensure!(
            fs::metadata(path)?.is_file(),
            "expected a regular local file"
        );
        let file = File::open(path)?;
        ensure!(file.metadata()?.is_file(), "expected a regular local file");
        let mut decoder = Decoder::open(Box::new(file))?;
        let first_block = decoder
            .next_block()?
            .context("ALAC track is empty")?
            .to_vec();
        Ok((decoder, first_block))
    }

    fn play_remaining(&mut self, output: &mut PulseOutput) -> Result<()> {
        let context = || format!("playing {}", self.path.display());
        while let Some(block) = self.decoder.next_block().with_context(context)? {
            output.write(block).with_context(context)?;
        }
        Ok(())
    }
}

fn pulse_socket() -> Result<PathBuf> {
    if let Some(server) = env::var_os("PULSE_SERVER") {
        let server = server.to_str().context("PULSE_SERVER must be UTF-8")?;
        return Ok(PathBuf::from(
            server.strip_prefix("unix:").unwrap_or(server),
        ));
    }
    let runtime = env::var_os("PULSE_RUNTIME_PATH")
        .or_else(|| env::var_os("XDG_RUNTIME_DIR").map(pulse_runtime))
        .context("set PULSE_SERVER to a local Unix socket or set XDG_RUNTIME_DIR")?;
    Ok(PathBuf::from(runtime).join("native"))
}

fn pulse_runtime(runtime: OsString) -> OsString {
    PathBuf::from(runtime).join("pulse").into_os_string()
}
