// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Result, anyhow, bail};
use slipmat_core::auth::{Client, Reply};
use tokio::io::{Interest, unix::AsyncFd};
use tokio::signal::unix::{SignalKind, signal};

pub enum Command {
    SignIn,
    SignOut,
    Status,
}

impl Command {
    pub fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Option<Self>> {
        let command = match args.next().as_deref().and_then(OsStr::to_str) {
            Some("sign-in") => Self::SignIn,
            Some("sign-out") => Self::SignOut,
            Some("sign-in-status") => Self::Status,
            _ => return Ok(None),
        };
        if args.next().is_some() {
            bail!("Authentication commands take no arguments.");
        }
        Ok(Some(command))
    }
}

pub async fn run(command: Command) -> Result<()> {
    if !matches!(command, Command::SignIn) {
        return exchange(command).await;
    }
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
    eprintln!();
    bail!("Sign-in cancelled.");
}

async fn exchange(command: Command) -> Result<()> {
    let mut connection = Client::connect().await;
    if matches!(command, Command::SignIn) && connection.as_ref().is_err_and(endpoint_missing) {
        crate::link::wait_for_daemon().await?;
        connection = Client::connect().await;
    }
    let mut client = connection.map_err(|error| {
        if endpoint_missing(&error) {
            anyhow!("Authentication is unavailable. The local auth endpoint is not running.")
        } else {
            anyhow!("Could not connect to the authentication endpoint.")
        }
    })?;
    let reply = match command {
        Command::SignIn => sign_in(&mut client).await?,
        Command::SignOut => auth_reply(client.sign_out().await)?,
        Command::Status => auth_reply(client.status().await)?,
    };

    let message = match (command, reply) {
        (Command::SignIn, Reply::SignedIn) => "Signed in to an Apple Music session.",
        (Command::Status, Reply::SignedIn) => "An Apple Music session is active.",
        (Command::Status, Reply::SignedOut) => "No Apple Music session is active.",
        (Command::SignOut, Reply::SignedOut) => "The Apple Music session has ended.",
        (Command::SignIn, Reply::SignedOut) => bail!("Sign-in ended while signed out."),
        (_, Reply::Busy) => bail!("Authentication is busy. Try again shortly."),
        (_, Reply::Rejected) => bail!("The authentication request was rejected."),
        (_, Reply::VerificationUnsupported) => {
            bail!("Apple requested an unsupported verification method.")
        }
        (_, Reply::Expired) => bail!("The authentication request expired. Try again."),
        (_, Reply::Failed) => bail!("The authentication request failed."),
        (
            _,
            Reply::Playing
            | Reply::Paused
            | Reply::Position { .. }
            | Reply::Stopped
            | Reply::Played { .. }
            | Reply::LicenceDenied
            | Reply::PlaybackFailed { .. },
        ) => {
            bail!("Unexpected playback reply from the authentication endpoint.");
        }
        (_, Reply::SignedIn | Reply::CredentialsRequired | Reply::VerificationRequired) => {
            bail!("Unexpected reply from the authentication endpoint.")
        }
    };
    println!("{message}");
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

    let terminal = SignInTerminal::open()?;
    let account = terminal.prompt("Apple Account email: ", true).await?;
    let password = terminal.prompt("Apple password (hidden): ", false).await?;
    let mut reply = auth_reply(client.credentials(account, password).await)?;
    while matches!(reply, Reply::VerificationRequired) {
        let code = terminal.prompt("Apple verification code: ", true).await?;
        reply = auth_reply(client.verification_code(code).await)?;
    }
    Ok(reply)
}

fn auth_reply(reply: io::Result<Reply>) -> Result<Reply> {
    reply.map_err(|_| anyhow!("Communication with the authentication endpoint failed."))
}

struct SignInTerminal {
    tty: AsyncFd<File>,
    original: libc::termios,
}

impl SignInTerminal {
    fn open() -> Result<Self> {
        let tty_required = || anyhow!("Sign-in requires an interactive terminal.");
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
        Ok(Self { tty, original })
    }

    async fn prompt(&self, label: &str, echo: bool) -> Result<String> {
        let input_failed = || anyhow!("Could not read from the sign-in terminal.");
        let mut mode = self.original;
        if echo {
            mode.c_lflag |= libc::ECHO;
        } else {
            mode.c_lflag &= !(libc::ECHO | libc::ECHONL);
        }
        mode.c_lflag |= libc::ICANON | libc::ISIG;
        mode.c_iflag |= libc::ICRNL;
        mode.c_cc[libc::VINTR] = 3;
        // Keep the process running long enough to restore terminal settings.
        mode.c_cc[libc::VQUIT] = libc::_POSIX_VDISABLE;
        mode.c_cc[libc::VSUSP] = libc::_POSIX_VDISABLE;
        set_terminal(self.tty.as_raw_fd(), &mode)
            .map_err(|_| anyhow!("Could not set terminal echo for sign-in."))?;

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
        if !echo {
            tty.write_all(b"\n").map_err(|_| input_failed())?;
        }
        line.truncate(count);
        if line.pop() != Some(b'\n') {
            bail!("Sign-in input cancelled.");
        }
        if line.is_empty() {
            bail!("Sign-in input cannot be empty.");
        }
        String::from_utf8(line).map_err(|_| anyhow!("Sign-in input must be valid UTF-8."))
    }
}

impl Drop for SignInTerminal {
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
