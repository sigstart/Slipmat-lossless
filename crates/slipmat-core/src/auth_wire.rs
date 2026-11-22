// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The daemon's sign-in socket. The provider protocol is `fairplay-wire`, whose
//! framing and `Secret` this reuses.

use std::io;
use std::path::PathBuf;

pub use fairplay_wire::{Reader, Secret, write};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::net::UnixStream;

pub const FIRST_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Request {
    Begin,
    Credentials { account: Secret, password: Secret },
    VerificationCode { code: Secret },
    Status,
    SignOut,
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
        }

        let Fields {
            r#type,
            account,
            password,
            code,
        } = Fields::deserialize(deserializer)?;
        match (r#type, account, password, code) {
            (Kind::Begin, None, None, None) => Ok(Self::Begin),
            (Kind::Credentials, Some(account), Some(password), None) => {
                Ok(Self::Credentials { account, password })
            }
            (Kind::VerificationCode, None, None, Some(code)) => Ok(Self::VerificationCode { code }),
            (Kind::Status, None, None, None) => Ok(Self::Status),
            (Kind::SignOut, None, None, None) => Ok(Self::SignOut),
            _ => Err(serde::de::Error::custom("invalid sign-in request fields")),
        }
    }
}

pub fn socket_path() -> io::Result<PathBuf> {
    Ok(crate::paths::runtime_dir()?.join("slipmat-auth.sock"))
}

pub fn verify_peer(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() != crate::paths::effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sign-in peer has the wrong user ID",
        ));
    }
    Ok(())
}
