// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Client for the daemon's sign-in endpoint, and the replies the daemon passes
//! on from the provider.

use std::io;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::{UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf};
use tokio::time::timeout;

use crate::auth_wire::{self as wire, Reader, Request};
pub use fairplay_wire::{DecodeStep, FragmentShape, PlaybackFault, PlaybackStage};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(130);

/// The result of one sign-in endpoint request, or of one track on the
/// provider's stdio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Reply {
    CredentialsRequired,
    VerificationRequired,
    VerificationUnsupported,
    SignedIn,
    SignedOut,
    Busy,
    Rejected,
    Expired,
    Failed,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fault: Option<PlaybackFault>,
    },
}

impl Reply {
    pub fn is_playback_update(self) -> bool {
        matches!(self, Self::Playing | Self::Paused | Self::Position { .. })
    }
}

impl From<fairplay_wire::Reply> for Reply {
    /// The catalogue in `signedIn` is the daemon's to keep, so it is dropped here.
    fn from(reply: fairplay_wire::Reply) -> Self {
        use fairplay_wire::Reply as Provider;
        match reply {
            Provider::CredentialsRequired => Self::CredentialsRequired,
            Provider::VerificationRequired => Self::VerificationRequired,
            Provider::VerificationUnsupported => Self::VerificationUnsupported,
            Provider::Rejected => Self::Rejected,
            Provider::Failed => Self::Failed,
            Provider::SignedIn { .. } => Self::SignedIn,
            Provider::Playing => Self::Playing,
            Provider::Paused => Self::Paused,
            Provider::Position { position_ms } => Self::Position { position_ms },
            Provider::Stopped => Self::Stopped,
            Provider::Played {
                sample_rate,
                bits_per_sample,
                channels,
            } => Self::Played {
                sample_rate,
                bits_per_sample,
                channels,
            },
            Provider::LicenceDenied => Self::LicenceDenied,
            Provider::PlaybackFailed { stage, fault, .. } => Self::PlaybackFailed { stage, fault },
        }
    }
}

/// Sign-in is tied to this connection until it succeeds. Drop the client to
/// cancel an ongoing sign-in. An interrupted or failed exchange closes the
/// connection.
pub struct Client {
    connection: Option<(Reader<OwnedReadHalf>, OwnedWriteHalf)>,
}

impl Client {
    /// Connect to an already running daemon with sign-in enabled.
    pub async fn connect() -> io::Result<Self> {
        let path = wire::socket_path()?;
        let stream = timeout(Duration::from_secs(5), UnixStream::connect(path)).await??;
        wire::verify_peer(&stream)?;
        let (read, write) = stream.into_split();
        Ok(Self {
            connection: Some((Reader::new(read), write)),
        })
    }

    pub async fn begin(&mut self) -> io::Result<Reply> {
        self.request(Request::Begin).await
    }

    pub async fn credentials(&mut self, account: String, password: String) -> io::Result<Reply> {
        self.request(Request::Credentials {
            account: account.into(),
            password: password.into(),
        })
        .await
    }

    pub async fn verification_code(&mut self, code: String) -> io::Result<Reply> {
        self.request(Request::VerificationCode { code: code.into() })
            .await
    }

    pub async fn status(&mut self) -> io::Result<Reply> {
        self.request(Request::Status).await
    }

    pub async fn sign_out(&mut self) -> io::Result<Reply> {
        self.request(Request::SignOut).await
    }

    async fn request(&mut self, request: Request) -> io::Result<Reply> {
        // A cancelled future may have transmitted part of a request. Close
        // that connection so its next reply cannot be mistaken for another.
        let (mut read, mut write) = self.connection.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "sign-in connection closed")
        })?;
        let reply = timeout(RESPONSE_TIMEOUT, async {
            wire::write(&mut write, request).await?;
            read.frame().await?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "sign-in connection closed")
            })
        })
        .await??;
        self.connection = Some((read, write));
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fairplay_wire::Request as ProviderRequest;
    use serde_json::json;

    #[test]
    fn playback_wire_uses_camel_case_tags_and_snake_case_fields() {
        assert_eq!(
            serde_json::to_value(Reply::Played {
                sample_rate: 44100,
                bits_per_sample: 24,
                channels: 2,
            })
            .unwrap(),
            json!({"type": "played", "sample_rate": 44100, "bits_per_sample": 24, "channels": 2})
        );
        assert_eq!(
            serde_json::to_value(Reply::LicenceDenied).unwrap(),
            json!({"type": "licenceDenied"})
        );
    }

    #[test]
    fn playback_controls_belong_to_worker_stdio() {
        for (command, name) in [
            (ProviderRequest::Pause, "pause"),
            (ProviderRequest::Resume, "resume"),
            (ProviderRequest::Stop, "stop"),
        ] {
            let frame = json!({"type": name});
            assert_eq!(serde_json::to_value(command).unwrap(), frame);
            assert!(serde_json::from_value::<Request>(frame).is_err());
        }
        let play = json!({"type": "playTrack", "track_id": "123"});
        assert_eq!(
            serde_json::to_value(ProviderRequest::PlayTrack {
                track_id: "123".to_owned(),
            })
            .unwrap(),
            play
        );
        assert!(serde_json::from_value::<Request>(play).is_err());
        for (reply, frame) in [
            (Reply::Playing, json!({"type": "playing"})),
            (Reply::Paused, json!({"type": "paused"})),
            (
                Reply::Position { position_ms: 42 },
                json!({"type": "position", "position_ms": 42}),
            ),
            (Reply::Stopped, json!({"type": "stopped"})),
        ] {
            assert_eq!(serde_json::to_value(reply).unwrap(), frame);
            assert_eq!(serde_json::from_value::<Reply>(frame).unwrap(), reply);
        }
    }

    #[test]
    fn playback_diagnostics_accept_only_named_stages() {
        for (name, stage) in [
            ("session", PlaybackStage::Session),
            ("catalogue", PlaybackStage::Catalogue),
            ("stream", PlaybackStage::Stream),
            ("output", PlaybackStage::Output),
        ] {
            let frame = json!({"type": "playbackFailed", "stage": name});
            let reply = Reply::PlaybackFailed { stage, fault: None };
            assert_eq!(serde_json::to_value(reply).unwrap(), frame);
            assert_eq!(serde_json::from_value::<Reply>(frame).unwrap(), reply);
        }
        let frame = json!({
            "type": "playbackFailed",
            "stage": "stream",
            "fault": {"kind": "refused", "status": 403, "resume": true},
        });
        assert_eq!(
            serde_json::from_value::<Reply>(frame).unwrap(),
            Reply::PlaybackFailed {
                stage: PlaybackStage::Stream,
                fault: Some(PlaybackFault::Refused {
                    status: 403,
                    resume: true
                }),
            }
        );
        for frame in [
            json!({"type": "playbackFailed", "stage": "custom"}),
            json!({"type": "playbackFailed", "stage": "output", "details": "fixture"}),
            json!({"type": "playbackFailed", "stage": "stream", "fault": {"kind": "custom"}}),
            json!({
                "type": "playbackFailed",
                "stage": "stream",
                "fault": {"kind": "refused", "status": 403, "resume": true, "text": "fixture"},
            }),
            json!({"type": "trialFailed", "stage": "output"}),
        ] {
            assert!(serde_json::from_value::<Reply>(frame).is_err());
        }
    }
}
