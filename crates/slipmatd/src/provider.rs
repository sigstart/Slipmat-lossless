// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The provider process: spawning it on its profile directory, the stdio
//! exchange, and killing its whole process group when it is dropped.

use std::fs;
use std::future::pending;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use fairplay_wire::{Catalogue, Reply as ProviderReply, Request as ProviderRequest};
use slipmat_core::auth::Reply;
use slipmat_core::auth_wire::{self as wire, Reader, Request};
use slipmat_core::player::protocol::Tokens;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout, timeout_at};

use crate::auth::PlaybackControl;
use crate::profile_store::Profile;

pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn validate_provider(path: &Path) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file()
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o6022 != 0
        || (metadata.uid() != slipmat_core::paths::effective_uid() && metadata.uid() != 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid FairPlay provider executable",
        ));
    }
    Ok(())
}

/// What the provider is launched to do with its profile directory.
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    SignIn,
    Restore,
}

impl Mode {
    fn arg(self) -> &'static str {
        match self {
            Self::SignIn => "sign-in",
            Self::Restore => "restore",
        }
    }
}

pub(crate) enum End {
    Reply(Reply),
    Deferred(Reply),
    Disconnected,
    SignedIn,
}

pub(crate) struct Provider {
    pub(crate) catalogue: Option<Tokens>,
    /// Dropped after the process group is killed, which removes the directory.
    pub(crate) profile: Option<Profile>,
    pub(crate) child: Child,
    pub(crate) input: ChildStdin,
    pub(crate) output: Reader<ChildStdout>,
    /// What the provider saw of the last track that would not decode.
    pub(crate) diagnosis: Option<Box<fairplay_wire::Diagnosis>>,
}

impl Provider {
    /// Without a profile the launcher gives the provider a fresh tmpfs, which
    /// can sign in but cannot be saved.
    pub(crate) fn spawn(path: &Path, profile: Option<Profile>, mode: Mode) -> io::Result<Self> {
        if profile.is_none() && matches!(mode, Mode::Restore) {
            return Err(io::Error::other("a restore needs a profile directory"));
        }
        validate_provider(path)?;
        let parent = std::process::id();
        let mut command = Command::new(path);
        command
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        if let Some(profile) = &profile {
            command.arg(&profile.path).arg(mode.arg());
        }
        // SAFETY: the callback only uses async-signal-safe Linux operations.
        // Recheck the parent after installing the signal to cover a fork race.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() as u32 != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                let limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::setrlimit(libc::RLIMIT_CORE, &limit) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(forward_native_log(stderr));
        }
        let missing_pipe = || io::Error::other("FairPlay provider pipe unavailable");
        let input = child.stdin.take().ok_or_else(missing_pipe)?;
        let output = Reader::new(child.stdout.take().ok_or_else(missing_pipe)?);
        Ok(Self {
            catalogue: None,
            profile,
            child,
            input,
            output,
            diagnosis: None,
        })
    }

    pub(crate) async fn stop(&mut self) -> io::Result<()> {
        let group = self.kill_group();
        self.child.kill().await.and(group)
    }

    fn kill_group(&self) -> io::Result<()> {
        let Some(pid) = self.child.id() else {
            return Ok(());
        };
        // SAFETY: the unreaped child owns this process group. Its audio
        // subprocess must inherit the group so cancellation reaches it too.
        if unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) async fn exchange(&mut self, request: ProviderRequest) -> io::Result<Option<Reply>> {
        wire::write(&mut self.input, request).await?;
        match self.output.frame::<ProviderReply>().await? {
            Some(ProviderReply::SignedIn { catalogue }) => {
                self.catalogue = Some(tokens(catalogue)?);
                Ok(Some(Reply::SignedIn))
            }
            Some(reply) => Ok(Some(reply.into())),
            None => Ok(None),
        }
    }

    /// Runs one track, played or checked, until it ends.
    pub(crate) async fn play_track(
        &mut self,
        request: ProviderRequest,
        mut controls: mpsc::UnboundedReceiver<PlaybackControl>,
        events: &mpsc::UnboundedSender<Reply>,
    ) -> Option<Reply> {
        self.diagnosis = None;
        self.send_control(request).await?;
        let mut stop_deadline = None;
        loop {
            let control = tokio::select! {
                biased;
                _ = async {
                    match stop_deadline {
                        Some(deadline) => sleep_until(deadline).await,
                        None => pending().await,
                    }
                } => return None,
                frame = self.output.frame::<ProviderReply>() => {
                    let mut frame = frame.ok()??;
                    // Kept here: the reply the queue sees carries the fault, not this.
                    if let ProviderReply::PlaybackFailed { diagnosis, .. } = &mut frame
                        && let Some(diagnosis) = diagnosis.take()
                    {
                        tracing::warn!(?diagnosis, "what the provider saw of the failing file");
                        self.diagnosis = Some(diagnosis);
                    }
                    match Reply::from(frame) {
                        reply if reply.is_playback_update() => {
                            let _ = events.send(reply);
                            continue;
                        }
                        reply @ (
                            Reply::Played { .. }
                            | Reply::Stopped
                            | Reply::LicenceDenied
                            | Reply::PlaybackFailed { .. }
                            | Reply::Failed
                        ) => return Some(reply),
                        _ => return None,
                    }
                }
                _ = events.closed(), if stop_deadline.is_none() => PlaybackControl::Stop,
                control = controls.recv(), if stop_deadline.is_none() => {
                    control.unwrap_or(PlaybackControl::Stop)
                }
            };
            let request = match control {
                PlaybackControl::Pause => ProviderRequest::Pause,
                PlaybackControl::Resume => ProviderRequest::Resume,
                PlaybackControl::Stop => {
                    stop_deadline = Some(Instant::now() + IO_TIMEOUT);
                    ProviderRequest::Stop
                }
            };
            self.send_control(request).await?;
        }
    }

    async fn send_control(&mut self, request: ProviderRequest) -> Option<()> {
        timeout(IO_TIMEOUT, wire::write(&mut self.input, request))
            .await
            .ok()?
            .ok()
    }

    pub(crate) async fn authenticate(
        &mut self,
        read: &mut Reader<OwnedReadHalf>,
        write: &mut OwnedWriteHalf,
        deadline: Instant,
    ) -> End {
        let mut request = ProviderRequest::Begin;
        loop {
            let beginning = matches!(request, ProviderRequest::Begin);
            let reply = tokio::select! {
                biased;
                _ = sleep_until(deadline) => return End::Reply(Reply::Expired),
                // A client waits for each reply. Data here indicates pipelining.
                incoming = read.frame::<Request>() => return match incoming {
                    Ok(None) => End::Disconnected,
                    _ => End::Reply(Reply::Failed),
                },
                reply = self.exchange(request) => match reply {
                    Ok(Some(reply)) => reply,
                    _ => Reply::Failed,
                },
            };
            match reply {
                Reply::SignedIn => return End::SignedIn,
                Reply::Expired
                | Reply::Rejected
                | Reply::Failed
                | Reply::VerificationUnsupported => {
                    return End::Reply(reply);
                }
                Reply::CredentialsRequired if beginning => {}
                Reply::VerificationRequired if !beginning => {}
                _ => return End::Reply(Reply::Failed),
            }

            if send_before(write, reply, deadline).await.is_err() {
                return End::Disconnected;
            }
            request = loop {
                let next = tokio::select! {
                    biased;
                    _ = sleep_until(deadline) => return End::Deferred(Reply::Expired),
                    _ = self.child.wait() => return End::Deferred(Reply::Failed),
                    next = read.frame() => next,
                };
                let response = match next {
                    Ok(Some(Request::Credentials { account, password }))
                        if reply == Reply::CredentialsRequired =>
                    {
                        break ProviderRequest::Credentials { account, password };
                    }
                    Ok(Some(Request::VerificationCode { code }))
                        if reply == Reply::VerificationRequired =>
                    {
                        break ProviderRequest::VerificationCode { code };
                    }
                    Ok(Some(Request::Status)) => reply,
                    Ok(Some(Request::Begin | Request::SignOut)) => Reply::Busy,
                    Ok(Some(_)) => Reply::Rejected,
                    Ok(None) => return End::Disconnected,
                    Err(_) => return End::Reply(Reply::Failed),
                };
                if send_before(write, response, deadline).await.is_err() {
                    return End::Disconnected;
                }
            };
        }
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.kill_group();
    }
}

/// Lines the DRM wrapper writes itself, by prefix: its lease, recovery and key
/// context reports, whose formats carry fixed words, numbers and catalogue ids.
/// Apple's own code shares the stream and is free text that has not been read,
/// so everything else is dropped rather than risk a credential in the log.
const NATIVE_LOG: [&str; 7] = [
    "[recovery] ",
    "[.] LEASE_END ",
    "[.] PLAYBACK_ERROR",
    "[!] refreshed context",
    "[hybris] wrapper state: ",
    "[hybris] key context ",
    "[guard] ",
];

/// Longer than any line the wrapper writes; a longer one is not one of them.
const NATIVE_LINE: u64 = 512;

/// Reads the provider's stderr to its end, so it can never fill and stall the
/// provider, logging only the wrapper's own lines.
async fn forward_native_log(stderr: ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut reader)
            .take(NATIVE_LINE)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.last() != Some(&b'\n') {
            // Too long: skip the rest of it rather than read it as new lines.
            let mut rest = Vec::new();
            if reader.read_until(b'\n', &mut rest).await.is_err() {
                return;
            }
            continue;
        }
        if let Some(line) = native_line(&String::from_utf8_lossy(&line)) {
            tracing::info!(target: "slipmatd::fairplay", "{line}");
        }
    }
}

fn native_line(line: &str) -> Option<String> {
    NATIVE_LOG
        .iter()
        .any(|prefix| line.starts_with(prefix))
        .then(|| line.chars().filter(|c| !c.is_control()).collect())
}

/// Only a catalogue the daemon can use counts as signed in.
fn tokens(catalogue: Catalogue) -> io::Result<Tokens> {
    let Catalogue {
        developer_token,
        music_user_token,
        storefront,
        authorized,
    } = catalogue;
    let usable = authorized
        && !developer_token.is_empty()
        && !music_user_token.is_empty()
        && storefront.len() == 2
        && storefront.bytes().all(|byte| byte.is_ascii_lowercase());
    if !usable {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid provider catalogue",
        ));
    }
    Ok(Tokens {
        developer_token,
        music_user_token: Some(music_user_token),
        storefront,
        authorized,
    })
}

pub(crate) async fn send(write: &mut OwnedWriteHalf, reply: Reply) -> io::Result<()> {
    timeout(IO_TIMEOUT, wire::write(write, reply)).await?
}

pub(crate) async fn send_before(
    write: &mut OwnedWriteHalf,
    reply: Reply,
    deadline: Instant,
) -> io::Result<()> {
    timeout_at(
        deadline.min(Instant::now() + IO_TIMEOUT),
        wire::write(write, reply),
    )
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_wrappers_own_lines_are_logged() {
        for line in [
            "[.] LEASE_END code=3084 — scheduling async recovery\n",
            "[recovery] STATE=Running — REFRESH_SUCCESS\n",
            "[hybris] wrapper state: RUNNING\n",
            "[!] refreshed context\n",
        ] {
            assert_eq!(native_line(line).as_deref(), Some(line.trim_end()));
        }
        for line in [
            "[SVPlaybackLease] token=secret-value\n",
            "Music-User-Token: secret-value\n",
            "[!] catched an exception: whatever Apple said\n",
            "",
        ] {
            assert_eq!(native_line(line), None, "{line}");
        }
        // A line cannot forge a second one in the log.
        assert_eq!(
            native_line("[recovery] a\r\x1b[2Kfake\n").as_deref(),
            Some("[recovery] a[2Kfake")
        );
    }
}
