// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-FileCopyrightText: 2026 Jan Vermeulen
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
    Credentials {
        account: Secret,
        password: Secret,
    },
    VerificationCode {
        code: Secret,
    },
    PlayTrack {
        track_id: String,
    },
    /// Download, decrypt and decode a track with no audio output, as fast as
    /// it arrives, to find out whether it plays through.
    CheckTrack {
        track_id: String,
    },
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
            CheckTrack,
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
            (Kind::CheckTrack, None, None, None, Some(track_id)) => {
                Ok(Self::CheckTrack { track_id })
            }
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
    /// Which stream was chosen, once per track, before it plays.
    Streaming {
        codec: StreamCodec,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fault: Option<PlaybackFault>,
        /// Detail for the log, boxed apart from `fault` so the fault itself
        /// stays small enough to return by value on every fragment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnosis: Option<Box<Diagnosis>>,
    },
}

/// What a track is streamed as. Lossless when Apple has a lossless master;
/// otherwise the best AAC it offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamCodec {
    Alac,
    Aac,
}

impl StreamCodec {
    pub fn is_lossless(self) -> bool {
        self == Self::Alac
    }
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

/// What went wrong inside a stage, as codes rather than text: nothing from
/// the network or the library crosses the pipe as prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum PlaybackFault {
    /// The media playlists could not be fetched or read.
    Playlist,
    /// The shared prefetch key would not open.
    PrefetchKey,
    /// The thread that owns the FairPlay library stopped answering.
    Worker,
    /// Nothing arrived for the stall timeout, and resuming did not help.
    Stalled,
    /// The download broke off, and resuming did not help.
    Dropped,
    /// The CDN answered with an unexpected status, first time or on a resume.
    Refused { status: u16, resume: bool },
    /// The file is not the fragmented MP4 its playlist described.
    Container,
    /// A lease refresh freed the track key and it would not reopen.
    KeyReopen,
    /// FairPlay refused a sample, with the library's status code.
    Decrypt { status: i32 },
    /// The clear stream did not decode as ALAC: which step failed and after
    /// how much audio. With how many later key tags the playlist had that were
    /// skipped, and when the first began: a failure after that point is the
    /// wrong key, not a broken decoder. Every field is defaulted, so a
    /// provider built before any of them still parses.
    Decoder {
        #[serde(default)]
        step: Option<DecodeStep>,
        #[serde(default)]
        decoded_ms: Option<u64>,
        #[serde(default)]
        ignored_keys: u16,
        #[serde(default)]
        first_ignored_at_ms: Option<u64>,
    },
    /// The audio server refused or dropped the output stream.
    AudioServer,
    /// The song has no lossless master, and its AAC needs Google's Widevine
    /// CDM, which is not installed. A client can offer to install it.
    WidevineMissing,
    /// The Widevine CDM would not start, or stopped answering.
    Widevine,
}

/// What the provider saw of a file that would not decode: the fragment that
/// failed beside the first, and the init segment's two sample descriptions,
/// of which only the first is decoded with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Diagnosis {
    #[serde(default)]
    pub fragment: Option<FragmentShape>,
    #[serde(default)]
    pub first_fragment: Option<FragmentShape>,
    #[serde(default)]
    pub first_config: Option<AlacConfig>,
    #[serde(default)]
    pub second_config: Option<AlacConfig>,
    #[serde(default)]
    pub first_protection: Option<Protection>,
    #[serde(default)]
    pub second_protection: Option<Protection>,
    /// Where the playlist changes key, honouring every tag, up to
    /// [`MAX_CHANGES`]. The provider decrypts with the first two keys only.
    #[serde(default)]
    pub key_changes: Vec<KeyChange>,
    /// Where the fragments fed so far change sample description, likewise.
    #[serde(default)]
    pub description_changes: Vec<DescriptionChange>,
}

/// How many changes a [`Diagnosis`] lists, so it fits in one frame.
pub const MAX_CHANGES: usize = 32;

/// A sample description's encryption, as hex. Key ids and IVs travel in the
/// file in the clear; the key itself never leaves the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Protection {
    pub key_id: String,
    pub iv_size: u8,
    #[serde(default)]
    pub constant_iv: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KeyChange {
    pub segment: u32,
    pub key: NamedKey,
}

/// Which key a playlist tag names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NamedKey {
    Prefetch,
    Track,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DescriptionChange {
    pub segment: u32,
    /// 0 is the track's default.
    pub description: u32,
}

/// Which part of decoding failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DecodeStep {
    /// The MP4 around the audio could not be read.
    Container,
    /// An ALAC frame did not decode.
    Audio,
    /// The demuxer ran out of audio before the stream ended.
    EndedEarly,
}

/// How one fragment of the file was laid out and encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FragmentShape {
    /// Its media segment in the playlist, counting from zero.
    pub index: u32,
    pub starts_at_ms: u64,
    pub samples: u32,
    /// Samples encrypted in more than one piece.
    pub split_samples: u32,
    pub tfhd_flags: u32,
    /// Every `trun` box's flags, combined.
    pub trun_flags: u32,
    /// The sample description it names, or 0 for the track's default.
    #[serde(default)]
    pub description: u32,
    /// How old the key context was that decrypted it.
    #[serde(default)]
    pub key_age_ms: Option<u64>,
}

/// The parts of an ALAC magic cookie that decide whether a frame decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AlacConfig {
    pub frame_length: u32,
    pub bit_depth: u8,
    pub channels: u8,
    pub max_frame_bytes: u32,
    pub sample_rate: u32,
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
            json!({"type": "checkTrack", "track_id": "123"}),
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
        assert!(request(json!({"type": "checkTrack"})).is_err());
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
            Reply::Streaming {
                codec: StreamCodec::Aac,
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
                fault: None,
                diagnosis: None,
            },
            Reply::PlaybackFailed {
                stage: PlaybackStage::Stream,
                fault: Some(PlaybackFault::Refused {
                    status: 403,
                    resume: true,
                }),
                diagnosis: None,
            },
            Reply::PlaybackFailed {
                stage: PlaybackStage::Stream,
                fault: Some(PlaybackFault::Decrypt { status: -2 }),
                diagnosis: None,
            },
        ];
        for reply in replies {
            let text = serde_json::to_string(&reply).unwrap();
            assert_eq!(serde_json::from_str::<Reply>(&text).unwrap(), reply);
        }
    }

    #[test]
    fn a_decoder_fault_from_a_provider_without_key_counts_still_parses() {
        let frame = json!({
            "type": "playbackFailed",
            "stage": "output",
            "fault": {"kind": "decoder"},
        });
        assert_eq!(
            serde_json::from_value::<Reply>(frame).unwrap(),
            Reply::PlaybackFailed {
                stage: PlaybackStage::Output,
                fault: Some(PlaybackFault::Decoder {
                    step: None,
                    decoded_ms: None,
                    ignored_keys: 0,
                    first_ignored_at_ms: None
                }),
                diagnosis: None,
            }
        );
    }

    #[test]
    fn a_full_decoder_fault_round_trips() {
        let shape = |index, starts_at_ms| FragmentShape {
            index,
            starts_at_ms,
            samples: 161,
            split_samples: 0,
            tfhd_flags: 0x2_0000,
            trun_flags: 0x201,
            description: 2,
            key_age_ms: Some(912_000),
        };
        let config = AlacConfig {
            frame_length: 4096,
            bit_depth: 24,
            channels: 2,
            max_frame_bytes: 0,
            sample_rate: 44_100,
        };
        let reply = Reply::PlaybackFailed {
            stage: PlaybackStage::Output,
            fault: Some(PlaybackFault::Decoder {
                step: Some(DecodeStep::Audio),
                decoded_ms: Some(987_012),
                ignored_keys: 0,
                first_ignored_at_ms: None,
            }),
            diagnosis: Some(Box::new(Diagnosis {
                fragment: Some(shape(66, 986_940)),
                first_fragment: Some(shape(0, 0)),
                first_config: Some(config),
                second_config: Some(AlacConfig {
                    bit_depth: 16,
                    ..config
                }),
                first_protection: Some(Protection {
                    key_id: "11".repeat(16),
                    iv_size: 0,
                    constant_iv: Some("22".repeat(16)),
                }),
                second_protection: None,
                key_changes: (0..MAX_CHANGES as u32)
                    .map(|segment| KeyChange {
                        segment,
                        key: NamedKey::Prefetch,
                    })
                    .collect(),
                description_changes: (0..MAX_CHANGES as u32)
                    .map(|segment| DescriptionChange {
                        segment,
                        description: 2,
                    })
                    .collect(),
            })),
        };
        let text = serde_json::to_string(&reply).unwrap();
        assert!(text.len() < MAX_FRAME);
        assert_eq!(serde_json::from_str::<Reply>(&text).unwrap(), reply);
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
                stage: PlaybackStage::Stream,
                fault: None,
                diagnosis: None,
            })
            .unwrap(),
            json!({"type": "playbackFailed", "stage": "stream"})
        );
        assert_eq!(
            serde_json::to_value(Reply::PlaybackFailed {
                stage: PlaybackStage::Stream,
                fault: Some(PlaybackFault::Dropped),
                diagnosis: None,
            })
            .unwrap(),
            json!({"type": "playbackFailed", "stage": "stream", "fault": {"kind": "dropped"}})
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
