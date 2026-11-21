// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native sign-in and playback. The provider owns the account session
//! and audio output. A manual trial and the queue share one playback slot.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions, Permissions};
use std::future::pending;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use slipmat_core::native_auth::Reply;
use slipmat_core::native_auth_wire::{self as wire, PlaybackRequest, Reader, Request};
use tokio::net::{
    UnixListener, UnixStream,
    unix::{OwnedReadHalf, OwnedWriteHalf},
};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, sleep_until, timeout, timeout_at};

const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 32;

pub struct Endpoint {
    task: JoinHandle<()>,
    service: Rc<Service>,
    _socket: SocketFile,
    _binding_lock: File,
}

impl Endpoint {
    pub async fn bind(provider: PathBuf) -> io::Result<Self> {
        let path = wire::socket_path()?;
        let binding_lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.with_extension("lock"))?;
        // SAFETY: the file owns a valid descriptor. Keep it open until after
        // removing the socket so standalone and full daemons cannot race.
        if unsafe { libc::flock(binding_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let listener = bind_listener(&path).await?;
        let metadata = fs::symlink_metadata(&path)?;
        let socket = SocketFile {
            path,
            inode: metadata.ino(),
            device: metadata.dev(),
        };
        fs::set_permissions(&socket.path, Permissions::from_mode(0o600))?;
        let storage = SessionStorage::for_provider(&provider, &socket.path);
        if let Some(storage) = &storage {
            storage.remove_runtime();
        }
        let service = Rc::new(Service {
            provider,
            storage,
            session: RefCell::new(Session::SignedOut),
            catalogue: RefCell::new(None),
            attempt_timeout: ATTEMPT_TIMEOUT,
        });
        let task = tokio::task::spawn_local(serve(listener, service.clone()));
        tracing::info!("native sign-in socket listening");
        Ok(Self {
            task,
            service,
            _socket: socket,
            _binding_lock: binding_lock,
        })
    }

    pub fn service(&self) -> Rc<Service> {
        self.service.clone()
    }
}

pub async fn run(provider: PathBuf) -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let _endpoint = Endpoint::bind(provider).await?;
    tokio::select! {
        _ = term.recv() => {}
        _ = interrupt.recv() => {}
    }
    Ok(())
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(storage) = &self.service.storage {
            storage.remove_runtime();
        }
    }
}

struct SocketFile {
    path: PathBuf,
    inode: u64,
    device: u64,
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|meta| meta.ino() == self.inode && meta.dev() == self.device)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

async fn bind_listener(path: &Path) -> io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => return Ok(listener),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {}
        Err(err) => return Err(err),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != wire::effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "native sign-in path is occupied",
        ));
    }
    // Require a refused connection before removing a leftover native socket.
    match timeout(IO_TIMEOUT, UnixStream::connect(path)).await? {
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
            fs::remove_file(path)?;
            UnixListener::bind(path)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "native sign-in socket is in use",
        )),
    }
}

async fn serve(listener: UnixListener, service: Rc<Service>) {
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut clients = JoinSet::new();
    if service
        .storage
        .as_ref()
        .is_some_and(|storage| storage.archive.is_file())
    {
        service.session.replace(Session::Busy);
        let restoring = service.clone();
        clients.spawn_local(async move { restoring.restore().await });
    }
    let mut reap = tokio::time::interval(Duration::from_secs(1));
    reap.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    tracing::warn!("native sign-in socket accept failed");
                    return;
                };
                if wire::verify_peer(&stream).is_err() {
                    continue;
                }
                let Ok(permit) = connections.clone().try_acquire_owned() else {
                    continue;
                };
                let service = service.clone();
                clients.spawn_local(async move {
                    let _permit = permit;
                    service.connection(stream).await;
                });
            }
            _ = clients.join_next(), if !clients.is_empty() => {}
            _ = reap.tick() => service.reap_provider(),
        }
    }
}

struct SessionStorage {
    archive: PathBuf,
    runtime: PathBuf,
}

impl SessionStorage {
    fn for_provider(provider: &Path, socket: &Path) -> Option<Self> {
        if !provider.with_file_name("profile-store").is_file() {
            return None;
        }
        let state = slipmat_core::paths::state_dir()?;
        fs::create_dir_all(&state).ok()?;
        Some(Self {
            archive: state.join("native-session.enc"),
            runtime: socket.with_file_name("slipmat-native-profile"),
        })
    }

    fn remove_runtime(&self) {
        let _ = fs::remove_dir_all(&self.runtime);
    }

    fn forget(&self) -> io::Result<()> {
        match fs::remove_file(&self.archive) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

pub struct Service {
    provider: PathBuf,
    storage: Option<SessionStorage>,
    session: RefCell<Session>,
    catalogue: RefCell<Option<slipmat_core::player::protocol::Tokens>>,
    attempt_timeout: Duration,
}

pub enum PlaybackControl {
    Pause,
    Resume,
    Stop,
}

enum Session {
    SignedOut,
    Busy,
    SignedIn(Box<Provider>),
    Playing {
        trial: watch::Sender<Trial>,
        queue: bool,
    },
}

#[derive(Clone, Copy)]
enum Trial {
    Active,
    Cancelled,
    Stopped(Reply),
}

impl Service {
    pub fn catalogue(&self) -> Option<slipmat_core::player::protocol::Tokens> {
        if self.is_signed_in() {
            self.catalogue.borrow().clone()
        } else {
            self.catalogue.borrow_mut().take();
            None
        }
    }

    async fn restore(&self) {
        let reservation = Reservation(&self.session);
        if let Ok(mut provider) =
            Provider::spawn_with_storage(&self.provider, self.storage.as_ref(), "restore")
            && matches!(
                timeout(Duration::from_secs(75), provider.exchange(Request::Begin)).await,
                Ok(Ok(Some(Reply::SignedIn)))
            )
            && matches!(provider.child.try_wait(), Ok(None))
        {
            self.catalogue.replace(provider.catalogue.take());
            self.session.replace(Session::SignedIn(Box::new(provider)));
            tracing::info!("restored native Apple Music session");
            return;
        }
        drop(reservation);
        tracing::info!("saved native session could not be restored");
    }

    fn reap_provider(&self) {
        let mut session = self.session.borrow_mut();
        if let Session::SignedIn(provider) = &mut *session
            && !matches!(provider.child.try_wait(), Ok(None))
        {
            *session = Session::SignedOut;
        }
    }

    fn status(&self) -> Reply {
        self.reap_provider();
        match &*self.session.borrow() {
            Session::SignedOut => Reply::SignedOut,
            Session::Busy => Reply::Busy,
            Session::Playing { trial, queue: true } if matches!(*trial.borrow(), Trial::Active) => {
                Reply::SignedIn
            }
            Session::Playing { .. } => Reply::Busy,
            Session::SignedIn(_) => Reply::SignedIn,
        }
    }

    pub fn is_authenticating(&self) -> bool {
        matches!(*self.session.borrow(), Session::Busy)
    }

    pub fn is_signed_in(&self) -> bool {
        self.reap_provider();
        match &*self.session.borrow() {
            Session::SignedIn(_) => true,
            Session::Playing { trial, .. } => matches!(*trial.borrow(), Trial::Active),
            _ => false,
        }
    }

    pub(crate) async fn sign_out(&self) -> Reply {
        self.reap_provider();
        if matches!(*self.session.borrow(), Session::Busy) {
            return Reply::Busy;
        }
        if self
            .storage
            .as_ref()
            .is_some_and(|storage| storage.forget().is_err())
        {
            return Reply::Failed;
        }
        let trial = match &*self.session.borrow() {
            Session::Playing { trial, .. } => {
                let updates = trial.subscribe();
                trial.send_replace(Trial::Cancelled);
                Some(updates)
            }
            _ => None,
        };
        if let Some(mut trial) = trial {
            return match trial
                .wait_for(|state| matches!(state, Trial::Stopped(_)))
                .await
                .as_deref()
            {
                Ok(Trial::Stopped(reply)) => *reply,
                _ => Reply::Failed,
            };
        }
        let mut provider = {
            let mut session = self.session.borrow_mut();
            match *session {
                Session::SignedOut => return Reply::SignedOut,
                Session::Busy | Session::Playing { .. } => return Reply::Busy,
                Session::SignedIn(_) => std::mem::replace(&mut *session, Session::Busy),
            }
        };
        let reservation = Reservation(&self.session);
        let result = if let Session::SignedIn(provider) = &mut provider {
            provider.stop().await
        } else {
            return Reply::Failed;
        };
        drop(reservation);
        if result.is_ok() {
            Reply::SignedOut
        } else {
            Reply::Failed
        }
    }

    async fn connection(self: &Rc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut read = Reader::new(read);
        let mut deferred = None;
        loop {
            let reply = match read.frame().await {
                Ok(Some(Request::Status)) => self.status(),
                Ok(Some(Request::SignOut)) => self.sign_out().await,
                Ok(Some(Request::PlayTrack { track_id })) => {
                    if self
                        .play_track(track_id, &mut read, &mut write)
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                Ok(Some(Request::Begin)) => {
                    deferred = None;
                    match self.begin(&mut read, &mut write).await {
                        End::Reply(reply) => reply,
                        End::Deferred(reply) => {
                            deferred = Some(reply);
                            continue;
                        }
                        End::Disconnected => return,
                        End::SignedIn => continue,
                    }
                }
                Ok(Some(Request::Credentials { .. } | Request::VerificationCode { .. })) => {
                    deferred.take().unwrap_or(Reply::Rejected)
                }
                Ok(None) => return,
                Err(_) => {
                    let _ = send(&mut write, Reply::Failed).await;
                    return;
                }
            };
            if send(&mut write, reply).await.is_err() {
                return;
            }
        }
    }

    async fn begin(&self, read: &mut Reader<OwnedReadHalf>, write: &mut OwnedWriteHalf) -> End {
        match self.status() {
            Reply::SignedOut => {}
            reply => return End::Reply(reply),
        }
        *self.session.borrow_mut() = Session::Busy;
        let reservation = Reservation(&self.session);
        let deadline = Instant::now() + self.attempt_timeout;
        let Ok(mut provider) =
            Provider::spawn_with_storage(&self.provider, self.storage.as_ref(), "seed")
        else {
            return End::Reply(Reply::Failed);
        };
        let mut end = provider.authenticate(read, write, deadline).await;
        if matches!(end, End::SignedIn) {
            // Keep the child only after delivering success to the owner.
            end = if !matches!(provider.child.try_wait(), Ok(None)) {
                End::Reply(Reply::Failed)
            } else if send_before(write, Reply::SignedIn, deadline).await.is_err() {
                End::Disconnected
            } else {
                self.catalogue.replace(provider.catalogue.take());
                *self.session.borrow_mut() = Session::SignedIn(Box::new(provider));
                return End::SignedIn;
            };
        }
        let _ = provider.stop().await;
        drop(reservation);
        end
    }

    async fn play_track(
        self: &Rc<Self>,
        track_id: String,
        read: &mut Reader<OwnedReadHalf>,
        write: &mut OwnedWriteHalf,
    ) -> io::Result<()> {
        let mut events = match self.start_playback(track_id, None) {
            Ok(events) => events,
            Err(reply) => return send(write, reply).await,
        };
        loop {
            let reply = tokio::select! {
                biased;
                incoming = read.frame::<Request>() => return match incoming {
                    Ok(None) => Err(io::ErrorKind::BrokenPipe.into()),
                    _ => send(write, Reply::Failed).await,
                },
                reply = events.recv() => reply.unwrap_or(Reply::Failed),
            };
            if !reply.is_playback_update() {
                return send(write, reply).await;
            }
        }
    }

    /// Claim the native session now and drive one queue track on the LocalSet.
    /// Dropping either channel requests a stop.
    pub fn start_queue(
        self: &Rc<Self>,
        track_id: String,
        controls: mpsc::UnboundedReceiver<PlaybackControl>,
    ) -> Result<mpsc::UnboundedReceiver<Reply>, Reply> {
        self.start_playback(track_id, Some(controls))
    }

    fn start_playback(
        self: &Rc<Self>,
        track_id: String,
        controls: Option<mpsc::UnboundedReceiver<PlaybackControl>>,
    ) -> Result<mpsc::UnboundedReceiver<Reply>, Reply> {
        if track_id.is_empty() || !track_id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Reply::Failed);
        }
        self.reap_provider();
        let (trial, mut updates) = watch::channel(Trial::Active);
        let playing = Session::Playing {
            trial: trial.clone(),
            queue: controls.is_some(),
        };
        let mut provider = {
            let mut session = self.session.borrow_mut();
            match *session {
                Session::SignedIn(_) => {}
                Session::SignedOut => return Err(Reply::SignedOut),
                Session::Busy | Session::Playing { .. } => return Err(Reply::Busy),
            }
            let Session::SignedIn(provider) = std::mem::replace(&mut *session, playing) else {
                unreachable!("idle provider was checked under the same mutable borrow");
            };
            provider
        };
        let (events, receiver) = mpsc::unbounded_channel();
        let service = self.clone();
        tokio::task::spawn_local(async move {
            let reservation = Reservation(&service.session);
            let result = tokio::select! {
                biased;
                _ = updates.changed() => None,
                result = provider.play_track(track_id, controls, &events) => result,
            };
            let reply = if let Some(reply) = result {
                service.session.replace(Session::SignedIn(provider));
                reply
            } else {
                let signed_out = if provider.stop().await.is_ok() {
                    Reply::SignedOut
                } else {
                    Reply::Failed
                };
                drop(reservation);
                let signing_out = matches!(*trial.borrow(), Trial::Cancelled);
                trial.send_replace(Trial::Stopped(signed_out));
                if signing_out {
                    signed_out
                } else {
                    Reply::Failed
                }
            };
            let _ = events.send(reply);
        });
        Ok(receiver)
    }
}

struct Reservation<'a>(&'a RefCell<Session>);

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut session = self.0.borrow_mut();
        if matches!(*session, Session::Busy | Session::Playing { .. }) {
            *session = Session::SignedOut;
        }
    }
}

enum End {
    Reply(Reply),
    Deferred(Reply),
    Disconnected,
    SignedIn,
}

struct Provider {
    catalogue: Option<slipmat_core::player::protocol::Tokens>,
    runtime_profile: Option<PathBuf>,
    child: Child,
    input: ChildStdin,
    output: Reader<ChildStdout>,
}

impl Provider {
    fn spawn_with_storage(
        path: &Path,
        storage: Option<&SessionStorage>,
        mode: &str,
    ) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file()
            || metadata.mode() & 0o111 == 0
            || metadata.mode() & 0o6022 != 0
            || (metadata.uid() != wire::effective_uid() && metadata.uid() != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid native sign-in provider executable",
            ));
        }
        let parent = std::process::id();
        let mut command = Command::new(path);
        command
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        if let Some(storage) = storage {
            command
                .arg("--session-profile")
                .arg(&storage.runtime)
                .arg(&storage.archive)
                .arg(mode);
            for name in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
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
        let missing_pipe = || io::Error::other("native sign-in provider pipe unavailable");
        let input = child.stdin.take().ok_or_else(missing_pipe)?;
        let output = Reader::new(child.stdout.take().ok_or_else(missing_pipe)?);
        Ok(Self {
            catalogue: None,
            runtime_profile: storage.map(|storage| storage.runtime.clone()),
            child,
            input,
            output,
        })
    }

    async fn stop(&mut self) -> io::Result<()> {
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

    async fn exchange(&mut self, request: Request) -> io::Result<Option<Reply>> {
        wire::write(&mut self.input, request).await?;
        match self.output.frame::<wire::ProviderReply>().await? {
            Some(wire::ProviderReply::Catalogue { kind, catalogue })
                if kind == "signedIn"
                    && catalogue.authorized
                    && !catalogue.developer_token.is_empty()
                    && catalogue
                        .music_user_token
                        .as_ref()
                        .is_some_and(|token| !token.is_empty())
                    && catalogue.storefront.len() == 2
                    && catalogue
                        .storefront
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase()) =>
            {
                self.catalogue = Some(catalogue);
                Ok(Some(Reply::SignedIn))
            }
            Some(wire::ProviderReply::Plain(reply)) => Ok(Some(reply)),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid provider reply",
            )),
            None => Ok(None),
        }
    }

    async fn play_track(
        &mut self,
        track_id: String,
        mut controls: Option<mpsc::UnboundedReceiver<PlaybackControl>>,
        events: &mpsc::UnboundedSender<Reply>,
    ) -> Option<Reply> {
        self.send_control(PlaybackRequest::PlayTrack { track_id })
            .await?;
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
                frame = self.output.frame::<Reply>() => {
                    match frame.ok()?? {
                        reply if reply.is_playback_update() => {
                            let _ = events.send(reply);
                            continue;
                        }
                        reply @ (
                            Reply::Played { .. }
                            | Reply::Stopped
                            | Reply::LicenceDenied
                            | Reply::TrialFailed { .. }
                            | Reply::Failed
                        ) => return Some(reply),
                        _ => return None,
                    }
                }
                _ = events.closed(), if stop_deadline.is_none() => {
                    controls.as_ref()?;
                    PlaybackControl::Stop
                }
                control = async {
                    match &mut controls {
                        Some(controls) => controls.recv().await,
                        None => pending().await,
                    }
                }, if stop_deadline.is_none() => control.unwrap_or(PlaybackControl::Stop),
            };
            let request = match control {
                PlaybackControl::Pause => PlaybackRequest::Pause,
                PlaybackControl::Resume => PlaybackRequest::Resume,
                PlaybackControl::Stop => {
                    stop_deadline = Some(Instant::now() + IO_TIMEOUT);
                    PlaybackRequest::Stop
                }
            };
            self.send_control(request).await?;
        }
    }

    async fn send_control(&mut self, request: PlaybackRequest) -> Option<()> {
        timeout(IO_TIMEOUT, wire::write(&mut self.input, request))
            .await
            .ok()?
            .ok()
    }

    async fn authenticate(
        &mut self,
        read: &mut Reader<OwnedReadHalf>,
        write: &mut OwnedWriteHalf,
        deadline: Instant,
    ) -> End {
        let mut request = Request::Begin;
        loop {
            let beginning = matches!(request, Request::Begin);
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
                    Ok(Some(request @ Request::Credentials { .. }))
                        if reply == Reply::CredentialsRequired =>
                    {
                        break request;
                    }
                    Ok(Some(request @ Request::VerificationCode { .. }))
                        if reply == Reply::VerificationRequired =>
                    {
                        break request;
                    }
                    Ok(Some(Request::Status)) => reply,
                    Ok(Some(Request::Begin | Request::SignOut | Request::PlayTrack { .. })) => {
                        Reply::Busy
                    }
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
        if let Some(profile) = &self.runtime_profile {
            let _ = fs::remove_dir_all(profile);
        }
    }
}

async fn send(write: &mut OwnedWriteHalf, reply: Reply) -> io::Result<()> {
    timeout(IO_TIMEOUT, wire::write(write, reply)).await?
}

async fn send_before(
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
    use slipmat_core::native_auth::TrialStage;

    struct TestClient {
        write: OwnedWriteHalf,
        read: Reader<OwnedReadHalf>,
    }

    impl TestClient {
        fn connect(service: &Rc<Service>) -> Self {
            let (client, server) = UnixStream::pair().unwrap();
            let service = Rc::clone(service);
            tokio::task::spawn_local(async move { service.connection(server).await });
            let (read, write) = client.into_split();
            Self {
                write,
                read: Reader::new(read),
            }
        }

        async fn exchange(&mut self, request: Request) -> Reply {
            timeout(IO_TIMEOUT, async {
                wire::write(&mut self.write, request).await.unwrap();
                self.read.frame().await.unwrap().unwrap()
            })
            .await
            .unwrap()
        }
    }

    fn service(attempt_timeout: Duration) -> Rc<Service> {
        Rc::new(Service {
            provider: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/native-auth-provider"),
            storage: None,
            session: RefCell::new(Session::SignedOut),
            catalogue: RefCell::new(None),
            attempt_timeout,
        })
    }

    fn credentials(account: &str) -> Request {
        Request::Credentials {
            account: account.to_owned().into(),
            password: "fixture-password".to_owned().into(),
        }
    }

    fn play(track_id: &str) -> Request {
        Request::PlayTrack {
            track_id: track_id.to_owned(),
        }
    }

    async fn sign_in(client: &mut TestClient) {
        assert_eq!(
            client.exchange(Request::Begin).await,
            Reply::CredentialsRequired
        );
        assert_eq!(
            client.exchange(credentials("test@example.invalid")).await,
            Reply::VerificationRequired
        );
        assert_eq!(
            client
                .exchange(Request::VerificationCode {
                    code: "123456".to_owned().into(),
                })
                .await,
            Reply::SignedIn
        );
    }

    #[tokio::test]
    async fn queue_controls_release_the_slot_and_preserve_the_provider_on_stop() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let service = service(ATTEMPT_TIMEOUT);
                let mut worker = Command::new("/bin/sh")
                    .arg("-ec")
                    .arg(
                        r#"
                        next() {
                            IFS= read -r frame
                            [ "$frame" = "$1" ]
                        }
                        next '{"type":"playTrack","track_id":"123"}'
                        /bin/sleep 600 &
                        audio=$!
                        printf '%s\n' '{"type":"playing"}' '{"type":"position","position_ms":42}'
                        next '{"type":"pause"}'
                        printf '%s\n' '{"type":"paused"}'
                        next '{"type":"resume"}'
                        printf '%s\n' '{"type":"playing"}'
                        next '{"type":"stop"}'
                        kill "$audio"
                        wait "$audio" || :
                        printf '%s\n' '{"type":"stopped"}'
                        next '{"type":"playTrack","track_id":"456"}'
                        printf '%s\n' '{"type":"playing"}' '{"type":"played","sample_rate":44100,"bits_per_sample":24,"channels":2}'
                        next '{"type":"playTrack","track_id":"789"}'
                        printf '%s\n' '{"type":"signedIn"}'
                        IFS= read -r frame
                        "#,
                    )
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .process_group(0)
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap();
                let provider_pid = worker.id().unwrap();
                let provider = Provider {
                    catalogue: None,
                    runtime_profile: None,
                    input: worker.stdin.take().unwrap(),
                    output: Reader::new(worker.stdout.take().unwrap()),
                    child: worker,
                };
                assert!(!service.is_signed_in());
                let (controls, input) = mpsc::unbounded_channel();
                assert!(matches!(
                    service.start_queue("123".to_owned(), input),
                    Err(Reply::SignedOut)
                ));
                drop(controls);
                service.session.replace(Session::SignedIn(Box::new(provider)));
                timeout(IO_TIMEOUT, async {
                    let (controls, input) = mpsc::unbounded_channel();
                    let mut events = service.start_queue("123".to_owned(), input).unwrap();
                    assert!(service.is_signed_in());
                    let (_, other_input) = mpsc::unbounded_channel();
                    assert!(matches!(
                        service.start_queue("456".to_owned(), other_input),
                        Err(Reply::Busy)
                    ));
                    assert_eq!(events.recv().await, Some(Reply::Playing));
                    assert_eq!(events.recv().await, Some(Reply::Position { position_ms: 42 }));
                    controls.send(PlaybackControl::Pause).unwrap();
                    assert_eq!(events.recv().await, Some(Reply::Paused));
                    assert!(service.is_signed_in());
                    controls.send(PlaybackControl::Resume).unwrap();
                    assert_eq!(events.recv().await, Some(Reply::Playing));
                    controls.send(PlaybackControl::Stop).unwrap();
                    assert_eq!(events.recv().await, Some(Reply::Stopped));
                    assert_eq!(events.recv().await, None);
                    assert_eq!(service.status(), Reply::SignedIn);

                    let mut manual = TestClient::connect(&service);
                    assert_eq!(
                        manual.exchange(play("456")).await,
                        Reply::Played {
                            sample_rate: 44100,
                            bits_per_sample: 24,
                            channels: 2,
                        }
                    );
                    assert_eq!(service.status(), Reply::SignedIn);

                    let (_controls, input) = mpsc::unbounded_channel();
                    let mut events = service.start_queue("789".to_owned(), input).unwrap();
                    assert_eq!(events.recv().await, Some(Reply::Failed));
                    assert!(!service.is_signed_in());
                    assert!(!Path::new(&format!("/proc/{provider_pid}")).exists());
                })
                .await
                .unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn signed_in_provider_handles_trials_and_reports_terminal_status() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let service = service(ATTEMPT_TIMEOUT);
                let mut sign_in_client = TestClient::connect(&service);
                let mut client = TestClient::connect(&service);
                assert_eq!(client.exchange(play("123")).await, Reply::SignedOut);
                sign_in(&mut sign_in_client).await;
                drop(sign_in_client);

                for id in ["", "i.123", "+123", " 123", "１２３"] {
                    assert_eq!(client.exchange(play(id)).await, Reply::Failed);
                }
                for (id, expected) in [
                    ("456", Reply::LicenceDenied),
                    (
                        "789",
                        Reply::TrialFailed {
                            stage: TrialStage::Output,
                        },
                    ),
                    ("111", Reply::Failed),
                    (
                        "123",
                        Reply::Played {
                            sample_rate: 44100,
                            bits_per_sample: 24,
                            channels: 2,
                        },
                    ),
                ] {
                    assert_eq!(client.exchange(play(id)).await, expected);
                    assert_eq!(client.exchange(Request::Status).await, Reply::SignedIn);
                }
                assert_eq!(client.exchange(play("222")).await, Reply::Failed);
                assert_eq!(client.exchange(Request::Status).await, Reply::SignedOut);
            })
            .await;
    }

    async fn start_trial(service: &Service, client: &mut TestClient) -> (u32, u32) {
        sign_in(client).await;
        let provider_pid = match &*service.session.borrow() {
            Session::SignedIn(provider) => provider.child.id().unwrap(),
            _ => panic!("expected a signed-in provider"),
        };
        wire::write(&mut client.write, play("999")).await.unwrap();
        let children = format!("/proc/{provider_pid}/task/{provider_pid}/children");
        let audio_pid = timeout(IO_TIMEOUT, async {
            loop {
                if let Ok(pid) = fs::read_to_string(&children).unwrap().trim().parse::<u32>() {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        (provider_pid, audio_pid)
    }

    async fn assert_trial_stopped(provider_pid: u32, audio_pid: u32) {
        assert!(!Path::new(&format!("/proc/{provider_pid}")).exists());
        timeout(IO_TIMEOUT, async {
            loop {
                match fs::read_to_string(format!("/proc/{audio_pid}/stat")) {
                    Ok(stat) if stat.rsplit_once(") ").unwrap().1.starts_with('Z') => break,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                    Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(error) => panic!("could not inspect mock audio subprocess: {error}"),
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn trial_disconnect_cancels_provider_and_audio_subprocess() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let service = service(ATTEMPT_TIMEOUT);
                let mut initiator = TestClient::connect(&service);
                let mut other = TestClient::connect(&service);
                let (provider_pid, audio_pid) = start_trial(&service, &mut initiator).await;
                assert_eq!(other.exchange(Request::Status).await, Reply::Busy);
                assert_eq!(other.exchange(play("123")).await, Reply::Busy);
                drop(initiator);
                timeout(IO_TIMEOUT, async {
                    while other.exchange(Request::Status).await == Reply::Busy {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(other.exchange(Request::Status).await, Reply::SignedOut);
                assert_trial_stopped(provider_pid, audio_pid).await;
            })
            .await;
    }

    #[tokio::test]
    async fn shared_sign_out_cancels_provider_and_audio_subprocess() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let service = service(ATTEMPT_TIMEOUT);
                let mut initiator = TestClient::connect(&service);
                let mut other = TestClient::connect(&service);
                let (provider_pid, audio_pid) = start_trial(&service, &mut initiator).await;
                assert_eq!(other.exchange(Request::SignOut).await, Reply::SignedOut);
                assert_eq!(
                    timeout(IO_TIMEOUT, initiator.read.frame::<Reply>())
                        .await
                        .unwrap()
                        .unwrap(),
                    Some(Reply::SignedOut)
                );
                assert_eq!(other.exchange(Request::Status).await, Reply::SignedOut);
                assert_trial_stopped(provider_pid, audio_pid).await;
            })
            .await;
    }

    #[tokio::test]
    async fn only_the_initiating_connection_can_verify_a_shared_session() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let service = service(ATTEMPT_TIMEOUT);
                let mut initiator = TestClient::connect(&service);
                let mut other = TestClient::connect(&service);
                assert_eq!(
                    initiator.exchange(Request::Begin).await,
                    Reply::CredentialsRequired
                );
                assert_eq!(other.exchange(Request::Begin).await, Reply::Busy);
                assert_eq!(
                    initiator
                        .exchange(credentials("test@example.invalid"))
                        .await,
                    Reply::VerificationRequired
                );
                let verification = || Request::VerificationCode {
                    code: "123456".to_owned().into(),
                };
                assert_eq!(other.exchange(verification()).await, Reply::Rejected);
                assert_eq!(initiator.exchange(verification()).await, Reply::SignedIn);
                let tokens = service.catalogue().unwrap();
                assert_eq!(tokens.developer_token, "fixture-developer");
                assert_eq!(tokens.music_user_token.as_deref(), Some("fixture-user"));
                drop(initiator);
                assert_eq!(other.exchange(Request::Status).await, Reply::SignedIn);
                assert_eq!(other.exchange(Request::SignOut).await, Reply::SignedOut);
                assert!(service.catalogue().is_none());
                assert_eq!(other.exchange(Request::Status).await, Reply::SignedOut);
            })
            .await;
    }

    #[tokio::test]
    async fn disconnect_denial_and_provider_exit_release_the_attempt() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let service = service(ATTEMPT_TIMEOUT);
                let mut initiator = TestClient::connect(&service);
                assert_eq!(
                    initiator.exchange(Request::Begin).await,
                    Reply::CredentialsRequired
                );
                drop(initiator);
                let mut other = TestClient::connect(&service);
                timeout(IO_TIMEOUT, async {
                    while other.exchange(Request::Status).await == Reply::Busy {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                for (account, reply) in [
                    ("reject@example.invalid", Reply::Rejected),
                    ("crash@example.invalid", Reply::Failed),
                ] {
                    assert_eq!(
                        other.exchange(Request::Begin).await,
                        Reply::CredentialsRequired
                    );
                    assert_eq!(other.exchange(credentials(account)).await, reply);
                    assert_eq!(other.exchange(Request::Status).await, Reply::SignedOut);
                }
            })
            .await;
    }

    #[tokio::test]
    async fn expired_challenge_cannot_complete() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let attempt_timeout = Duration::from_secs(1);
                let service = service(attempt_timeout);
                let mut client = TestClient::connect(&service);
                assert_eq!(
                    client.exchange(Request::Begin).await,
                    Reply::CredentialsRequired
                );
                tokio::time::sleep(attempt_timeout).await;
                assert_eq!(
                    client.exchange(credentials("test@example.invalid")).await,
                    Reply::Expired
                );
                assert_eq!(client.exchange(Request::Status).await, Reply::SignedOut);
            })
            .await;
    }
}
