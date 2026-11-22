// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The provider's own files in the profile directory, beside the ones the
//! libraries write: the device identity and the marker that a sign-in finished.
//!
//! The daemon saves and restores the directory; these names are in its
//! allowlist, so a restored profile keeps the same device and stays signed in.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

/// Apple's Android client, as the wrapper this library came from identifies it.
const DEVICE_PREFIX: &str = "Music/4.9/Android/10/Samsung S9/7663313/en-US/en-US/";
/// The ID every session used before each install had its own. A signed-in
/// profile without a `device-id` file was made with it, so it keeps it.
const LEGACY_ANDROID_ID: &str = "dc28071e981c439e";
const DEVICE_ID: &str = "device-id";
const SESSION_READY: &str = "session-ready";
const MUSIC_TOKEN: &str = "MUSIC_TOKEN";

pub fn prepare(base: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(base)
}

pub fn has_session(base: &Path) -> bool {
    base.join(SESSION_READY).is_file()
}

pub fn mark_session(base: &Path) -> io::Result<()> {
    write_private(&base.join(SESSION_READY), b"1")
}

/// A token without the marker belongs to a sign-in that never finished.
pub fn forget_token(base: &Path) {
    let _ = fs::remove_file(base.join(MUSIC_TOKEN));
}

/// This install's device string, or "" for the library's shared default when
/// the ID cannot be stored.
pub fn device_info(base: &Path) -> String {
    android_id(base)
        .map(|id| format!("{DEVICE_PREFIX}{id}"))
        .unwrap_or_default()
}

fn android_id(base: &Path) -> io::Result<String> {
    let path = base.join(DEVICE_ID);
    if let Ok(saved) = fs::read_to_string(&path)
        && is_android_id(saved.trim())
    {
        return Ok(saved.trim().to_owned());
    }
    let id = if has_session(base) {
        LEGACY_ANDROID_ID.to_owned()
    } else {
        random_id()?
    };
    let temp = path.with_extension("tmp");
    let _ = fs::remove_file(&temp);
    write_private(&temp, format!("{id}\n").as_bytes())?;
    fs::rename(&temp, &path)?;
    Ok(id)
}

fn is_android_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn random_id() -> io::Result<String> {
    let mut bytes = [0u8; 8];
    // SAFETY: `bytes` is a writable buffer of the length passed.
    let read = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if read != bytes.len() as isize {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?
        .write_all(contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("fairplay-profile-{}-{unique}", std::process::id()));
            prepare(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_new_profile_gets_its_own_device_id_and_keeps_it() {
        let dir = Dir::new();
        let first = device_info(&dir.0);
        let id = first.strip_prefix(DEVICE_PREFIX).unwrap();
        assert!(is_android_id(id));
        assert_ne!(id, LEGACY_ANDROID_ID);
        assert_eq!(device_info(&dir.0), first);
    }

    #[test]
    fn a_signed_in_profile_without_an_id_keeps_the_legacy_one() {
        let dir = Dir::new();
        mark_session(&dir.0).unwrap();
        assert_eq!(
            device_info(&dir.0),
            format!("{DEVICE_PREFIX}{LEGACY_ANDROID_ID}")
        );
    }

    #[test]
    fn a_malformed_id_is_replaced() {
        let dir = Dir::new();
        fs::write(dir.0.join(DEVICE_ID), "not-an-id\n").unwrap();
        let id = device_info(&dir.0);
        assert!(is_android_id(id.strip_prefix(DEVICE_PREFIX).unwrap()));
    }

    #[test]
    fn a_token_without_the_marker_is_forgotten() {
        let dir = Dir::new();
        fs::write(dir.0.join(MUSIC_TOKEN), "token").unwrap();
        assert!(!has_session(&dir.0));
        forget_token(&dir.0);
        assert!(!dir.0.join(MUSIC_TOKEN).exists());
    }
}
