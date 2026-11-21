// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Client for the daemon's native sign-in and playback trial endpoint.

use std::io;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::{UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf};
use tokio::time::timeout;

use crate::native_auth_wire::{self as wire, Reader, Request};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(130);

/// The result of one native endpoint request.
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
    TrialFailed {
        stage: TrialStage,
    },
}

impl Reply {
    pub fn is_playback_update(self) -> bool {
        matches!(self, Self::Playing | Self::Paused | Self::Position { .. })
    }
}

/// Redacted diagnostics for a failed playback trial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrialStage {
    Session,
    Storefront,
    Catalogue,
    CatalogueAuthorization,
    CatalogueTrack,
    CatalogueAsset,
    Stream,
    Output,
}

/// Sign-in is tied to this connection until it succeeds. Drop the client to
/// cancel an ongoing sign-in or playback trial. An interrupted or failed
/// exchange closes the connection.
pub struct Client {
    connection: Option<(Reader<OwnedReadHalf>, OwnedWriteHalf)>,
}

impl Client {
    /// Connect to an already running daemon with native sign-in enabled.
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

    /// Wait for the provider's terminal result for one catalog track.
    pub async fn play_track(&mut self, track_id: String) -> io::Result<Reply> {
        self.request(Request::PlayTrack { track_id }).await
    }

    async fn request(&mut self, request: Request) -> io::Result<Reply> {
        // A cancelled future may have transmitted part of a request. Close
        // that connection so its next reply cannot be mistaken for another.
        let (mut read, mut write) = self.connection.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "native sign-in connection closed",
            )
        })?;
        let playback = matches!(request, Request::PlayTrack { .. });
        let exchange = async {
            wire::write(&mut write, request).await?;
            loop {
                let reply: Reply = read.frame().await?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "native sign-in connection closed",
                    )
                })?;
                if !playback || !reply.is_playback_update() {
                    return Ok::<_, io::Error>(reply);
                }
            }
        };
        let reply = if playback {
            exchange.await?
        } else {
            timeout(RESPONSE_TIMEOUT, exchange).await??
        };
        self.connection = Some((read, write));
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_auth_wire::PlaybackRequest;
    use serde_json::json;

    #[test]
    fn playback_wire_uses_camel_case_tags_and_snake_case_fields() {
        let request = Request::PlayTrack {
            track_id: "123".to_owned(),
        };
        let frame = json!({"type": "playTrack", "track_id": "123"});
        assert_eq!(serde_json::to_value(request).unwrap(), frame);
        assert!(matches!(
            serde_json::from_value::<Request>(frame).unwrap(),
            Request::PlayTrack { track_id } if track_id == "123"
        ));
        for frame in [
            json!({"type": "playTrack", "trackId": "123"}),
            json!({"type": "playTrack", "track_id": 123}),
            json!({"type": "playTrack", "track_id": "123", "password": "fixture"}),
        ] {
            assert!(serde_json::from_value::<Request>(frame).is_err());
        }
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
            (PlaybackRequest::Pause, "pause"),
            (PlaybackRequest::Resume, "resume"),
            (PlaybackRequest::Stop, "stop"),
        ] {
            let frame = json!({"type": name});
            assert_eq!(serde_json::to_value(command).unwrap(), frame);
            assert!(serde_json::from_value::<Request>(frame).is_err());
        }
        assert_eq!(
            serde_json::to_value(PlaybackRequest::PlayTrack {
                track_id: "123".to_owned(),
            })
            .unwrap(),
            json!({"type": "playTrack", "track_id": "123"})
        );
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

    #[tokio::test]
    async fn manual_client_waits_for_the_terminal_reply() {
        let (client, server) = UnixStream::pair().unwrap();
        let (read, write) = client.into_split();
        let mut client = Client {
            connection: Some((Reader::new(read), write)),
        };
        let (read, mut write) = server.into_split();
        let mut read = Reader::new(read);
        let server = async {
            assert!(matches!(
                read.frame::<Request>().await.unwrap(),
                Some(Request::PlayTrack { track_id }) if track_id == "123"
            ));
            for reply in [
                Reply::Playing,
                Reply::Position { position_ms: 42 },
                Reply::Paused,
                Reply::Stopped,
            ] {
                wire::write(&mut write, reply).await.unwrap();
            }
            assert!(matches!(
                read.frame::<Request>().await.unwrap(),
                Some(Request::Status)
            ));
            wire::write(&mut write, Reply::SignedIn).await.unwrap();
        };
        timeout(Duration::from_secs(5), async {
            tokio::join!(server, async {
                assert_eq!(
                    client.play_track("123".to_owned()).await.unwrap(),
                    Reply::Stopped
                );
                assert_eq!(client.status().await.unwrap(), Reply::SignedIn);
            });
        })
        .await
        .unwrap();
    }

    #[test]
    fn playback_diagnostics_accept_only_named_stages() {
        for (name, stage) in [
            ("session", TrialStage::Session),
            ("catalogue", TrialStage::Catalogue),
            ("stream", TrialStage::Stream),
            ("output", TrialStage::Output),
        ] {
            let frame = json!({"type": "trialFailed", "stage": name});
            let reply = Reply::TrialFailed { stage };
            assert_eq!(serde_json::to_value(reply).unwrap(), frame);
            assert_eq!(serde_json::from_value::<Reply>(frame).unwrap(), reply);
        }
        for frame in [
            json!({"type": "trialFailed", "stage": "custom"}),
            json!({"type": "trialFailed", "stage": "output", "details": "fixture"}),
        ] {
            assert!(serde_json::from_value::<Reply>(frame).is_err());
        }
    }
}
