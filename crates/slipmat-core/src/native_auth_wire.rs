// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Internal native sign-in socket and provider stdio protocol.

use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use zeroize::{Zeroize, Zeroizing};

const MAX_FRAME: usize = 8192;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Request {
    Begin,
    Credentials { account: Secret, password: Secret },
    VerificationCode { code: Secret },
    Status,
    SignOut,
    PlayTrack { track_id: String },
}

/// Playback requests sent over provider stdio.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PlaybackRequest {
    PlayTrack { track_id: String },
    Pause,
    Resume,
    Stop,
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Kind {
            Begin,
            Credentials,
            VerificationCode,
            Status,
            SignOut,
            PlayTrack,
        }

        // Deserialize secrets directly into their wrappers. A tagged enum
        // would buffer arbitrary fields in Serde before validating the tag.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            r#type: Kind,
            account: Option<Secret>,
            password: Option<Secret>,
            code: Option<Secret>,
            track_id: Option<String>,
        }

        let Fields {
            r#type,
            account,
            password,
            code,
            track_id,
        } = Fields::deserialize(deserializer)?;
        match (r#type, account, password, code, track_id) {
            (Kind::Begin, None, None, None, None) => Ok(Self::Begin),
            (Kind::Credentials, Some(account), Some(password), None, None) => {
                Ok(Self::Credentials { account, password })
            }
            (Kind::VerificationCode, None, None, Some(code), None) => {
                Ok(Self::VerificationCode { code })
            }
            (Kind::Status, None, None, None, None) => Ok(Self::Status),
            (Kind::SignOut, None, None, None, None) => Ok(Self::SignOut),
            (Kind::PlayTrack, None, None, None, Some(track_id)) => Ok(Self::PlayTrack { track_id }),
            _ => Err(serde::de::Error::custom(
                "invalid native sign-in request fields",
            )),
        }
    }
}

pub fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments and has no failure mode.
    unsafe { libc::geteuid() }
}

pub fn socket_path() -> io::Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    if !dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "XDG_RUNTIME_DIR must be absolute",
        ));
    }
    let metadata = std::fs::symlink_metadata(&dir)?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "XDG_RUNTIME_DIR must be a private directory owned by the effective user",
        ));
    }
    Ok(dir.join("slipmat-auth.sock"))
}

pub fn verify_peer(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "native sign-in peer has the wrong user ID",
        ));
    }
    Ok(())
}

pub struct Reader<R> {
    io: R,
    bytes: Zeroizing<Vec<u8>>,
    end: usize,
    scanned: usize,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(io: R) -> Self {
        Self {
            io,
            bytes: Zeroizing::new(vec![0; MAX_FRAME]),
            end: 0,
            scanned: 0,
        }
    }

    /// Preserve a partial frame across cancelled reads and erase consumed bytes.
    pub async fn frame<T: DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
        loop {
            if let Some(offset) = self.bytes[self.scanned..self.end]
                .iter()
                .position(|&byte| byte == b'\n')
            {
                let newline = self.scanned + offset;
                let result = serde_json::from_slice(&self.bytes[..newline]);
                let remaining = self.end - newline - 1;
                self.bytes.copy_within(newline + 1..self.end, 0);
                self.bytes[remaining..self.end].zeroize();
                self.end = remaining;
                self.scanned = 0;
                return result.map(Some).map_err(|_| invalid_frame());
            }
            if self.end == MAX_FRAME {
                return Err(invalid_frame());
            }
            self.scanned = self.end;
            let read = self.io.read(&mut self.bytes[self.end..]).await?;
            if read == 0 {
                return if self.end == 0 {
                    Ok(None)
                } else {
                    Err(invalid_frame())
                };
            }
            self.end += read;
        }
    }
}

pub async fn write<W: AsyncWrite + Unpin>(io: &mut W, value: impl Serialize) -> io::Result<()> {
    let mut frame = Frame(Zeroizing::new(Vec::with_capacity(MAX_FRAME)));
    serde_json::to_writer(&mut frame, &value).map_err(|_| invalid_frame())?;
    drop(value);
    frame.0.push(b'\n');
    io.write_all(&frame.0).await
}

struct Frame(Zeroizing<Vec<u8>>);

impl Write for Frame {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() >= MAX_FRAME - self.0.len() {
            return Err(invalid_frame());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn invalid_frame() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid native sign-in frame")
}
