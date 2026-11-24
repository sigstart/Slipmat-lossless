// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-FileCopyrightText: 2026 Jan Vermeulen
// SPDX-License-Identifier: GPL-3.0-or-later

//! Sign-in sessions, and the provider's one playback slot, which the queue
//! claims a track at a time. The provider process itself is `provider.rs`.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use slipmat_core::auth::Reply;
use slipmat_core::auth_wire::{self as wire, Reader, Request};
use tokio::net::{
    UnixListener, UnixStream,
    unix::{OwnedReadHalf, OwnedWriteHalf},
};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, timeout};

use crate::profile_store::ProfileStore;
use crate::provider::{End, IO_TIMEOUT, Mode, Provider, send, send_before, validate_provider};

const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONNECTIONS: usize = 32;
/// A provider lost this often inside the window is broken, not unlucky.
const RECOVERY_LIMIT: usize = 3;
const RECOVERY_WINDOW: Duration = Duration::from_secs(600);

pub struct Endpoint {
    task: JoinHandle<()>,
    service: Rc<Service>,
    _socket: SocketFile,
    _binding_lock: File,
}

impl Endpoint {
    pub async fn bind(provider: PathBuf) -> io::Result<Self> {
        validate_provider(&provider)?;
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
        let profiles = open_profile_store(&socket.path);
        let (recover, recoveries) = mpsc::unbounded_channel();
        let service = Rc::new(Service {
            provider,
            profiles,
            session: RefCell::new(Session::SignedOut),
            restored: std::cell::Cell::new(false),
            sign_out_epoch: Default::default(),
            catalogue: RefCell::new(None),
            profile_save: Default::default(),
            attempt_timeout: ATTEMPT_TIMEOUT,
            recoveries: RefCell::new(Recoveries::default()),
            recover,
            gave_up: std::cell::Cell::new(false),
            diagnosis: RefCell::new(None),
        });
        let task = tokio::task::spawn_local(serve(listener, service.clone(), recoveries));
        tracing::info!("sign-in socket listening");
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

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(profiles) = &self.service.profiles {
            profiles.clear();
        }
    }
}

/// Without a store the provider still signs in, but nothing is saved.
fn open_profile_store(socket: &Path) -> Option<ProfileStore> {
    let runtime = socket.parent()?;
    let Some(state) = slipmat_core::paths::state_dir() else {
        tracing::warn!("no state directory, so the Apple Music session will not be saved");
        return None;
    };
    ProfileStore::open(runtime, &state)
        .inspect_err(|err| {
            tracing::warn!(%err, "the Apple Music session will not be saved");
        })
        .ok()
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

pub(crate) async fn bind_listener(path: &Path) -> io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => return Ok(listener),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {}
        Err(err) => return Err(err),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != slipmat_core::paths::effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "sign-in path is occupied",
        ));
    }
    // Require a refused connection before removing a leftover sign-in socket.
    match timeout(IO_TIMEOUT, UnixStream::connect(path)).await? {
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
            fs::remove_file(path)?;
            UnixListener::bind(path)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "sign-in socket is in use",
        )),
    }
}

async fn serve(
    listener: UnixListener,
    service: Rc<Service>,
    mut recoveries: mpsc::UnboundedReceiver<Duration>,
) {
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut clients = JoinSet::new();
    if service
        .profiles
        .as_ref()
        .is_some_and(ProfileStore::has_archive)
    {
        service.session.replace(Session::Busy);
        let restoring = service.clone();
        clients.spawn_local(async move { restoring.restore(Restore::Startup).await });
    }
    let mut reap = tokio::time::interval(Duration::from_secs(1));
    reap.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    tracing::warn!("sign-in socket accept failed");
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
            Some(delay) = recoveries.recv() => {
                let recovering = service.clone();
                clients.spawn_local(async move {
                    tokio::time::sleep(delay).await;
                    recovering.restore(Restore::Recovery).await;
                });
            }
        }
    }
}

pub struct Service {
    provider: PathBuf,
    profiles: Option<ProfileStore>,
    session: RefCell<Session>,
    restored: std::cell::Cell<bool>,
    sign_out_epoch: std::cell::Cell<u64>,
    catalogue: RefCell<Option<slipmat_core::player::protocol::Tokens>>,
    attempt_timeout: Duration,
    profile_save: tokio::sync::Mutex<()>,
    recoveries: RefCell<Recoveries>,
    /// To the serve loop, which owns the tasks: restore after this delay.
    recover: mpsc::UnboundedSender<Duration>,
    /// Set when recovery stopped trying, until the daemon has said so.
    gave_up: std::cell::Cell<bool>,
    diagnosis: RefCell<Option<Box<fairplay_wire::Diagnosis>>>,
}

/// What a track is claimed for.
#[derive(Clone, Copy)]
enum Run {
    Play,
    Check,
}

impl Run {
    fn request(self, track_id: String) -> fairplay_wire::Request {
        match self {
            Self::Play => fairplay_wire::Request::PlayTrack { track_id },
            Self::Check => fairplay_wire::Request::CheckTrack { track_id },
        }
    }
}

/// Why a saved session is being restored.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Restore {
    /// The daemon started: the saved queue is reloaded with it.
    Startup,
    /// The provider was lost: the queue in memory is still the right one.
    Recovery,
}

/// Recent recoveries, to tell a provider that died once from one that keeps dying.
#[derive(Default)]
struct Recoveries {
    recent: Vec<Instant>,
}

impl Recoveries {
    /// How long to wait before the next attempt, doubling from a second, or
    /// `None` once the window holds the limit.
    fn next(&mut self, now: Instant) -> Option<Duration> {
        self.recent
            .retain(|&at| now.saturating_duration_since(at) < RECOVERY_WINDOW);
        if self.recent.len() >= RECOVERY_LIMIT {
            return None;
        }
        self.recent.push(now);
        Some(Duration::from_secs(1 << (self.recent.len() - 1)))
    }
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
    /// The provider is out on a queue track; sign-out cancels it through this.
    Playing(watch::Sender<TrackState>),
    /// The provider was lost, not signed out of, and the saved session is
    /// being restored. The catalogue credentials stay usable meanwhile, so the
    /// daemon sees no change of account and clears nothing.
    Recovering,
}

#[derive(Clone, Copy)]
enum TrackState {
    Active,
    Cancelled,
    Stopped(Reply),
}

impl Service {
    pub(crate) async fn wait_for_profile_save(&self) {
        drop(self.profile_save.lock().await);
    }

    pub fn sign_out_epoch(&self) -> u64 {
        self.sign_out_epoch.get()
    }

    pub fn take_restored(&self) -> bool {
        self.restored.replace(false)
    }

    /// Whether recovery gave up since this was last asked.
    pub fn take_gave_up(&self) -> bool {
        self.gave_up.replace(false)
    }

    pub fn catalogue(&self) -> Option<slipmat_core::player::protocol::Tokens> {
        let recovering = matches!(*self.session.borrow(), Session::Recovering);
        if recovering || self.is_signed_in() {
            self.catalogue.borrow().clone()
        } else {
            self.catalogue.borrow_mut().take();
            None
        }
    }

    /// The provider went away without a sign-out. With a saved session it is
    /// brought back, unless it keeps going; without one, this is a sign-out.
    fn lose_provider(&self) {
        let saved = self
            .profiles
            .as_ref()
            .is_some_and(ProfileStore::has_archive);
        let delay = saved
            .then(|| self.recoveries.borrow_mut().next(Instant::now()))
            .flatten();
        let recovering = delay.is_some_and(|delay| self.recover.send(delay).is_ok());
        if recovering {
            tracing::warn!(
                ?delay,
                "the playback engine stopped; restoring the saved session"
            );
        } else if saved {
            tracing::warn!("the playback engine keeps stopping; giving up until the next sign-in");
            self.gave_up.set(true);
        }
        *self.session.borrow_mut() = if recovering {
            Session::Recovering
        } else {
            Session::SignedOut
        };
    }

    async fn restore(&self, why: Restore) {
        let reservation = Reservation(&self.session);
        let profile = match &self.profiles {
            Some(profiles) => profiles.restored_profile().await,
            None => Err(io::ErrorKind::NotFound.into()),
        };
        if let Err(err) = &profile {
            tracing::warn!(%err, "the saved session could not be opened");
        }
        if let Ok(profile) = profile
            && let Ok(mut provider) = Provider::spawn(&self.provider, Some(profile), Mode::Restore)
            && matches!(
                timeout(
                    Duration::from_secs(75),
                    provider.exchange(fairplay_wire::Request::Begin)
                )
                .await,
                Ok(Ok(Some(Reply::SignedIn)))
            )
            && matches!(provider.child.try_wait(), Ok(None))
        {
            self.catalogue.replace(provider.catalogue.take());
            self.session.replace(Session::SignedIn(Box::new(provider)));
            self.restored.set(why == Restore::Startup);
            tracing::info!("restored Apple Music session");
            return;
        }
        drop(reservation);
        tracing::info!("saved session could not be restored");
    }

    fn reap_provider(&self) {
        let exited = match &mut *self.session.borrow_mut() {
            Session::SignedIn(provider) => !matches!(provider.child.try_wait(), Ok(None)),
            _ => false,
        };
        if exited {
            self.lose_provider();
        }
    }

    fn status(&self) -> Reply {
        self.reap_provider();
        match &*self.session.borrow() {
            Session::SignedOut => Reply::SignedOut,
            Session::Busy | Session::Recovering => Reply::Busy,
            Session::Playing(track) if matches!(*track.borrow(), TrackState::Active) => {
                Reply::SignedIn
            }
            Session::Playing(_) => Reply::Busy,
            Session::SignedIn(_) => Reply::SignedIn,
        }
    }

    pub fn is_authenticating(&self) -> bool {
        matches!(*self.session.borrow(), Session::Busy | Session::Recovering)
    }

    pub fn is_signed_in(&self) -> bool {
        self.reap_provider();
        match &*self.session.borrow() {
            Session::SignedIn(_) => true,
            Session::Playing(track) => matches!(*track.borrow(), TrackState::Active),
            _ => false,
        }
    }

    pub(crate) async fn sign_out(&self) -> Reply {
        self.reap_provider();
        if matches!(*self.session.borrow(), Session::Busy | Session::Recovering) {
            return Reply::Busy;
        }
        if self
            .profiles
            .as_ref()
            .is_some_and(|profiles| profiles.forget().is_err())
        {
            return Reply::Failed;
        }
        self.restored.set(false);
        self.sign_out_epoch
            .set(self.sign_out_epoch.get().wrapping_add(1));
        let playing = match &*self.session.borrow() {
            Session::Playing(track) => {
                let updates = track.subscribe();
                track.send_replace(TrackState::Cancelled);
                Some(updates)
            }
            _ => None,
        };
        if let Some(mut track) = playing {
            return match track
                .wait_for(|state| matches!(state, TrackState::Stopped(_)))
                .await
                .as_deref()
            {
                Ok(TrackState::Stopped(reply)) => *reply,
                _ => Reply::Failed,
            };
        }
        let mut provider = {
            let mut session = self.session.borrow_mut();
            match *session {
                Session::SignedOut => return Reply::SignedOut,
                Session::Busy | Session::Playing(_) | Session::Recovering => return Reply::Busy,
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
        let mut first = true;
        loop {
            let frame = if std::mem::take(&mut first) {
                match timeout(wire::FIRST_REQUEST_TIMEOUT, read.frame()).await {
                    Ok(frame) => frame,
                    Err(_) => return,
                }
            } else {
                read.frame().await
            };
            let reply = match frame {
                Ok(Some(Request::Status)) => self.status(),
                Ok(Some(Request::SignOut)) => self.sign_out().await,
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
        let profile = match self.profiles.as_ref().map(ProfileStore::empty_profile) {
            Some(Err(_)) => return End::Reply(Reply::Failed),
            profile => profile.and_then(Result::ok),
        };
        let Ok(mut provider) = Provider::spawn(&self.provider, profile, Mode::SignIn) else {
            return End::Reply(Reply::Failed);
        };
        let mut end = provider.authenticate(read, write, deadline).await;
        if matches!(end, End::SignedIn) {
            // Once success reaches the client, graceful shutdown must let
            // the accepted session finish saving.
            let _save = self.profile_save.lock().await;
            // Keep the child only after delivering success to the owner.
            end = if !matches!(provider.child.try_wait(), Ok(None)) {
                End::Reply(Reply::Failed)
            } else if Instant::now() >= deadline {
                End::Reply(Reply::Expired)
            } else if send_before(write, Reply::SignedIn, deadline).await.is_err() {
                End::Disconnected
            } else {
                self.save_profile(&provider).await;
                self.catalogue.replace(provider.catalogue.take());
                *self.session.borrow_mut() = Session::SignedIn(Box::new(provider));
                return End::SignedIn;
            };
        }
        let _ = provider.stop().await;
        drop(reservation);
        end
    }

    async fn save_profile(&self, provider: &Provider) {
        if let (Some(profiles), Some(profile)) = (&self.profiles, &provider.profile) {
            match profiles.save(profile).await {
                Ok(()) => tracing::info!("saved the Apple Music session"),
                Err(err) => tracing::warn!(%err, "the Apple Music session was not saved"),
            }
        }
    }

    /// Claim the session now and drive one queue track on the LocalSet.
    /// Dropping either channel requests a stop.
    pub fn start_queue(
        self: &Rc<Self>,
        track_id: String,
        controls: mpsc::UnboundedReceiver<PlaybackControl>,
    ) -> Result<mpsc::UnboundedReceiver<Reply>, Reply> {
        self.start_track(Run::Play, track_id, controls)
    }

    /// The same, decoding the track to nowhere to see whether it plays through.
    pub fn start_check(
        self: &Rc<Self>,
        track_id: String,
        controls: mpsc::UnboundedReceiver<PlaybackControl>,
    ) -> Result<mpsc::UnboundedReceiver<Reply>, Reply> {
        self.start_track(Run::Check, track_id, controls)
    }

    /// What the provider saw of the last track that would not decode.
    pub fn take_diagnosis(&self) -> Option<Box<fairplay_wire::Diagnosis>> {
        self.diagnosis.take()
    }

    fn start_track(
        self: &Rc<Self>,
        run: Run,
        track_id: String,
        controls: mpsc::UnboundedReceiver<PlaybackControl>,
    ) -> Result<mpsc::UnboundedReceiver<Reply>, Reply> {
        if track_id.is_empty() || !track_id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Reply::Failed);
        }
        self.reap_provider();
        let (track, mut updates) = watch::channel(TrackState::Active);
        let playing = Session::Playing(track.clone());
        let mut provider = {
            let mut session = self.session.borrow_mut();
            match *session {
                Session::SignedIn(_) => {}
                Session::SignedOut => return Err(Reply::SignedOut),
                Session::Busy | Session::Playing(_) | Session::Recovering => {
                    return Err(Reply::Busy);
                }
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
                result = provider.play_track(run.request(track_id), controls, &events) => result,
            };
            let reply = if let Some(reply) = result {
                service.diagnosis.replace(provider.diagnosis.take());
                service.session.replace(Session::SignedIn(provider));
                reply
            } else {
                let signed_out = if provider.stop().await.is_ok() {
                    Reply::SignedOut
                } else {
                    Reply::Failed
                };
                drop(reservation);
                let signing_out = matches!(*track.borrow(), TrackState::Cancelled);
                if !signing_out {
                    service.lose_provider();
                }
                track.send_replace(TrackState::Stopped(signed_out));
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
        if matches!(
            *session,
            Session::Busy | Session::Playing(_) | Session::Recovering
        ) {
            *session = Session::SignedOut;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slipmat_core::auth::PlaybackStage;
    use std::process::Stdio;
    use tokio::process::Command;

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
        service_with(attempt_timeout, None).0
    }

    /// A service with a profile store, and the serve loop's end of the
    /// recovery channel so a test can see what was asked for.
    fn service_with(
        attempt_timeout: Duration,
        profiles: Option<ProfileStore>,
    ) -> (Rc<Service>, mpsc::UnboundedReceiver<Duration>) {
        let (recover, recoveries) = mpsc::unbounded_channel();
        let service = Rc::new(Service {
            provider: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/auth-provider"),
            profiles,
            session: RefCell::new(Session::SignedOut),
            restored: std::cell::Cell::new(false),
            sign_out_epoch: Default::default(),
            catalogue: RefCell::new(None),
            profile_save: Default::default(),
            attempt_timeout,
            recoveries: RefCell::new(Recoveries::default()),
            recover,
            gave_up: std::cell::Cell::new(false),
            diagnosis: RefCell::new(None),
        });
        (service, recoveries)
    }

    fn credentials(account: &str) -> Request {
        Request::Credentials {
            account: account.to_owned().into(),
            password: "fixture-password".to_owned().into(),
        }
    }

    /// Play one queue track and return its terminal reply.
    async fn play_to_end(service: &Rc<Service>, id: &str) -> Reply {
        let (_controls, input) = mpsc::unbounded_channel();
        let mut events = match service.start_queue(id.to_owned(), input) {
            Ok(events) => events,
            Err(reply) => return reply,
        };
        loop {
            match events.recv().await {
                Some(reply) if reply.is_playback_update() => {}
                reply => return reply.unwrap_or(Reply::Failed),
            }
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
                        printf '%s\n' '{"type":"signedIn","catalogue":{"developerToken":"fixture-dev","musicUserToken":"fixture-user","storefront":"us","authorized":true}}'
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
                    profile: None,
                    input: worker.stdin.take().unwrap(),
                    output: Reader::new(worker.stdout.take().unwrap()),
                    child: worker,
                    diagnosis: None,
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

                    assert_eq!(
                        play_to_end(&service, "456").await,
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
    async fn queue_tracks_report_their_terminal_status() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let service = service(ATTEMPT_TIMEOUT);
                let mut client = TestClient::connect(&service);
                assert_eq!(play_to_end(&service, "123").await, Reply::SignedOut);
                sign_in(&mut client).await;

                for id in ["", "i.123", "+123", " 123", "１２３"] {
                    assert_eq!(play_to_end(&service, id).await, Reply::Failed);
                }
                for (id, expected) in [
                    ("456", Reply::LicenceDenied),
                    (
                        "789",
                        Reply::PlaybackFailed {
                            stage: PlaybackStage::Output,
                            fault: None,
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
                    assert_eq!(play_to_end(&service, id).await, expected);
                    assert_eq!(client.exchange(Request::Status).await, Reply::SignedIn);
                }
                assert_eq!(play_to_end(&service, "222").await, Reply::Failed);
                assert_eq!(client.exchange(Request::Status).await, Reply::SignedOut);
            })
            .await;
    }

    /// A queue track the fixture never finishes, and the audio child it spawns.
    struct StalledTrack {
        provider_pid: u32,
        audio_pid: u32,
        controls: mpsc::UnboundedSender<PlaybackControl>,
        events: mpsc::UnboundedReceiver<Reply>,
    }

    async fn start_stalled_track(service: &Rc<Service>, client: &mut TestClient) -> StalledTrack {
        sign_in(client).await;
        let provider_pid = match &*service.session.borrow() {
            Session::SignedIn(provider) => provider.child.id().unwrap(),
            _ => panic!("expected a signed-in provider"),
        };
        let (controls, input) = mpsc::unbounded_channel();
        let events = service.start_queue("999".to_owned(), input).unwrap();
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
        StalledTrack {
            provider_pid,
            audio_pid,
            controls,
            events,
        }
    }

    async fn assert_track_stopped(provider_pid: u32, audio_pid: u32) {
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
    async fn a_dropped_queue_stops_the_provider_and_audio_subprocess() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let service = service(ATTEMPT_TIMEOUT);
                let mut client = TestClient::connect(&service);
                let track = start_stalled_track(&service, &mut client).await;
                assert_eq!(client.exchange(Request::Status).await, Reply::SignedIn);
                let (_, input) = mpsc::unbounded_channel();
                assert!(matches!(
                    service.start_queue("123".to_owned(), input),
                    Err(Reply::Busy)
                ));
                drop((track.controls, track.events));
                // The fixture ignores `stop`, so this waits out the stop deadline.
                timeout(IO_TIMEOUT * 2, async {
                    while client.exchange(Request::Status).await == Reply::SignedIn {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(client.exchange(Request::Status).await, Reply::SignedOut);
                assert_track_stopped(track.provider_pid, track.audio_pid).await;
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
                let mut track = start_stalled_track(&service, &mut initiator).await;
                assert_eq!(other.exchange(Request::SignOut).await, Reply::SignedOut);
                assert_eq!(
                    timeout(IO_TIMEOUT, track.events.recv()).await.unwrap(),
                    Some(Reply::SignedOut)
                );
                assert_eq!(other.exchange(Request::Status).await, Reply::SignedOut);
                assert_track_stopped(track.provider_pid, track.audio_pid).await;
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

    /// A store holding a saved session, in a directory removed on drop.
    struct SavedSession(PathBuf);

    impl SavedSession {
        fn new() -> (Self, ProfileStore) {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("slipmat-recovery-{}-{unique}", std::process::id()));
            fs::create_dir_all(dir.join("profiles")).unwrap();
            let archive = dir.join("fairplay-session.enc");
            fs::write(&archive, b"sealed").unwrap();
            let store = ProfileStore::at(archive, dir.join("profiles"));
            (Self(dir), store)
        }
    }

    impl Drop for SavedSession {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tokens() -> slipmat_core::player::protocol::Tokens {
        slipmat_core::player::protocol::Tokens {
            developer_token: "fixture-dev".into(),
            music_user_token: Some("fixture-user".into()),
            storefront: "us".into(),
            authorized: true,
        }
    }

    #[test]
    fn recoveries_back_off_then_give_up_until_the_window_passes() {
        let mut recoveries = Recoveries::default();
        let start = Instant::now();
        let delays: Vec<_> = (0..4).map(|_| recoveries.next(start)).collect();
        assert_eq!(
            delays,
            [
                Some(Duration::from_secs(1)),
                Some(Duration::from_secs(2)),
                Some(Duration::from_secs(4)),
                None
            ]
        );
        assert_eq!(
            recoveries.next(start + RECOVERY_WINDOW),
            Some(Duration::from_secs(1))
        );
    }

    #[tokio::test]
    async fn a_provider_lost_mid_track_is_recovered_without_dropping_the_catalogue() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_saved, store) = SavedSession::new();
                let (service, mut recoveries) = service_with(ATTEMPT_TIMEOUT, Some(store));
                // Answers the track, then dies the way an unreadable reply ends it.
                let mut worker = Command::new("/bin/sh")
                    .arg("-ec")
                    .arg(r#"IFS= read -r frame; printf '%s\n' '{"type":"playing"}'"#)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .process_group(0)
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap();
                let provider = Provider {
                    catalogue: None,
                    profile: None,
                    input: worker.stdin.take().unwrap(),
                    output: Reader::new(worker.stdout.take().unwrap()),
                    child: worker,
                    diagnosis: None,
                };
                service.catalogue.replace(Some(tokens()));
                service
                    .session
                    .replace(Session::SignedIn(Box::new(provider)));

                let reply = timeout(IO_TIMEOUT, play_to_end(&service, "123"))
                    .await
                    .unwrap();
                assert_eq!(reply, Reply::Failed);
                assert!(matches!(*service.session.borrow(), Session::Recovering));
                assert_eq!(service.catalogue(), Some(tokens()));
                assert!(service.is_authenticating());
                assert_eq!(service.status(), Reply::Busy);
                assert_eq!(recoveries.try_recv(), Ok(Duration::from_secs(1)));
                assert_eq!(service.sign_out_epoch(), 0);
                assert!(!service.take_gave_up());
            })
            .await;
    }

    #[test]
    fn a_provider_that_keeps_stopping_ends_signed_out() {
        let (_saved, store) = SavedSession::new();
        let (service, mut recoveries) = service_with(ATTEMPT_TIMEOUT, Some(store));
        service.catalogue.replace(Some(tokens()));
        for _ in 0..RECOVERY_LIMIT {
            service.lose_provider();
            assert!(matches!(*service.session.borrow(), Session::Recovering));
            assert!(recoveries.try_recv().is_ok());
        }
        service.lose_provider();
        assert!(matches!(*service.session.borrow(), Session::SignedOut));
        assert!(recoveries.try_recv().is_err());
        assert!(service.take_gave_up());
        assert_eq!(service.catalogue(), None);
    }

    #[test]
    fn without_a_saved_session_a_lost_provider_is_signed_out() {
        let (service, mut recoveries) = service_with(ATTEMPT_TIMEOUT, None);
        service.lose_provider();
        assert!(matches!(*service.session.borrow(), Session::SignedOut));
        assert!(recoveries.try_recv().is_err());
        // Nothing was saved, so there was nothing to give up on.
        assert!(!service.take_gave_up());
    }

    #[tokio::test]
    async fn a_check_asks_for_checktrack_and_keeps_what_the_provider_saw() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let service = service(ATTEMPT_TIMEOUT);
                let mut worker = Command::new("/bin/sh")
                    .arg("-ec")
                    .arg(
                        r#"
                        IFS= read -r frame
                        [ "$frame" = '{"type":"checkTrack","track_id":"123"}' ]
                        printf '%s\n' '{"type":"playbackFailed","stage":"output","fault":{"kind":"decoder","step":"audio","decoded_ms":986940},"diagnosis":{"fragment":{"index":66,"startsAtMs":986960,"samples":161,"splitSamples":0,"tfhdFlags":131098,"trunFlags":513,"description":2}}}'
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
                let provider = Provider {
                    catalogue: None,
                    profile: None,
                    input: worker.stdin.take().unwrap(),
                    output: Reader::new(worker.stdout.take().unwrap()),
                    child: worker,
                    diagnosis: None,
                };
                service.session.replace(Session::SignedIn(Box::new(provider)));

                let (_control, controls) = mpsc::unbounded_channel();
                let mut events = service.start_check("123".to_owned(), controls).unwrap();
                let reply = timeout(IO_TIMEOUT, events.recv()).await.unwrap();
                assert!(matches!(
                    reply,
                    Some(Reply::PlaybackFailed {
                        stage: PlaybackStage::Output,
                        fault: Some(_),
                    })
                ));
                let diagnosis = service.take_diagnosis().unwrap();
                assert_eq!(diagnosis.fragment.map(|shape| shape.description), Some(2));
                // Taken once: the next track's failure is not this one's.
                assert!(service.take_diagnosis().is_none());
                assert!(service.is_signed_in());
            })
            .await;
    }
}
