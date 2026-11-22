// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! One provider process's conversation with the daemon.
//!
//! `begin`, then sign-in or restore, then `signedIn` with the catalogue
//! credentials. After that the provider plays one track at a time until the
//! daemon closes the pipe, relaying `pause`, `resume` and `stop` to it.

use std::os::fd::OwnedFd;
use std::pin::pin;
use std::sync::Arc;

use anyhow::{Result, bail};
use fairplay_wire::{Reader, Reply, Request, Secret};
use tokio::net::unix::pipe;
use tokio::sync::mpsc;

use crate::catalogue;
use crate::fairplay::{Challenge, Credentials, Fairplay};
use crate::play::{self, Control, Player, Update};

#[derive(Clone, Copy)]
pub enum Mode {
    SignIn,
    Restore,
}

pub struct Wire {
    input: Reader<pipe::Receiver>,
    output: pipe::Sender,
}

impl Wire {
    pub fn open(input: OwnedFd, output: OwnedFd) -> Result<Self> {
        Ok(Self {
            input: Reader::new(pipe::Receiver::from_owned_fd(input)?),
            output: pipe::Sender::from_owned_fd(output)?,
        })
    }

    async fn read(&mut self) -> Result<Option<Request>> {
        Ok(self.input.frame().await?)
    }

    async fn send(&mut self, reply: Reply) -> Result<()> {
        Ok(fairplay_wire::write(&mut self.output, reply).await?)
    }
}

pub async fn run(wire: &mut Wire, mode: Mode, fairplay: Result<Fairplay>) -> Result<()> {
    let Some(Request::Begin) = wire.read().await? else {
        bail!("the daemon did not begin");
    };
    let Ok(fairplay) = fairplay else {
        wire.send(Reply::Failed).await?;
        bail!("the FairPlay libraries did not start");
    };
    let signed_in = match mode {
        Mode::SignIn => sign_in(wire, &fairplay).await?,
        Mode::Restore => start(wire, &fairplay, None).await?,
    };
    if !signed_in {
        bail!("not signed in");
    }
    let catalogue = match catalogue::credentials(&fairplay).await {
        Ok(catalogue) => catalogue,
        Err(err) => {
            wire.send(Reply::Failed).await?;
            return Err(err);
        }
    };
    let player = Player::new(Arc::new(fairplay), catalogue.clone());
    wire.send(Reply::SignedIn { catalogue }).await?;
    serve_tracks(wire, Arc::new(player?)).await
}

/// `false` when sign-in ended with a reply already sent.
async fn sign_in(wire: &mut Wire, fairplay: &Fairplay) -> Result<bool> {
    wire.send(Reply::CredentialsRequired).await?;
    let Some(Request::Credentials { account, password }) = wire.read().await? else {
        bail!("expected credentials");
    };
    if !is_credential(&account) || !is_credential(&password) {
        wire.send(Reply::Rejected).await?;
        return Ok(false);
    }
    start(wire, fairplay, Some(Credentials { account, password })).await
}

/// Runs the library's sign-in, relaying its prompts. Without credentials this
/// is a restore, and any prompt fails it: a restore never asks the user.
async fn start(
    wire: &mut Wire,
    fairplay: &Fairplay,
    credentials: Option<Credentials>,
) -> Result<bool> {
    let restoring = credentials.is_none();
    let (challenges, mut prompts) = mpsc::unbounded_channel();
    let mut started = pin!(fairplay.start_session(credentials, challenges));
    loop {
        tokio::select! {
            result = &mut started => {
                if result.is_ok() {
                    return Ok(true);
                }
                wire.send(Reply::Failed).await?;
                return Ok(false);
            }
            Some(prompt) = prompts.recv() => match prompt {
                _ if restoring => exit_after(wire, Reply::Failed).await,
                Challenge::Unsupported => exit_after(wire, Reply::VerificationUnsupported).await,
                Challenge::TwoFactor(answer) => {
                    wire.send(Reply::VerificationRequired).await?;
                    let code = match wire.read().await? {
                        Some(Request::VerificationCode { code }) if is_verification_code(&code) => {
                            Some(code)
                        }
                        _ => None,
                    };
                    let _ = answer.send(code);
                }
            },
        }
    }
}

/// The library is blocked inside a prompt that must not be answered, so the
/// process ends here rather than returning to it.
async fn exit_after(wire: &mut Wire, reply: Reply) -> ! {
    let _ = wire.send(reply).await;
    // SAFETY: ends the process without running native atexit handlers.
    unsafe { libc::_exit(1) }
}

/// The track in progress. Once stopping, its updates are dropped and how it
/// ended is reported as `stopped`.
struct Running {
    controls: mpsc::UnboundedSender<Control>,
    stopping: bool,
}

async fn serve_tracks(wire: &mut Wire, player: Arc<Player>) -> Result<()> {
    let (updates, mut track_updates) = mpsc::unbounded_channel();
    let mut track: Option<Running> = None;
    loop {
        tokio::select! {
            request = wire.read() => {
                let Some(request) = request? else {
                    return Ok(());
                };
                match (request, &mut track) {
                    (Request::PlayTrack { track_id }, None) => {
                        let controls = play::spawn(player.clone(), track_id, updates.clone());
                        track = Some(Running {
                            controls,
                            stopping: false,
                        });
                    }
                    (Request::Pause, Some(running)) if !running.stopping => {
                        let _ = running.controls.send(Control::Pause);
                    }
                    (Request::Resume, Some(running)) if !running.stopping => {
                        let _ = running.controls.send(Control::Resume);
                    }
                    (Request::Stop, Some(running)) => {
                        running.stopping = true;
                        let _ = running.controls.send(Control::Stop);
                    }
                    _ => {}
                }
            }
            Some(update) = track_updates.recv() => match update {
                Update::Playback(reply) => {
                    if track.as_ref().is_some_and(|running| !running.stopping) {
                        wire.send(reply).await?;
                    }
                }
                Update::Finished(reply) => {
                    let stopping = track.take().is_some_and(|running| running.stopping);
                    wire.send(if stopping { Reply::Stopped } else { reply }).await?;
                }
            },
        }
    }
}

fn is_credential(secret: &Secret) -> bool {
    !secret.expose().is_empty() && !secret.expose().contains('\0')
}

fn is_verification_code(code: &Secret) -> bool {
    code.expose().len() == 6 && code.expose().bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_and_codes_are_checked_before_the_library_sees_them() {
        assert!(is_credential(&Secret::from("a@example.invalid".to_owned())));
        assert!(!is_credential(&Secret::from(String::new())));
        assert!(!is_credential(&Secret::from("a\0b".to_owned())));
        assert!(is_verification_code(&Secret::from("123456".to_owned())));
        for code in ["12345", "1234567", "12345a", "١٢٣٤٥٦"] {
            assert!(!is_verification_code(&Secret::from(code.to_owned())));
        }
    }
}
