// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Result, anyhow, bail};
use slipmat_core::native_auth::{Client, Reply, TrialStage};
use tokio::io::{Interest, unix::AsyncFd};
use tokio::signal::unix::{SignalKind, signal};

pub enum Command {
    SignIn,
    SignOut,
    Status,
    PlayTrack { track_id: String },
}

impl Command {
    pub fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Option<Self>> {
        let command = match args.next().as_deref().and_then(OsStr::to_str) {
            Some("sign-in") => Self::SignIn,
            Some("sign-out") => Self::SignOut,
            Some("sign-in-status") => Self::Status,
            Some("native-play") => {
                let track_id = args
                    .next()
                    .and_then(|arg| arg.into_string().ok())
                    .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
                    .filter(|_| args.next().is_none())
                    .ok_or_else(|| {
                        anyhow!("Usage: climat native-play <catalog-song-id> (one decimal ID).")
                    })?;
                return Ok(Some(Self::PlayTrack { track_id }));
            }
            _ => return Ok(None),
        };
        if args.next().is_some() {
            bail!("Native authentication commands take no arguments.");
        }
        Ok(Some(command))
    }
}

pub async fn run(command: Command) -> Result<()> {
    let signing_in = match command {
        Command::SignIn => true,
        Command::PlayTrack { .. } => false,
        Command::SignOut | Command::Status => return exchange(command).await,
    };
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    tokio::select! {
        biased;
        _ = interrupt.recv() => {},
        _ = terminate.recv() => {},
        _ = hangup.recv() => {},
        result = exchange(command) => return result,
    }
    if signing_in {
        eprintln!();
        bail!("Native sign-in cancelled.");
    }
    bail!("Native playback cancelled.");
}

async fn exchange(command: Command) -> Result<()> {
    let mut connection = Client::connect().await;
    if matches!(command, Command::SignIn) && connection.as_ref().is_err_and(endpoint_missing) {
        crate::link::wait_for_daemon().await?;
        connection = Client::connect().await;
    }
    let mut client = connection.map_err(|error| {
        if endpoint_missing(&error) {
            anyhow!("Native authentication is unavailable. The local auth endpoint is not running.")
        } else {
            anyhow!("Could not connect to the native authentication endpoint.")
        }
    })?;
    let reply = match command {
        Command::SignIn => sign_in(&mut client).await?,
        Command::SignOut => auth_reply(client.sign_out().await)?,
        Command::Status => auth_reply(client.status().await)?,
        Command::PlayTrack { track_id } => return play_track(&mut client, track_id).await,
    };

    let message = match (command, reply) {
        (Command::SignIn, Reply::SignedIn) => "Signed in to a native Apple Music session.",
        (Command::Status, Reply::SignedIn) => "A native Apple Music session is active.",
        (Command::Status, Reply::SignedOut) => "No native Apple Music session is active.",
        (Command::SignOut, Reply::SignedOut) => "The native Apple Music session has ended.",
        (Command::SignIn, Reply::SignedOut) => bail!("Native sign-in ended while signed out."),
        (_, Reply::Busy) => bail!("Native authentication is busy. Try again shortly."),
        (_, Reply::Rejected) => bail!("The native authentication request was rejected."),
        (_, Reply::VerificationUnsupported) => {
            bail!("Apple requested an unsupported verification method.")
        }
        (_, Reply::Expired) => bail!("The native authentication request expired. Try again."),
        (_, Reply::Failed) => bail!("The native authentication request failed."),
        (
            _,
            Reply::Playing
            | Reply::Paused
            | Reply::Position { .. }
            | Reply::Stopped
            | Reply::Played { .. }
            | Reply::LicenceDenied
            | Reply::TrialFailed { .. },
        ) => {
            bail!("Unexpected playback reply from the native authentication endpoint.");
        }
        (
            _,
            Reply::SignedIn
            | Reply::SignedOut
            | Reply::CredentialsRequired
            | Reply::VerificationRequired,
        ) => bail!("Unexpected reply from the native authentication endpoint."),
    };
    println!("{message}");
    Ok(())
}

async fn play_track(client: &mut Client, track_id: String) -> Result<()> {
    let reply = auth_reply(client.play_track(track_id).await)?;
    match reply {
        Reply::Played {
            sample_rate,
            bits_per_sample,
            channels,
        } => println!("Played at {sample_rate} Hz, {bits_per_sample}-bit, {channels} channels."),
        Reply::LicenceDenied => bail!("Apple Music denied the playback licence for this track."),
        Reply::TrialFailed { stage } => {
            let stage = match stage {
                TrialStage::Session => "session setup",
                TrialStage::Storefront => "Apple Music storefront lookup",
                TrialStage::Catalogue => "catalogue lookup",
                TrialStage::CatalogueAuthorization => {
                    bail!("Apple Music rejected catalogue authorization for the native session.")
                }
                TrialStage::CatalogueTrack => bail!(
                    "Apple Music could not find that song in your storefront. Check the song ID."
                ),
                TrialStage::CatalogueAsset => {
                    bail!("Apple Music returned no lossless asset for that song.")
                }
                TrialStage::Stream => "streaming",
                TrialStage::Output => "audio output",
            };
            bail!("Native playback failed during {stage}.");
        }
        Reply::SignedOut => bail!("No native Apple Music session is active. Run `climat sign-in`."),
        Reply::Busy => bail!("The native Apple Music session is busy. Try again shortly."),
        Reply::Expired => bail!("The native playback request expired. Try again."),
        Reply::Rejected => bail!("The native playback request was rejected."),
        Reply::Failed => bail!("Native playback failed."),
        Reply::CredentialsRequired
        | Reply::VerificationRequired
        | Reply::VerificationUnsupported
        | Reply::SignedIn
        | Reply::Playing
        | Reply::Paused
        | Reply::Position { .. }
        | Reply::Stopped => {
            bail!("Unexpected reply to the native playback request.");
        }
    }
    Ok(())
}

fn endpoint_missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

async fn sign_in(client: &mut Client) -> Result<Reply> {
    let reply = auth_reply(client.begin().await)?;
    if !matches!(reply, Reply::CredentialsRequired) {
        return Ok(reply);
    }

    let terminal = SecretTerminal::open()?;
    let account = terminal.prompt("Apple Account (hidden): ").await?;
    let password = terminal.prompt("Apple password (hidden): ").await?;
    let mut reply = auth_reply(client.credentials(account, password).await)?;
    while matches!(reply, Reply::VerificationRequired) {
        let code = terminal
            .prompt("Apple verification code (hidden): ")
            .await?;
        reply = auth_reply(client.verification_code(code).await)?;
    }
    Ok(reply)
}

fn auth_reply(reply: io::Result<Reply>) -> Result<Reply> {
    reply.map_err(|_| anyhow!("Communication with the native authentication endpoint failed."))
}

struct SecretTerminal {
    tty: AsyncFd<File>,
    original: libc::termios,
}

impl SecretTerminal {
    fn open() -> Result<Self> {
        let tty_required = || anyhow!("Native sign-in requires an interactive terminal.");
        if !io::stdin().is_terminal() {
            return Err(tty_required());
        }
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/tty")
            .map_err(|_| tty_required())?;
        let fd = tty.as_raw_fd();
        // SAFETY: fd is live and original is writable for tcgetattr.
        let mut original = std::mem::MaybeUninit::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } == -1 {
            return Err(tty_required());
        }
        // SAFETY: tcgetattr succeeded and initialized the termios.
        let original = unsafe { original.assume_init() };
        let tty = AsyncFd::new(tty).map_err(|_| tty_required())?;
        let mut hidden = original;
        hidden.c_lflag &= !(libc::ECHO | libc::ECHONL);
        hidden.c_lflag |= libc::ICANON | libc::ISIG;
        hidden.c_iflag |= libc::ICRNL;
        hidden.c_cc[libc::VINTR] = 3;
        // Keep the process running long enough to restore echo on exit.
        hidden.c_cc[libc::VQUIT] = libc::_POSIX_VDISABLE;
        hidden.c_cc[libc::VSUSP] = libc::_POSIX_VDISABLE;
        set_terminal(fd, &hidden)
            .map_err(|_| anyhow!("Could not disable terminal echo for native sign-in."))?;
        Ok(Self { tty, original })
    }

    async fn prompt(&self, label: &str) -> Result<String> {
        let input_failed = || anyhow!("Could not read from the native sign-in terminal.");
        // SAFETY: the terminal owns this live fd. Discard input typed before the prompt.
        if unsafe { libc::tcflush(self.tty.as_raw_fd(), libc::TCIFLUSH) } == -1 {
            return Err(input_failed());
        }
        let mut tty = self.tty.get_ref();
        tty.write_all(label.as_bytes())
            .map_err(|_| input_failed())?;

        // Linux's canonical terminal buffer holds one line of up to 4096 bytes.
        let mut line = vec![0; 4096];
        let count = self
            .tty
            .async_io(Interest::READABLE, |mut tty| tty.read(&mut line))
            .await
            .map_err(|_| input_failed())?;
        tty.write_all(b"\n").map_err(|_| input_failed())?;
        line.truncate(count);
        if line.pop() != Some(b'\n') {
            bail!("Native sign-in input cancelled.");
        }
        if line.is_empty() {
            bail!("Native sign-in input cannot be empty.");
        }
        String::from_utf8(line).map_err(|_| anyhow!("Native sign-in input must be valid UTF-8."))
    }
}

impl Drop for SecretTerminal {
    fn drop(&mut self) {
        let _ = set_terminal(self.tty.as_raw_fd(), &self.original);
    }
}

fn set_terminal(fd: std::os::fd::RawFd, attributes: &libc::termios) -> io::Result<()> {
    loop {
        // SAFETY: callers hold the live terminal fd and initialized attributes.
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, attributes) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
