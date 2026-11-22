// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Queue commands, playback state, and catalogue credentials.

use serde::{Deserialize, Serialize};

/// Commands for the daemon's queue.
#[derive(Debug, Clone)]
pub enum Command {
    /// Items rather than ids, resolved by whoever sends it: a restore knows
    /// details that nothing in the daemon does yet.
    SetQueue {
        items: Vec<Item>,
        start_position: usize,
        start_playing: bool,
    },
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    ChangeToIndex {
        index: usize,
    },
    MoveInQueue {
        from: usize,
        to: usize,
    },
    PlayNext {
        songs: Vec<String>,
    },
    PlayLater {
        songs: Vec<String>,
    },
    ClearQueue,
    RemoveFromQueue {
        index: usize,
    },
    /// Stop whatever is playing and decode this track to nowhere, to see
    /// whether it plays through. The queue stays where it was.
    Check {
        id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    #[default]
    None,
    One,
    All,
}

/// Playback events, folded into `PlayerState` by `apply()`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    #[serde(rename = "playbackState")]
    PlaybackState { state: PlaybackState },

    #[serde(rename = "nowPlaying")]
    NowPlaying { item: Option<Item>, queue: Queue },

    #[serde(rename = "position", rename_all = "camelCase")]
    Position {
        #[serde(deserialize_with = "ms")]
        position_ms: u64,
        #[serde(deserialize_with = "ms")]
        duration_ms: u64,
    },

    #[serde(rename = "queue")]
    Queue(Queue),
}

/// Never logged, never written to `settings.ini` (rule 7).
#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    pub developer_token: String,
    pub music_user_token: Option<String>,
    #[serde(default = "default_storefront")]
    pub storefront: String,
    #[serde(default)]
    pub authorized: bool,
}

fn default_storefront() -> String {
    "us".to_owned()
}

/// Hand-written so a stray `{:?}` can never leak a token into a log.
impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("developer_token", &"<redacted>")
            .field(
                "music_user_token",
                &self.music_user_token.as_ref().map(|_| "<redacted>"),
            )
            .field("storefront", &self.storefront)
            .field("authorized", &self.authorized)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackState {
    #[default]
    None,
    Loading,
    Playing,
    Paused,
    Stopped,
    Ended,
    Seeking,
    Waiting,
    Stalled,
    Completed,
    #[serde(other)]
    Unknown,
}

impl PlaybackState {
    /// What MPRIS calls `PlaybackStatus`, collapsed from the ten states.
    pub fn is_playing(self) -> bool {
        matches!(self, Self::Playing | Self::Seeking)
    }

    /// True while the player is working towards audio — the UI shows a spinner
    /// rather than a stale "paused".
    pub fn is_busy(self) -> bool {
        matches!(self, Self::Loading | Self::Waiting | Self::Stalled)
    }
}

/// Why the queue event fired. An edit and a cursor move can carry identical
/// numbers and mean opposite things (#121).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueChange {
    /// The items changed: something was added, removed, moved or reordered,
    /// so the reported index may name the wrong track (#117, #118).
    Items,
    /// Only the cursor moved.
    ///
    /// **The default, deliberately.** A correction happens only when something
    /// says an edit occurred — never because nothing said otherwise.
    #[default]
    Position,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Queue {
    #[serde(default)]
    pub reason: QueueChange,
    /// **Signed**: `-1` means a queue is loaded but nothing has become current
    /// yet. Use `index()`, not this field.
    #[serde(default = "no_position")]
    pub position: i64,
    #[serde(default)]
    pub items: Vec<Item>,
}

/// Milliseconds, floored at zero.
///
/// A negative or fractional value must become a number rather than a rejected
/// event (#89): a rejected line is a worse failure than a rounded one.
fn ms<'de, D: serde::Deserializer<'de>>(de: D) -> Result<u64, D::Error> {
    let raw = f64::deserialize(de)?;
    Ok(if raw.is_finite() && raw > 0.0 {
        raw.round() as u64
    } else {
        0
    })
}

fn no_position() -> i64 {
    -1
}

impl Queue {
    /// The current index, or `None` when nothing is current yet.
    pub fn index(&self) -> Option<usize> {
        usize::try_from(self.position)
            .ok()
            .filter(|i| *i < self.items.len())
    }
}

/// One queue item. Mapped into `music::types::Track` at the
/// boundary — this shape stays inside `player/`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    /// Process-local identity for this queue occurrence. Equal catalog items
    /// still have different values; empty means none was assigned.
    #[serde(default, skip_serializing)]
    pub occurrence_id: String,
    pub id: Option<String>,
    pub catalog_id: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default, deserialize_with = "ms")]
    pub duration_ms: u64,
    #[serde(default)]
    pub track_number: u32,
    /// Apple serves a *template* containing `{w}`/`{h}`/`{f}`.
    /// See `music::types::Artwork`.
    pub artwork_template: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrence_ids_distinguish_duplicates_and_link_the_current_item() {
        let line = r#"{
            "event":"nowPlaying",
            "item":{"occurrenceId":"run-a:7","id":"song-a"},
            "queue":{"reason":"position","position":0,"items":[
                {"occurrenceId":"run-a:7","id":"song-a"},
                {"occurrenceId":"run-a:8","id":"song-a"}
            ]}
        }"#;

        match serde_json::from_str::<Event>(line).expect("must parse") {
            Event::NowPlaying {
                item: Some(item),
                queue,
            } => {
                assert_eq!(item.occurrence_id, "run-a:7");
                assert_eq!(
                    queue.items[0].occurrence_id.as_str(),
                    item.occurrence_id.as_str()
                );
                assert_ne!(
                    queue.items[1].occurrence_id.as_str(),
                    item.occurrence_id.as_str()
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn a_negative_position_is_read_as_zero_not_rejected() {
        // The exact lines from the log in #89, twice at a track boundary. As a
        // `u64` these were rejected whole and the position was dropped.
        for line in [
            r#"{"event":"position","positionMs":-544000,"durationMs":266000}"#,
            r#"{"event":"position","positionMs":-298000,"durationMs":232000}"#,
        ] {
            match serde_json::from_str::<Event>(line).expect("must parse") {
                Event::Position {
                    position_ms,
                    duration_ms,
                } => {
                    assert_eq!(position_ms, 0, "negative should floor at zero");
                    assert!(duration_ms > 0, "duration should survive: {line}");
                }
                other => panic!("wrong variant: {other:?}"),
            }
        }
    }

    #[test]
    fn an_ordinary_position_is_untouched() {
        let line = r#"{"event":"position","positionMs":30000,"durationMs":266000}"#;
        match serde_json::from_str::<Event>(line).unwrap() {
            Event::Position {
                position_ms,
                duration_ms,
            } => assert_eq!((position_ms, duration_ms), (30_000, 266_000)),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn a_fractional_millisecond_rounds_rather_than_failing() {
        // Not seen on the wire, but the point of reading `f64` is that a shape
        // we have not seen costs a rounded value rather than a dropped event.
        let line = r#"{"event":"position","positionMs":1500.6,"durationMs":266000}"#;
        match serde_json::from_str::<Event>(line).unwrap() {
            Event::Position { position_ms, .. } => assert_eq!(position_ms, 1501),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn a_negative_track_duration_is_floored_too() {
        // `Item::duration_ms` comes from the same source and had the same gap.
        let item: Item =
            serde_json::from_str(r#"{"title":"x","durationMs":-1}"#).expect("must parse");
        assert_eq!(item.duration_ms, 0);
    }

    #[test]
    fn parses_a_now_playing_event() {
        let raw = r#"{"event":"nowPlaying","item":{"id":"1440857781","title":"Roundabout",
            "artist":"Yes","album":"Fragile","durationMs":513000,"trackNumber":1,
            "artworkTemplate":"https://is1.mzstatic.com/image/thumb/x/{w}x{h}bb.jpg"},
            "queue":{"position":0,"items":[]}}"#;
        let ev: Event = serde_json::from_str(raw).unwrap();
        let Event::NowPlaying {
            item: Some(item), ..
        } = ev
        else {
            panic!("expected NowPlaying");
        };
        assert_eq!(item.title, "Roundabout");
        assert_eq!(item.duration_ms, 513_000);
    }

    #[test]
    fn a_queue_position_of_minus_one_parses() {
        // -1 means loaded but nothing current. Parsing it as usize rejected
        // the entire event, so the queue silently never updated.
        let raw = r#"{"event":"queue","position":-1,"items":[
            {"id":"1049009209","title":"Roundabout"}]}"#;
        let ev: Event = serde_json::from_str(raw).expect("must not fail to parse");
        let Event::Queue(queue) = ev else {
            panic!("expected Queue")
        };
        assert_eq!(queue.items.len(), 1);
        assert_eq!(queue.index(), None, "-1 means nothing is current yet");
    }

    #[test]
    fn a_real_queue_position_resolves() {
        let queue = Queue {
            position: 1,
            items: vec![Item::default(), Item::default()],
            ..Default::default()
        };
        assert_eq!(queue.index(), Some(1));
    }

    #[test]
    fn a_position_past_the_end_is_not_current() {
        let queue = Queue {
            position: 5,
            items: vec![Item::default()],
            ..Default::default()
        };
        assert_eq!(queue.index(), None, "must not index out of bounds");
    }

    #[test]
    fn an_unknown_playback_state_does_not_fail_the_parse() {
        // Apple adding a state must not take the player down.
        let ev: Event =
            serde_json::from_str(r#"{"event":"playbackState","state":"teleporting"}"#).unwrap();
        let Event::PlaybackState { state } = ev else {
            panic!("expected PlaybackState");
        };
        assert_eq!(state, PlaybackState::Unknown);
    }

    #[test]
    fn tokens_never_appear_in_debug_output() {
        let t = Tokens {
            developer_token: "eyJhbGciOi.SECRET".into(),
            music_user_token: Some("ALSO_SECRET".into()),
            storefront: "gb".into(),
            authorized: true,
        };
        let shown = format!("{t:?}");
        assert!(
            !shown.contains("SECRET"),
            "token leaked into Debug: {shown}"
        );
        assert!(shown.contains("gb"));
    }
}
