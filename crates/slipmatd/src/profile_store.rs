// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The FairPlay provider's saved session.
//!
//! The provider keeps its Apple session in a profile directory the daemon
//! creates under `$XDG_RUNTIME_DIR`, which must be tmpfs so the decrypted files
//! never reach a disk. The sandbox sees it as `/state`. After a sign-in the
//! client accepted, the daemon snapshots an allowlist of files from it and
//! seals them with AES-256-GCM into `fairplay-session.enc`. The key lives in
//! Secret Service, reached through `secret-tool`. A restore decrypts the
//! archive into a fresh profile directory before the provider starts.
//!
//! The format is the one the Go `profile-store` helper wrote: a nonce, then
//! the sealed JSON object of file name to base64 contents, with the version
//! string as associated data. Sessions saved by the helper therefore restore.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::{CString, OsStr};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use zeroize::{Zeroize, Zeroizing};

const MAX_PROFILE: u64 = 16 << 20;
const ARCHIVE_VERSION: &[u8] = b"slipmat-fairplay-profile-v2";
const KEYRING_TIMEOUT: Duration = Duration::from_secs(5);
const NONCE_LEN: usize = 12;

const PROFILE_FILES: [&str; 13] = [
    "device-id",
    "adi.pb",
    "IC-Info.sids",
    "fsi.pdat",
    "session-ready",
    "STOREFRONT_ID",
    "MUSIC_TOKEN",
    "mpl_db/accounts.sqlitedb",
    "mpl_db/accounts.sqlitedb-wal",
    "mpl_db/accounts.sqlitedb-journal",
    "mpl_db/cookies.sqlitedb",
    "mpl_db/cookies.sqlitedb-wal",
    "mpl_db/cookies.sqlitedb-journal",
];

pub(crate) struct ProfileStore {
    pub(crate) archive: PathBuf,
    root: PathBuf,
    next: Cell<u64>,
}

/// A profile directory for one provider process, removed when dropped.
pub(crate) struct Profile {
    pub(crate) path: PathBuf,
    dir: File,
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl ProfileStore {
    /// Clears profiles a previous daemon left behind and checks the runtime
    /// directory is private tmpfs.
    pub(crate) fn open(runtime: &Path, state: &Path) -> io::Result<Self> {
        if !is_tmpfs(runtime)? {
            return Err(io::Error::other("the runtime directory is not tmpfs"));
        }
        fs::create_dir_all(state)?;
        let store = Self {
            archive: state.join("fairplay-session.enc"),
            root: runtime.join("slipmat-fairplay-profile"),
            next: Cell::new(0),
        };
        store.clear();
        DirBuilder::new().mode(0o700).create(&store.root)?;
        Ok(store)
    }

    pub(crate) fn clear(&self) {
        let _ = fs::remove_dir_all(&self.root);
    }

    pub(crate) fn has_archive(&self) -> bool {
        self.archive.is_file()
    }

    pub(crate) fn forget(&self) -> io::Result<()> {
        match fs::remove_file(&self.archive) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    pub(crate) fn empty_profile(&self) -> io::Result<Profile> {
        let id = self.next.replace(self.next.get() + 1);
        let path = self.root.join(id.to_string());
        DirBuilder::new().mode(0o700).create(&path)?;
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&path);
        let profile = Profile {
            dir: match dir {
                Ok(dir) => dir,
                Err(err) => {
                    let _ = fs::remove_dir(&path);
                    return Err(err);
                }
            },
            path,
        };
        Ok(profile)
    }

    pub(crate) async fn restored_profile(&self) -> io::Result<Profile> {
        let blob = Zeroizing::new(read_limited(
            &mut File::open(&self.archive)?,
            2 * MAX_PROFILE,
        )?);
        let key = keyring(&self.archive, false).await?;
        let profile = self.empty_profile()?;
        let files = open_archive(&key, &blob)?;
        write_profile(&profile.path, &files)?;
        Ok(profile)
    }

    pub(crate) async fn save(&self, profile: &Profile) -> io::Result<()> {
        let data = snapshot(&profile.dir)?;
        if snapshot(&profile.dir)? != data {
            return Err(io::Error::other("profile changed during snapshot"));
        }
        let key = keyring(&self.archive, true).await?;
        let sealed = seal(&key, &data)?;
        replace_archive(&self.archive, &sealed)
    }
}

fn is_tmpfs(path: &Path) -> io::Result<bool> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `path` is NUL-terminated and `stat` is a valid out-pointer.
    let mut stat = unsafe { std::mem::zeroed::<libc::statfs>() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.f_type == libc::TMPFS_MAGIC)
}

type Files = BTreeMap<String, Zeroizing<Vec<u8>>>;

/// Opens one component at a time so `O_NOFOLLOW` covers the whole path.
fn open_beneath(dir: &File, path: &str) -> io::Result<File> {
    let mut parent: Option<File> = None;
    let mut components = path.split('/').peekable();
    while let Some(name) = components.next() {
        if name.is_empty() || name == "." || name == ".." {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        // O_NONBLOCK: a FIFO must not block here before the type check.
        let mut flags =
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY;
        if components.peek().is_some() {
            flags |= libc::O_DIRECTORY;
        }
        let name = CString::new(name)?;
        let at = parent.as_ref().unwrap_or(dir).as_raw_fd();
        // SAFETY: `at` is an open directory and `name` is NUL-terminated.
        let fd = unsafe { libc::openat(at, name.as_ptr(), flags) };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `openat` just returned this descriptor and nothing else owns it.
        parent = Some(File::from(unsafe { OwnedFd::from_raw_fd(fd) }));
    }
    parent.ok_or_else(|| io::ErrorKind::InvalidInput.into())
}

fn read_limited(file: &mut File, limit: u64) -> io::Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.size() > limit {
        return Err(io::Error::other("invalid profile file"));
    }
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        data.zeroize();
        return Err(io::Error::other("profile file too large"));
    }
    Ok(data)
}

fn snapshot(profile: &File) -> io::Result<Zeroizing<Vec<u8>>> {
    let mut files = BTreeMap::<&str, String>::new();
    let mut size = 0;
    for name in PROFILE_FILES {
        let read = open_beneath(profile, &format!("files/{name}"))
            .and_then(|mut file| read_limited(&mut file, MAX_PROFILE - size));
        let data = match read {
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                files.values_mut().for_each(Zeroize::zeroize);
                return Err(err);
            }
            Ok(data) => Zeroizing::new(data),
        };
        size += data.len() as u64;
        files.insert(name, STANDARD.encode(&*data));
    }
    let present = |name| {
        files
            .get(name)
            .is_some_and(|data: &String| !data.is_empty())
    };
    let encoded = if present("session-ready") && present("mpl_db/accounts.sqlitedb") {
        serde_json::to_vec(&files).map_err(io::Error::other)
    } else {
        Err(io::Error::other("incomplete profile"))
    };
    files.values_mut().for_each(Zeroize::zeroize);
    encoded.map(Zeroizing::new)
}

fn seal(key: &Key<Aes256Gcm>, data: &[u8]) -> io::Result<Vec<u8>> {
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let sealed = Aes256Gcm::new(key)
        .encrypt(
            &nonce,
            Payload {
                msg: data,
                aad: ARCHIVE_VERSION,
            },
        )
        .map_err(|_| io::Error::other("profile encryption failed"))?;
    Ok([nonce.as_slice(), &sealed].concat())
}

fn open_archive(key: &Key<Aes256Gcm>, blob: &[u8]) -> io::Result<Files> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid profile archive");
    let (nonce, sealed) = blob.split_at_checked(NONCE_LEN).ok_or_else(invalid)?;
    let data = Zeroizing::new(
        Aes256Gcm::new(key)
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: ARCHIVE_VERSION,
                },
            )
            .map_err(|_| invalid())?,
    );
    let mut encoded: BTreeMap<String, String> =
        serde_json::from_slice(&data).map_err(|_| invalid())?;
    let mut files = Files::new();
    for (name, value) in &mut encoded {
        let decoded = STANDARD.decode(value.as_bytes());
        value.zeroize();
        if PROFILE_FILES.contains(&name.as_str()) {
            files.insert(
                name.clone(),
                Zeroizing::new(decoded.map_err(|_| invalid())?),
            );
        }
    }
    Ok(files)
}

fn write_profile(profile: &Path, files: &Files) -> io::Result<()> {
    let root = profile.join("files");
    DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(root.join("mpl_db"))?;
    for (name, data) in files {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join(name))?
            .write_all(data)?;
    }
    Ok(())
}

fn replace_archive(archive: &Path, sealed: &[u8]) -> io::Result<()> {
    let mut suffix = [0; 8];
    OsRng.fill_bytes(&mut suffix);
    let temp = archive.with_file_name(format!(".fairplay-session-{}", hex(&suffix)));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        file.write_all(sealed)?;
        file.sync_all()?;
        fs::rename(&temp, archive)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

/// The archive's key from Secret Service, created on first save.
async fn keyring(archive: &Path, create: bool) -> io::Result<Key<Aes256Gcm>> {
    let attributes = [
        OsStr::new("application"),
        OsStr::new("dev.miguelrincon.Slipmat"),
        OsStr::new("profile"),
        archive.as_os_str(),
    ];
    let lookup = timeout(
        KEYRING_TIMEOUT,
        Command::new("secret-tool")
            .arg("lookup")
            .args(attributes)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let stdout = Zeroizing::new(lookup.stdout);
    if lookup.status.success() {
        return parse_key(stdout.trim_ascii());
    }
    if !create {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no profile key in the keyring",
        ));
    }
    let key = Aes256Gcm::generate_key(&mut OsRng);
    let encoded = Zeroizing::new(hex(&key));
    let mut store = Command::new("secret-tool")
        .arg("store")
        .arg("--label=Slipmat FairPlay session key")
        .args(attributes)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stored = timeout(KEYRING_TIMEOUT, async {
        let mut input = store
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("no stdin"))?;
        input.write_all(encoded.as_bytes()).await?;
        drop(input);
        store.wait().await
    })
    .await??;
    if !stored.success() {
        return Err(io::Error::other("the keyring refused the profile key"));
    }
    Ok(key)
}

fn parse_key(text: &[u8]) -> io::Result<Key<Aes256Gcm>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid profile key");
    if text.len() != 64 {
        return Err(invalid());
    }
    let mut key = Key::<Aes256Gcm>::default();
    for (byte, pair) in key.iter_mut().zip(text.chunks_exact(2)) {
        let digit = |c: u8| (c as char).to_digit(16).ok_or_else(invalid);
        *byte = ((digit(pair[0])? << 4) | digit(pair[1])?) as u8;
    }
    Ok(key)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with(files: &[(&str, &[u8])]) -> (tempdir::Dir, Profile) {
        let dir = tempdir::Dir::new();
        let path = dir.0.join("profile");
        fs::create_dir_all(path.join("files/mpl_db")).unwrap();
        for (name, data) in files {
            fs::write(path.join("files").join(name), data).unwrap();
        }
        let profile = Profile {
            dir: File::open(&path).unwrap(),
            path,
        };
        (dir, profile)
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);

        impl Dir {
            pub fn new() -> Self {
                let unique = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = std::env::temp_dir()
                    .join(format!("slipmat-profile-{}-{unique}", std::process::id()));
                std::fs::create_dir(&path).unwrap();
                Self(path)
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    const COMPLETE: [(&str, &[u8]); 3] = [
        ("session-ready", b"1"),
        ("mpl_db/accounts.sqlitedb", b"accounts"),
        ("adi.pb", b"\x00\xffbinary"),
    ];

    #[test]
    fn a_sealed_snapshot_restores_the_same_files() {
        let (dir, profile) = profile_with(&COMPLETE);
        let key = Aes256Gcm::generate_key(&mut OsRng);
        let sealed = seal(&key, &snapshot(&profile.dir).unwrap()).unwrap();
        let files = open_archive(&key, &sealed).unwrap();
        let restored = dir.0.join("restored");
        write_profile(&restored, &files).unwrap();
        for (name, data) in COMPLETE {
            assert_eq!(fs::read(restored.join("files").join(name)).unwrap(), data);
        }
    }

    #[test]
    fn the_archive_is_bound_to_its_version_and_key() {
        let (_dir, profile) = profile_with(&COMPLETE);
        let key = Aes256Gcm::generate_key(&mut OsRng);
        let mut sealed = seal(&key, &snapshot(&profile.dir).unwrap()).unwrap();
        assert!(open_archive(&Aes256Gcm::generate_key(&mut OsRng), &sealed).is_err());
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(open_archive(&key, &sealed).is_err());
        assert!(open_archive(&key, &[0; 4]).is_err());
    }

    #[test]
    fn an_archive_written_by_the_go_helper_opens() {
        // Go's json.Marshal of map[string][]byte: sorted keys, padded base64.
        let key = Aes256Gcm::generate_key(&mut OsRng);
        let json = br#"{"mpl_db/accounts.sqlitedb":"YWNjb3VudHM=","session-ready":"MQ==","unknown":"eA=="}"#;
        let files = open_archive(&key, &seal(&key, json).unwrap()).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(
            **files.get("mpl_db/accounts.sqlitedb").unwrap(),
            b"accounts"
        );
        assert_eq!(**files.get("session-ready").unwrap(), b"1");
    }

    #[test]
    fn an_incomplete_profile_is_not_saved() {
        let (_dir, profile) = profile_with(&[("session-ready", b"1")]);
        assert!(snapshot(&profile.dir).is_err());
    }

    #[test]
    fn a_symlinked_profile_file_is_refused() {
        let (dir, profile) = profile_with(&COMPLETE);
        fs::write(dir.0.join("outside"), b"secret").unwrap();
        std::os::unix::fs::symlink(
            dir.0.join("outside"),
            profile.path.join("files/MUSIC_TOKEN"),
        )
        .unwrap();
        assert!(snapshot(&profile.dir).is_err());
    }

    #[test]
    fn a_symlinked_directory_is_refused() {
        let (dir, profile) = profile_with(&[("session-ready", b"1")]);
        let elsewhere = dir.0.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(elsewhere.join("accounts.sqlitedb"), b"accounts").unwrap();
        fs::remove_dir(profile.path.join("files/mpl_db")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, profile.path.join("files/mpl_db")).unwrap();
        assert!(snapshot(&profile.dir).is_err());
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let (_dir, profile) = profile_with(&COMPLETE);
        let fifo =
            CString::new(profile.path.join("files/fsi.pdat").as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is NUL-terminated.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(snapshot(&profile.dir).is_err());
    }

    #[test]
    fn keys_round_trip_through_hex() {
        let key = Aes256Gcm::generate_key(&mut OsRng);
        assert_eq!(parse_key(hex(&key).as_bytes()).unwrap(), key);
        assert!(parse_key(b"zz").is_err());
        assert!(parse_key(&[b'g'; 64]).is_err());
    }
}
