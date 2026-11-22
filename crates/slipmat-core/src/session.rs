// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! What was playing when the app last closed.
//!
//! **State, not cache and not a preference.** It cannot be recomputed, so it
//! does not belong in the cache directory next to `unplayable.json`, which can;
//! and it is not something anybody chose, so it does not belong in
//! `settings.ini` next to the theme. `$XDG_STATE_HOME/slipmat/session.json`,
//! which is exactly what that directory is for.
//!
//! Catalog ids and song titles are not secrets, so rule 7 is not engaged —
//! there is nothing here that would matter if someone read the file.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::player::protocol::Item;

/// A queue and where in it we were.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Catalog ids, in queue order.
    pub songs: Vec<String>,
    /// The same songs with their details, because a restore runs before the
    /// library has loaded. Empty in a file written before this existed.
    #[serde(default)]
    pub items: Vec<Item>,
    /// Index into `songs` of the track that was current.
    #[serde(default)]
    pub start: usize,
    /// How far into that track. Zero is normal — it is only known accurately
    /// when the app closed cleanly.
    #[serde(default)]
    pub position_ms: u64,
}

fn path() -> Option<PathBuf> {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var("HOME").ok()?).join(".local/state"),
    };
    Some(base.join("slipmat/session.json"))
}

/// What was playing last time, if anything.
///
/// A missing or unreadable file is not an error: it means there is nothing to
/// restore, which is the normal first-run path.
pub fn load() -> Option<Session> {
    let path = path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    parse(&raw)
}

/// The part of `load` that is not I/O, so it can be tested without writing to
/// anybody's home directory.
fn parse(raw: &str) -> Option<Session> {
    match serde_json::from_str::<Session>(raw) {
        // An empty queue is nothing to restore, not an error.
        Ok(session) if !session.songs.is_empty() => Some(session),
        Ok(_) => None,
        Err(err) => {
            // A file from a future version, or a truncated write. Say so once
            // and carry on without it rather than refusing to start.
            tracing::warn!(?err, "could not read the saved session");
            None
        }
    }
}

/// Remember a queue. Best-effort: failing to save must never interrupt
/// playback, and the cost of losing it is one restore.
pub fn save(session: &Session) {
    let Some(path) = path() else { return };
    match serde_json::to_string(session) {
        Ok(json) => {
            if let Err(err) = crate::artwork::write_atomically(&path, json.as_bytes()) {
                tracing::warn!(?err, "could not save the session");
            }
        }
        Err(err) => tracing::warn!(?err, "could not serialise the session"),
    }
}

/// Forget it — nothing is loaded, or the user signed out.
pub fn clear() {
    if let Some(path) = path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_survives_the_round_trip() {
        let item = Item {
            occurrence_id: "q:7".into(),
            catalog_id: Some("1440857781".into()),
            title: "Roundabout".into(),
            artist: "Yes".into(),
            duration_ms: 510_000,
            ..Item::default()
        };
        let saved = Session {
            songs: vec!["1440857781".into()],
            items: vec![item.clone()],
            start: 0,
            position_ms: 42_000,
        };
        let back = parse(&serde_json::to_string(&saved).unwrap()).unwrap();
        assert_eq!(back.songs, saved.songs);
        assert_eq!(back.start, 0);
        assert_eq!(back.position_ms, 42_000);
        // The details come back; the occurrence id belongs to one process.
        assert_eq!(
            back.items,
            vec![Item {
                occurrence_id: String::new(),
                ..item
            }]
        );
    }

    #[test]
    fn an_empty_queue_is_nothing_to_restore() {
        assert!(parse(r#"{"songs":[],"start":0,"position_ms":0}"#).is_none());
    }

    #[test]
    fn a_file_we_cannot_read_is_survivable() {
        // A truncated write, or a format from a future version. Refusing to
        // start over a state file would be absurd.
        assert!(parse("").is_none());
        assert!(parse("{").is_none());
        assert!(parse(r#"{"songs":"not a list"}"#).is_none());
    }

    #[test]
    fn the_optional_fields_default() {
        // Written by an older version, or by hand.
        let s = parse(r#"{"songs":["1"]}"#).unwrap();
        assert_eq!(s.start, 0);
        assert_eq!(s.position_ms, 0);
        assert!(s.items.is_empty());
    }
}
