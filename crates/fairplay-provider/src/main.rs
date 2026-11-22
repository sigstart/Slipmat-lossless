// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The FairPlay provider. `slipmatd` runs it inside bubblewrap through the
//! bundle's `launch`, as `fairplay-provider sign-in|restore`, and talks to it
//! over stdio in `fairplay-wire` frames.
//!
//! Start-up order is the design:
//!
//! 1. Reserve stdout for replies and point fd 1 at `/dev/null`, because the
//!    native libraries print to stdout.
//! 2. Start the Android runtime while this is the only thread
//!    ([`fairplay::Fairplay::start`]).
//! 3. Only then start a current-thread tokio runtime for the pipe and HTTP.

mod catalogue;
mod cbcs;
mod fairplay;
mod ffi;
mod hls;
mod mp4;
mod play;
mod profile;
mod session;

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::ExitCode;

use session::Mode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mode = match (args.next().as_deref(), args.next()) {
        (Some("sign-in"), None) => Mode::SignIn,
        (Some("restore"), None) => Mode::Restore,
        _ => return ExitCode::from(2),
    };
    let Ok((input, output)) = reserve_stdio() else {
        return ExitCode::FAILURE;
    };
    let fairplay = fairplay::Fairplay::start();
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return ExitCode::FAILURE;
    };
    let finished = runtime.block_on(async {
        let mut wire = session::Wire::open(input, output)?;
        session::run(&mut wire, mode, fairplay).await
    });
    match finished {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// The daemon's pipes, moved off fds 0 and 1 so nothing native can use them.
fn reserve_stdio() -> io::Result<(OwnedFd, OwnedFd)> {
    let input = duplicate(0)?;
    let output = duplicate(1)?;
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    for fd in [0, 1] {
        // SAFETY: both descriptors are open; this only repoints fd 0 or 1.
        if unsafe { libc::dup2(null.as_raw_fd(), fd) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((input, output))
}

fn duplicate(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor or -1.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if copy == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `copy` is a fresh descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}
