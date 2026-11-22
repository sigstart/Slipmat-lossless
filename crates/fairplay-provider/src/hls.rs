// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The parts of HLS an Apple Music lossless track uses.
//!
//! The master playlist lists variants; the ALAC one with the highest
//! bandwidth wins. Its media playlist is one encrypted fMP4 file, addressed
//! segment by segment with byte ranges, each under an `EXT-X-KEY`.
//!
//! **Only the first two keys count**: the shared prefetch key, then the
//! track's own key, and later key tags are ignored, as Apple's own client
//! does. The track key is usually on segment 1, but some releases keep the
//! prefetch key for segments 0 and 1, so it is the first other key wherever
//! it appears.

use std::ops::Range;

use anyhow::{Context, Result, bail, ensure};
use reqwest::Url;

/// Opened under adam ID "0" rather than the track's, and cached by the library.
pub const PREFETCH_KEY: &str = "skd://itunes.apple.com/P000000000/s1/e1";
const MAX_PLAYLIST: usize = 1 << 20;

#[derive(Debug, PartialEq, Eq)]
pub struct Media {
    pub file: Url,
    pub segments: Vec<Segment>,
    /// Distinct key URIs, in the order the segments first use them.
    pub keys: Vec<String>,
    pub ignored: IgnoredKeys,
}

/// Later FairPlay key tags naming a key other than the track's. They are
/// skipped, so if Apple ever meant them, everything after the first one would
/// decrypt with the wrong key and fail as undecodable at `first_at_ms`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IgnoredKeys {
    pub count: u16,
    /// Playing time at the first, summed from the segments' `#EXTINF`.
    pub first_at_ms: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Segment {
    pub bytes: Range<u64>,
    /// Index into [`Media::keys`].
    pub key: usize,
    /// Playing time at its start, summed from the `#EXTINF` before it.
    pub starts_at_ms: u64,
    /// The key the playlist says it is under, honouring every tag — which
    /// `key` does not, so the two disagree wherever a skipped tag applies.
    pub named: NamedKey,
}

/// Which key a playlist tag names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    Prefetch,
    Track,
    Other,
}

impl Media {
    /// The index of the segment holding `offset` in the file.
    pub fn segment_at(&self, offset: u64) -> Option<usize> {
        self.segments
            .iter()
            .position(|segment| segment.bytes.contains(&offset))
    }
}

pub async fn load(client: &reqwest::Client, master: &Url) -> Result<Media> {
    let variant = best_alac(master, &fetch(client, master).await?)?;
    parse_media(&variant, &fetch(client, &variant).await?)
}

async fn fetch(client: &reqwest::Client, url: &Url) -> Result<String> {
    let mut response = client.get(url.clone()).send().await?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "playlist request refused"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() + chunk.len() <= MAX_PLAYLIST,
            "playlist too large"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8(body)?)
}

pub fn best_alac(base: &Url, playlist: &str) -> Result<Url> {
    let mut best: Option<(u64, &str)> = None;
    let mut lines = playlist.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") else {
            continue;
        };
        let Some(uri) = lines
            .by_ref()
            .find(|line| !line.is_empty() && !line.starts_with('#'))
        else {
            break;
        };
        let attributes = parse_attributes(attributes);
        let is_alac = attribute(&attributes, "CODECS").is_some_and(|codecs| {
            codecs
                .split(',')
                .any(|codec| codec.trim().eq_ignore_ascii_case("alac"))
        });
        let bandwidth = attribute(&attributes, "BANDWIDTH")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        if is_alac && best.is_none_or(|(highest, _)| bandwidth > highest) {
            best = Some((bandwidth, uri));
        }
    }
    let (_, uri) = best.context("the track has no ALAC variant")?;
    https(base.join(uri)?)
}

pub fn parse_media(base: &Url, playlist: &str) -> Result<Media> {
    let mut keys: Vec<String> = Vec::new();
    let mut key = None;
    let mut track_key_seen = false;
    let mut range = None;
    let mut next_start = 0;
    let mut file: Option<Url> = None;
    let mut segments = Vec::new();
    let mut ignored = IgnoredKeys::default();
    let mut elapsed_ms = 0;
    let mut duration_ms = 0;
    let mut track_uri: Option<String> = None;
    let mut named = None;
    for line in playlist.lines().map(str::trim) {
        if let Some(attributes) = line.strip_prefix("#EXT-X-KEY:") {
            let attributes = parse_attributes(attributes);
            let fairplay = attribute(&attributes, "KEYFORMAT")
                .is_some_and(|format| format.contains("streamingkeydelivery"));
            if !fairplay {
                continue;
            }
            let uri = attribute(&attributes, "URI").context("EXT-X-KEY without a URI")?;
            if track_uri.is_none() && uri != PREFETCH_KEY {
                track_uri = Some(uri.to_owned());
            }
            named = Some(if uri == PREFETCH_KEY {
                NamedKey::Prefetch
            } else if track_uri.as_deref() == Some(uri) {
                NamedKey::Track
            } else {
                NamedKey::Other
            });
            if track_key_seen {
                if keys.last().is_some_and(|track| track != uri) {
                    ignored.count = ignored.count.saturating_add(1);
                    ignored.first_at_ms.get_or_insert(elapsed_ms);
                }
                continue;
            }
            ensure!(uri.starts_with("skd://"), "the track key is not FairPlay");
            track_key_seen = uri != PREFETCH_KEY;
            key = Some(match keys.iter().position(|known| known == uri) {
                Some(index) => index,
                None => {
                    keys.push(uri.to_owned());
                    keys.len() - 1
                }
            });
        } else if let Some(info) = line.strip_prefix("#EXTINF:") {
            duration_ms = extinf_ms(info);
        } else if let Some(byterange) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            range = Some(parse_byterange(byterange, next_start)?);
        } else if !line.is_empty() && !line.starts_with('#') {
            let bytes = range.take().context("a segment without a byte range")?;
            let url = https(base.join(line)?)?;
            match &file {
                Some(file) => ensure!(file == &url, "the track spans several files"),
                None => file = Some(url),
            }
            next_start = bytes.end;
            let starts_at_ms = elapsed_ms;
            elapsed_ms += std::mem::take(&mut duration_ms);
            segments.push(Segment {
                bytes,
                key: key.context("a segment before any FairPlay key")?,
                starts_at_ms,
                named: named.context("a segment before any FairPlay key")?,
            });
        }
    }
    ensure!(!segments.is_empty(), "the media playlist has no segments");
    Ok(Media {
        file: file.context("the media playlist has no file")?,
        segments,
        keys,
        ignored,
    })
}

/// `#EXTINF:<seconds>,[title]`, in milliseconds; unreadable counts as none.
fn extinf_ms(info: &str) -> u64 {
    let seconds = info.split(',').next().unwrap_or_default().trim();
    match seconds.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() && seconds > 0.0 => (seconds * 1000.0).round() as u64,
        _ => 0,
    }
}

/// `length[@offset]`; without an offset the range follows the previous one.
fn parse_byterange(value: &str, next_start: u64) -> Result<Range<u64>> {
    let (length, start) = match value.split_once('@') {
        Some((length, start)) => (length, start.parse()?),
        None => (value, next_start),
    };
    let length: u64 = length.parse()?;
    let end = start.checked_add(length).context("byte range overflows")?;
    Ok(start..end)
}

fn https(url: Url) -> Result<Url> {
    if url.scheme() != "https" || url.host_str().is_none() || !url.username().is_empty() {
        bail!("a playlist URL is not plain HTTPS");
    }
    Ok(url)
}

/// `KEY=value,KEY="quoted, value"` as name/value pairs, quotes removed.
fn parse_attributes(text: &str) -> Vec<(&str, &str)> {
    let mut attributes = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (at, character) in text.char_indices().chain([(text.len(), ',')]) {
        match character {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                if let Some((name, value)) = text[start..at].split_once('=') {
                    attributes.push((name.trim(), value.trim().trim_matches('"')));
                }
                start = at + 1;
            }
            _ => {}
        }
    }
    attributes
}

fn attribute<'a>(attributes: &[(&str, &'a str)], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| *value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://aod.itunes.apple.com/itunes-assets/x/master.m3u8").unwrap()
    }

    #[test]
    fn attributes_keep_commas_inside_quotes() {
        let parsed = parse_attributes(r#"BANDWIDTH=1644938,CODECS="alac,mp4a.40.2",URI="a,b""#);
        assert_eq!(
            parsed,
            vec![
                ("BANDWIDTH", "1644938"),
                ("CODECS", "alac,mp4a.40.2"),
                ("URI", "a,b")
            ]
        );
    }

    #[test]
    fn the_highest_bandwidth_alac_variant_wins() {
        let master = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=273335,CODECS=\"mp4a.40.2\"\naac.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=5552204,CODECS=\"alac\"\nhires.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1549788,CODECS=\"alac\"\ncd.m3u8\n";
        assert_eq!(
            best_alac(&base(), master).unwrap().as_str(),
            "https://aod.itunes.apple.com/itunes-assets/x/hires.m3u8"
        );
        let aac_only = "#EXT-X-STREAM-INF:BANDWIDTH=1,CODECS=\"mp4a.40.2\"\naac.m3u8\n";
        assert!(best_alac(&base(), aac_only).is_err());
    }

    const KEY: &str =
        r#"METHOD=SAMPLE-AES,KEYFORMAT="com.apple.streamingkeydelivery",KEYFORMATVERSIONS="1""#;

    fn key(uri: &str) -> String {
        format!("#EXT-X-KEY:URI=\"{uri}\",{KEY}\n")
    }

    fn segment(range: &str) -> String {
        format!("#EXTINF:10,\n#EXT-X-BYTERANGE:{range}\ntrack.mp4\n")
    }

    #[test]
    fn the_track_key_can_start_after_two_prefetch_segments() {
        // The Polis playlist: other DRM systems' keys between FairPlay's, and
        // the prefetch key covering segments 0 and 1.
        let playlist = [
            "#EXTM3U\n".to_owned(),
            key(PREFETCH_KEY),
            "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"data:text/plain;base64,AAAA\",KEYFORMAT=\"urn:uuid:edef8ba9\"\n".to_owned(),
            "#EXT-X-MAP:URI=\"track.mp4\",BYTERANGE=\"1020@0\"\n".to_owned(),
            segment("100@1020"),
            segment("100"),
            key("skd://itunes.apple.com/p1553683450/c23"),
            segment("100"),
            key("skd://itunes.apple.com/p1553683450/c99"),
            segment("50"),
        ]
        .concat();
        let media = parse_media(&base(), &playlist).unwrap();
        assert_eq!(
            media.keys,
            vec![PREFETCH_KEY, "skd://itunes.apple.com/p1553683450/c23"]
        );
        assert_eq!(
            media
                .segments
                .iter()
                .map(|segment| (segment.bytes.clone(), segment.key))
                .collect::<Vec<_>>(),
            vec![
                (1020..1120, 0),
                (1120..1220, 0),
                (1220..1320, 1),
                (1320..1370, 1)
            ]
        );
        // `c99` is skipped, after three ten-second segments.
        assert_eq!(
            media.ignored,
            IgnoredKeys {
                count: 1,
                first_at_ms: Some(30_000)
            }
        );
        assert_eq!(media.segment_at(1250), Some(2));
        assert_eq!(media.segments[2].starts_at_ms, 20_000);
        assert_eq!(
            media
                .segments
                .iter()
                .map(|segment| segment.named)
                .collect::<Vec<_>>(),
            [
                NamedKey::Prefetch,
                NamedKey::Prefetch,
                NamedKey::Track,
                NamedKey::Other
            ]
        );
        let key_at = |offset| {
            media
                .segment_at(offset)
                .map(|index| media.segments[index].key)
        };
        assert_eq!(key_at(1020), Some(0));
        assert_eq!(key_at(1250), Some(1));
        assert_eq!(key_at(5000), None);
        assert_eq!(
            media.file.as_str(),
            "https://aod.itunes.apple.com/itunes-assets/x/track.mp4"
        );
    }

    #[test]
    fn a_return_to_the_prefetch_key_is_counted_and_named() {
        let playlist = [
            "#EXTM3U\n".to_owned(),
            key(PREFETCH_KEY),
            segment("100@0"),
            key("skd://itunes.apple.com/p1553683450/c23"),
            segment("100"),
            key(PREFETCH_KEY),
            segment("100"),
        ]
        .concat();
        let media = parse_media(&base(), &playlist).unwrap();
        assert_eq!(media.segments[2].named, NamedKey::Prefetch);
        // Decrypted with the track key regardless: that is the rule being tested.
        assert_eq!(media.segments[2].key, 1);
        assert_eq!(
            media.ignored,
            IgnoredKeys {
                count: 1,
                first_at_ms: Some(20_000)
            }
        );
    }

    #[test]
    fn unusable_media_playlists_are_refused() {
        let no_key = segment("100@0");
        let two_files = [
            key(PREFETCH_KEY),
            segment("100@0"),
            "#EXT-X-BYTERANGE:100\nother.mp4\n".to_owned(),
        ]
        .concat();
        let not_fairplay = key("https://example.invalid/key");
        let plain_http = [
            key(PREFETCH_KEY),
            "#EXT-X-BYTERANGE:1@0\nhttp://cdn.invalid/t.mp4\n".to_owned(),
        ]
        .concat();
        for playlist in [no_key, two_files, not_fairplay, plain_http, String::new()] {
            assert!(parse_media(&base(), &playlist).is_err(), "{playlist}");
        }
    }
}
