// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The protocol between `slipmatd` and the sandboxed FairPlay provider.
//!
//! One JSON object per line over the provider's stdio, tagged by a camel-case
//! `type`. The daemon sends [`Request`]s and the provider answers with
//! [`Reply`]s. Credentials and catalogue tokens cross this pipe and no other,
//! so frames are bounded, buffers are zeroised, and secrets never `Debug`.
//!
//! The daemon's client socket reuses the framing and [`Secret`] from here, so
//! the two sides of the daemon cannot disagree about either.

use std::fmt;
use std::io::{self, Write};

use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

pub const MAX_FRAME: usize = 8192;

/// Daemon → provider.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Request {
    Begin,
    Credentials { account: Secret, password: Secret },
    VerificationCode { code: Secret },
    PlayTrack { track_id: String },
    Pause,
    Resume,
    Stop,
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Kind {
            Begin,
            Credentials,
            VerificationCode,
            PlayTrack,
            Pause,
            Resume,
            Stop,
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
            (Kind::PlayTrack, None, None, None, Some(track_id)) => Ok(Self::PlayTrack { track_id }),
            (Kind::Pause, None, None, None, None) => Ok(Self::Pause),
            (Kind::Resume, None, None, None, None) => Ok(Self::Resume),
            (Kind::Stop, None, None, None, None) => Ok(Self::Stop),
            _ => Err(serde::de::Error::custom("invalid provider request fields")),
        }
    }
}

/// Provider → daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Reply {
    CredentialsRequired,
    VerificationRequired,
    VerificationUnsupported,
    Rejected,
    Failed,
    SignedIn {
        catalogue: Catalogue,
    },
    Playing,
    Paused,
    Position {
        position_ms: u64,
    },
    Stopped,
    Played {
        sample_rate: u32,
        bits_per_sample: u8,
        channels: usize,
    },
    LicenceDenied,
    PlaybackFailed {
        stage: PlaybackStage,
    },
}

/// The catalogue credentials a signed-in provider hands the daemon.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Catalogue {
    pub developer_token: String,
    pub music_user_token: String,
    pub storefront: String,
    pub authorized: bool,
}

impl fmt::Debug for Catalogue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Catalogue")
            .field("storefront", &self.storefront)
            .field("authorized", &self.authorized)
            .finish_non_exhaustive()
    }
}

/// Redacted diagnostics for a track the provider could not play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlaybackStage {
    Session,
    Storefront,
    Catalogue,
    CatalogueAuthorization,
    CatalogueTrack,
    CatalogueAsset,
    Stream,
    Output,
}

/// A credential or code, zeroised on drop and never printed.
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

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

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

pub struct Reader<R> {
    io: R,
    bytes: Zeroizing<Vec<u8>>,
    end: usize,
    scanned: usize,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(io: R) -> Self {
        Self::with_limit(io, MAX_FRAME)
    }

    pub fn with_limit(io: R, limit: usize) -> Self {
        Self {
            io,
            bytes: Zeroizing::new(vec![0; limit]),
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
            if self.end == self.bytes.len() {
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
    io::Error::new(io::ErrorKind::InvalidData, "invalid frame")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn catalogue() -> Catalogue {
        Catalogue {
            developer_token: "dev-secret".into(),
            music_user_token: "user-secret".into(),
            storefront: "za".into(),
            authorized: true,
        }
    }

    fn request(value: Value) -> Result<Request, serde_json::Error> {
        serde_json::from_value(value)
    }

    #[test]
    fn requests_round_trip_with_the_go_providers_field_names() {
        let frames = [
            json!({"type": "begin"}),
            json!({"type": "credentials", "account": "a@example.invalid", "password": "pw"}),
            json!({"type": "verificationCode", "code": "123456"}),
            json!({"type": "playTrack", "track_id": "123"}),
            json!({"type": "pause"}),
            json!({"type": "resume"}),
            json!({"type": "stop"}),
        ];
        for frame in frames {
            let parsed = request(frame.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), frame);
        }
    }

    #[test]
    fn requests_with_misplaced_fields_are_refused() {
        assert!(request(json!({"type": "begin", "code": "123456"})).is_err());
        assert!(request(json!({"type": "credentials", "account": "a"})).is_err());
        assert!(request(json!({"type": "playTrack"})).is_err());
        assert!(request(json!({"type": "status"})).is_err());
        assert!(request(json!({"type": "pause", "extra": 1})).is_err());
    }

    #[test]
    fn replies_round_trip() {
        let replies = [
            Reply::CredentialsRequired,
            Reply::VerificationRequired,
            Reply::VerificationUnsupported,
            Reply::Rejected,
            Reply::Failed,
            Reply::SignedIn {
                catalogue: catalogue(),
            },
            Reply::Playing,
            Reply::Paused,
            Reply::Position { position_ms: 1250 },
            Reply::Stopped,
            Reply::Played {
                sample_rate: 44100,
                bits_per_sample: 24,
                channels: 2,
            },
            Reply::LicenceDenied,
            Reply::PlaybackFailed {
                stage: PlaybackStage::CatalogueAuthorization,
            },
        ];
        for reply in replies {
            let text = serde_json::to_string(&reply).unwrap();
            assert_eq!(serde_json::from_str::<Reply>(&text).unwrap(), reply);
        }
    }

    #[test]
    fn signed_in_matches_the_go_providers_frame() {
        let frame = json!({
            "type": "signedIn",
            "catalogue": {
                "developerToken": "dev-secret",
                "musicUserToken": "user-secret",
                "storefront": "za",
                "authorized": true,
            },
        });
        assert_eq!(
            serde_json::from_value::<Reply>(frame.clone()).unwrap(),
            Reply::SignedIn {
                catalogue: catalogue()
            }
        );
        assert!(serde_json::from_value::<Reply>(json!({"type": "signedIn"})).is_err());
        assert_eq!(
            serde_json::to_value(Reply::PlaybackFailed {
                stage: PlaybackStage::Stream
            })
            .unwrap(),
            json!({"type": "playbackFailed", "stage": "stream"})
        );
    }

    #[test]
    fn secrets_and_tokens_stay_out_of_debug_output() {
        let printed = format!(
            "{:?} {:?}",
            Secret::from("hunter2".to_owned()),
            Reply::SignedIn {
                catalogue: catalogue()
            }
        );
        for secret in ["hunter2", "dev-secret", "user-secret"] {
            assert!(!printed.contains(secret), "{printed}");
        }
    }

    #[tokio::test]
    async fn frames_split_across_reads_and_oversized_frames_fail() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut reader = Reader::new(server);
        client.write_all(b"{\"type\":").await.unwrap();
        client.write_all(b"\"pause\"}\n").await.unwrap();
        assert!(matches!(
            reader.frame::<Request>().await.unwrap(),
            Some(Request::Pause)
        ));
        drop(client);
        assert!(reader.frame::<Request>().await.unwrap().is_none());

        let (mut client, server) = tokio::io::duplex(64);
        let mut reader = Reader::with_limit(server, 16);
        client.write_all(&[b'x'; 32]).await.unwrap();
        assert!(reader.frame::<Request>().await.is_err());
    }
}
