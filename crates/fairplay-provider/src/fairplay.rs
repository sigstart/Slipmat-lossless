// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-FileCopyrightText: 2026 Jan Vermeulen
// SPDX-License-Identifier: GPL-3.0-or-later

//! The thread that owns every call into the FairPlay libraries.
//!
//! `hybris_backend_init` calls `setenv`, so it runs on the main thread while
//! that is the only thread. Every later call runs on one "fairplay" thread fed
//! by a channel. The libraries' thread affinity is undocumented, so one thread
//! is the assumption that needs no proof.
//!
//! Apple's verification prompt arrives as a C callback inside
//! `hybris_fairplay_init`. It blocks on a channel until the session task has
//! the code from the daemon. Any other prompt is a dialog the library would
//! otherwise answer on the user's behalf, so the provider exits instead.
//!
//! Key contexts are raw handles, so they stay on this thread: a fragment
//! travels here to be decrypted in place and travels back. Content keys never
//! leave the library.

use std::ffi::{CStr, CString, OsStr, c_char, c_int, c_uint, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use fairplay_wire::{PlaybackFault, Secret};
use serde::Deserialize;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::ffi::{Library, STALE_CONTEXT, SecretCString, take_malloced};
use crate::mp4::Fragment;
use crate::profile;

const LIBRARY: &CStr = c"/app/libdrm_client.so";
const LINKER_DIR: &CStr = c"/app/hybris-linker";
const LIB64_DIR: &CStr = c"/state/rootfs/system/lib64";
pub const BASE_DIR: &CStr = c"/state/files";

pub fn path(path: &CStr) -> &Path {
    Path::new(OsStr::from_bytes(path.to_bytes()))
}

pub struct Credentials {
    pub account: Secret,
    pub password: Secret,
}

pub enum Challenge {
    /// Apple asked for the six-digit code: answer with it, or `None` to fail.
    TwoFactor(mpsc::SyncSender<Option<Secret>>),
    /// Any other prompt. The provider must exit without answering it.
    Unsupported,
}

pub struct Account {
    pub developer_token: String,
    pub music_user_token: String,
}

/// A key a track decrypts with: the adam ID it is opened under, and its URI.
#[derive(Clone)]
pub struct KeyRequest {
    pub adam: String,
    pub uri: String,
}

enum Job {
    Start {
        credentials: Option<Credentials>,
        challenges: UnboundedSender<Challenge>,
        done: oneshot::Sender<Result<()>>,
    },
    Account(oneshot::Sender<Result<Account>>),
    /// Replaces the open keys. Fails with the index of a key that did not open.
    OpenKeys {
        keys: Vec<KeyRequest>,
        done: oneshot::Sender<Result<(), usize>>,
    },
    Decrypt {
        key: usize,
        fragment: Fragment,
        done: oneshot::Sender<Result<(Fragment, Duration), PlaybackFault>>,
    },
    /// Requests a new lease and drops every key context, now.
    RenewLease(oneshot::Sender<bool>),
}

pub struct Fairplay {
    jobs: mpsc::Sender<Job>,
}

impl Fairplay {
    /// Loads the library and starts the Android runtime, then hands every
    /// later call to the fairplay thread. Call it before any other thread
    /// exists.
    pub fn start() -> Result<Self> {
        if threads()? != 1 {
            bail!("the Android runtime must start before any other thread");
        }
        let library = Library::open(path(LIBRARY))?;
        // libhybris-core exits the process when the linker shim is missing.
        if !path(LINKER_DIR).join("q.so").is_file() || !path(LIB64_DIR).is_dir() {
            bail!("the FairPlay libraries are not staged");
        }
        // SAFETY: the strings are NUL-terminated and a null core path selects
        // the embedded one. Its `setenv` calls are sound because this thread
        // was checked to be the only one.
        let status = unsafe {
            (library.backend_init)(LINKER_DIR.as_ptr(), LIB64_DIR.as_ptr(), std::ptr::null())
        };
        if status != 0 {
            bail!("the Android runtime could not start");
        }
        let (jobs, inbox) = mpsc::channel();
        std::thread::Builder::new()
            .name("fairplay".into())
            .spawn(move || serve(library, inbox))?;
        Ok(Self { jobs })
    }

    /// Signs in with credentials, or reuses the restored profile without them.
    pub async fn start_session(
        &self,
        credentials: Option<Credentials>,
        challenges: UnboundedSender<Challenge>,
    ) -> Result<()> {
        let (done, result) = oneshot::channel();
        self.send(Job::Start {
            credentials,
            challenges,
            done,
        })?;
        result.await?
    }

    pub async fn account(&self) -> Result<Account> {
        let (done, result) = oneshot::channel();
        self.send(Job::Account(done))?;
        result.await?
    }

    /// Opens a track's keys. A key that does not open is Apple refusing it,
    /// which is the licence check: `Err` names that key.
    pub async fn open_keys(&self, keys: Vec<KeyRequest>) -> Result<Result<(), usize>> {
        let (done, result) = oneshot::channel();
        self.send(Job::OpenKeys { keys, done })?;
        Ok(result.await?)
    }

    /// Decrypts a fragment's protected ranges in place with an open key.
    /// Also says how old the key context was that decrypted it.
    pub async fn decrypt(
        &self,
        key: usize,
        fragment: Fragment,
    ) -> Result<(Fragment, Duration), PlaybackFault> {
        let (done, result) = oneshot::channel();
        self.send(Job::Decrypt {
            key,
            fragment,
            done,
        })
        .map_err(|_| PlaybackFault::Worker)?;
        result.await.map_err(|_| PlaybackFault::Worker)?
    }

    /// Renews the session's playback lease. A Widevine licence makes the web
    /// player's session the account's active one, and Apple then refuses new
    /// FairPlay keys to this one until it asks again. True when it took.
    pub async fn renew_lease(&self) -> bool {
        let (done, result) = oneshot::channel();
        if self.send(Job::RenewLease(done)).is_err() {
            return false;
        }
        result.await.unwrap_or(false)
    }

    fn send(&self, job: Job) -> Result<()> {
        self.jobs
            .send(job)
            .map_err(|_| anyhow!("the fairplay thread stopped"))
    }
}

fn threads() -> Result<usize> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .and_then(|count| count.trim().parse().ok())
        .context("unreadable thread count")
}

fn serve(library: Library, inbox: mpsc::Receiver<Job>) {
    let mut keys = Vec::new();
    let mut refresh = Refresh::new();
    for job in inbox {
        match job {
            Job::Start {
                credentials,
                challenges,
                done,
            } => {
                let _ = done.send(start_session(&library, credentials, challenges));
            }
            Job::Account(done) => {
                let _ = done.send(account(&library));
            }
            Job::OpenKeys {
                keys: requests,
                done,
            } => {
                keys.clear();
                refresh = Refresh::new();
                let opened = requests
                    .iter()
                    .enumerate()
                    .try_for_each(|(index, request)| {
                        keys.push(KeyContext::open(&library, request).ok_or(index)?);
                        Ok(())
                    });
                let _ = done.send(opened);
            }
            Job::RenewLease(done) => {
                let _ = done.send(refresh.now(&library));
            }
            Job::Decrypt {
                key,
                mut fragment,
                done,
            } => {
                refresh.if_due(&library);
                let result = match keys.get_mut(key) {
                    Some(key) => key
                        .decrypt(&library, &mut fragment)
                        .map(|age| (fragment, age)),
                    None => Err(PlaybackFault::Container),
                };
                let _ = done.send(result);
            }
        }
    }
}

/// An open key context. The handle belongs to `libandroidappmusic.so`, which
/// caches it per session; it is never freed here.
struct KeyContext {
    adam: CString,
    uri: CString,
    handle: *mut c_void,
    epoch: c_uint,
    opened: Instant,
}

impl KeyContext {
    fn open(library: &Library, request: &KeyRequest) -> Option<Self> {
        let mut key = Self {
            adam: CString::new(request.adam.as_str()).ok()?,
            uri: CString::new(request.uri.as_str()).ok()?,
            handle: std::ptr::null_mut(),
            epoch: 0,
            opened: Instant::now(),
        };
        key.reopen(library).then_some(key)
    }

    fn reopen(&mut self, library: &Library) -> bool {
        // SAFETY: both strings are NUL-terminated and `epoch` is writable.
        self.handle = unsafe {
            (library.open_key_context)(self.adam.as_ptr(), self.uri.as_ptr(), &mut self.epoch)
        };
        self.opened = Instant::now();
        !self.handle.is_null()
    }

    /// Decrypts in place and says how old the context was that did it.
    fn decrypt(
        &mut self,
        library: &Library,
        fragment: &mut Fragment,
    ) -> Result<Duration, PlaybackFault> {
        let age = self.opened.elapsed();
        for range in &fragment.protected {
            let block = fragment
                .bytes
                .get_mut(range.clone())
                .ok_or(PlaybackFault::Container)?;
            let mut status = self.decrypt_block(library, block)?;
            // A lease refresh freed the context: reopen it and retry once.
            if status == STALE_CONTEXT {
                if !self.reopen(library) {
                    return Err(PlaybackFault::KeyReopen);
                }
                status = self.decrypt_block(library, block)?;
            }
            if status != 0 {
                return Err(PlaybackFault::Decrypt { status });
            }
        }
        Ok(age)
    }

    fn decrypt_block(&self, library: &Library, block: &mut [u8]) -> Result<c_int, PlaybackFault> {
        let len = u32::try_from(block.len()).map_err(|_| PlaybackFault::Container)?;
        // SAFETY: the handle came from `hybris_open_kd_ctx_from_uri` with this
        // epoch, and `block` is `len` writable bytes of whole 16-byte blocks.
        Ok(unsafe { (library.decrypt)(self.handle, self.epoch, block.as_mut_ptr(), len) })
    }
}

/// When a track's key contexts are next refreshed. Open contexts go bad
/// about 15 minutes in without the lease manager saying so: fragments
/// decrypted 912 s after the keys opened failed to decode, three runs out of
/// three, while those at 897 s and a whole track decrypted in 5 s were fine.
struct Refresh {
    due: Instant,
}

/// A third under the lifetime measured, for one lease request per interval.
const KEY_REFRESH: Duration = Duration::from_secs(600);
/// After a refresh that did not take: soon, but not once per fragment.
const KEY_REFRESH_RETRY: Duration = Duration::from_secs(60);

impl Refresh {
    fn new() -> Self {
        Self {
            due: Instant::now() + KEY_REFRESH,
        }
    }

    /// Requests a new lease and drops every cached context once due. The open
    /// contexts then report stale, and `KeyContext::decrypt` reopens them.
    fn if_due(&mut self, library: &Library) {
        if Instant::now() >= self.due {
            self.now(library);
        }
    }

    /// Refreshes now and says whether the rebuilt context is ready.
    fn now(&mut self, library: &Library) -> bool {
        // SAFETY: takes no arguments; the library serialises its own state.
        let ready = unsafe { (library.refresh_keys)() } == 0;
        self.due = Instant::now()
            + if ready {
                KEY_REFRESH
            } else {
                KEY_REFRESH_RETRY
            };
        ready
    }
}

struct AuthContext {
    challenges: UnboundedSender<Challenge>,
}

fn start_session(
    library: &Library,
    credentials: Option<Credentials>,
    challenges: UnboundedSender<Challenge>,
) -> Result<()> {
    let base = path(BASE_DIR);
    profile::prepare(base)?;
    if credentials.is_none() && !profile::has_session(base) {
        profile::forget_token(base);
        bail!("no saved session to restore");
    }
    let device = profile::device_info(base);
    let device = SecretCString::new(&device).context("invalid device string")?;
    let secrets = match &credentials {
        Some(Credentials { account, password }) => Some((
            SecretCString::new(account.expose()).context("invalid account")?,
            SecretCString::new(password.expose()).context("invalid password")?,
        )),
        None => None,
    };
    let (account, password) = secrets.as_ref().map_or(
        (std::ptr::null(), std::ptr::null()),
        |(account, password)| (account.as_ptr(), password.as_ptr()),
    );
    let context = AuthContext { challenges };
    // SAFETY: every string is NUL-terminated and outlives the call, and
    // `context` is what `on_challenge` reads through the user-data pointer.
    let status = unsafe {
        (library.fairplay_init)(
            BASE_DIR.as_ptr(),
            device.as_ptr(),
            LIB64_DIR.as_ptr(),
            account,
            password,
            Some(on_challenge),
            (&raw const context).cast_mut().cast(),
        )
    };
    drop(secrets);
    if status != 0 {
        bail!("Apple's sign-in did not complete");
    }
    profile::mark_session(base)?;
    Ok(())
}

/// The library's prompt callback. Runs inside `hybris_fairplay_init`.
unsafe extern "C" fn on_challenge(
    kind: *const c_char,
    out: *mut c_char,
    size: c_int,
    user_data: *mut c_void,
) {
    let Ok(size) = usize::try_from(size) else {
        return;
    };
    if out.is_null() || size == 0 {
        return;
    }
    // SAFETY: the library passes a writable buffer of `size` bytes.
    let buffer = unsafe { std::slice::from_raw_parts_mut(out.cast::<u8>(), size) };
    buffer.fill(0);
    if kind.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: `start_session` passes its `AuthContext` and keeps it alive
    // until `hybris_fairplay_init` returns.
    let context = unsafe { &*user_data.cast::<AuthContext>() };
    // SAFETY: the prompt type is a NUL-terminated string.
    if unsafe { CStr::from_ptr(kind) } != c"2fa" {
        let _ = context.challenges.send(Challenge::Unsupported);
        // The session task reports the prompt and exits the process. Never
        // return to the library, which would pick an answer itself.
        std::thread::sleep(Duration::from_secs(10));
        // SAFETY: ends the process without running native atexit handlers.
        unsafe { libc::_exit(1) };
    }
    let (answer, code) = mpsc::sync_channel(1);
    if context
        .challenges
        .send(Challenge::TwoFactor(answer))
        .is_err()
    {
        return;
    }
    if let Ok(Some(code)) = code.recv() {
        let digits = code.expose().as_bytes();
        if digits.len() < size {
            buffer[..digits.len()].copy_from_slice(digits);
        }
    }
}

fn account(library: &Library) -> Result<Account> {
    // SAFETY: the library returns a malloc'd JSON string or null.
    let json = unsafe { take_malloced((library.get_account)()) }.context("no account")?;
    parse_account(&json)
}

fn parse_account(json: &[u8]) -> Result<Account> {
    #[derive(Deserialize)]
    struct Raw {
        dev_token: String,
        music_token: String,
    }
    let raw: Raw = serde_json::from_slice(json).map_err(|_| anyhow!("unreadable account"))?;
    if raw.dev_token.is_empty() || raw.music_token.is_empty() {
        bail!("the account has no catalogue authorization");
    }
    Ok(Account {
        developer_token: raw.dev_token,
        music_user_token: raw.music_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_account_needs_both_tokens() {
        let account =
            parse_account(br#"{"storefront_id":"143487","dev_token":"dev","music_token":"user"}"#)
                .unwrap();
        assert_eq!(account.developer_token, "dev");
        assert_eq!(account.music_user_token, "user");
        assert!(
            parse_account(br#"{"storefront_id":"143487","dev_token":"","music_token":"user"}"#)
                .is_err()
        );
        assert!(parse_account(b"not json").is_err());
    }

    #[test]
    fn the_thread_count_is_readable() {
        assert!(threads().unwrap() >= 1);
    }
}
