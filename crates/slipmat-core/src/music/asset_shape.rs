// SPDX-FileCopyrightText: 2026 Jan Vermeulen
// SPDX-License-Identifier: GPL-3.0-or-later

//! An asset's structure with its addresses taken out: what `climat info`
//! shows, so the shape of a stream can be read without handing anyone a link
//! to it. An asset is an HLS playlist or, for Apple's lossy kinds, a whole
//! MP4 file, read here from its first bytes.

use std::collections::BTreeSet;

/// One line per distinct tag line, URIs reduced to their scheme, then the
/// segment count and how many files those segments address.
pub fn summarise(playlist: &str) -> Vec<String> {
    let mut lines: Vec<(String, usize)> = Vec::new();
    let mut segments = 0;
    let mut files = BTreeSet::new();
    for line in playlist
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if !line.starts_with('#') {
            segments += 1;
            files.insert(line.split('?').next().unwrap_or(line).to_owned());
            continue;
        }
        let tag = redact(line);
        if tag.starts_with("#EXTINF") || tag.starts_with("#EXT-X-BYTERANGE") {
            continue;
        }
        match lines.iter_mut().find(|(known, _)| *known == tag) {
            Some((_, count)) => *count += 1,
            None => lines.push((tag, 1)),
        }
    }
    let mut summary: Vec<String> = lines
        .into_iter()
        .map(|(tag, count)| match count {
            1 => tag,
            count => format!("{tag}  (×{count})"),
        })
        .collect();
    if segments > 0 {
        summary.push(format!(
            "{segments} URI line(s), {} distinct file(s)",
            files.len()
        ));
    }
    summary
}

/// `URI="skd://…/c23"` becomes `URI="skd://…"`; a bare https address, `…`.
fn redact(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("URI=\"") {
        let (before, after) = rest.split_at(at + "URI=\"".len());
        out.push_str(before);
        let end = after.find('"').unwrap_or(after.len());
        let value = &after[..end];
        match value.split_once(':') {
            Some((scheme, _)) if !scheme.contains('/') => {
                out.push_str(scheme);
                out.push_str(":…");
            }
            _ => out.push('…'),
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// The highest-bandwidth variant a master playlist lists, if it is one.
pub fn best_variant(playlist: &str) -> Option<&str> {
    let mut best: Option<(u64, &str)> = None;
    let mut lines = playlist.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") else {
            continue;
        };
        let uri = lines
            .by_ref()
            .find(|line| !line.is_empty() && !line.starts_with('#'))?;
        let bandwidth = attributes
            .split(',')
            .find_map(|pair| pair.strip_prefix("BANDWIDTH="))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        if best.is_none_or(|(highest, _)| bandwidth > highest) {
            best = Some((bandwidth, uri));
        }
    }
    best.map(|(_, uri)| uri)
}

/// Containers whose children say what the audio is and how it is protected.
const CONTAINERS: [&[u8; 4]; 7] = [
    b"moov", b"trak", b"mdia", b"minf", b"stbl", b"sinf", b"schi",
];
/// An audio sample entry's own fields, before its child boxes.
const AUDIO_ENTRY: usize = 28;
const MAX_BOX_LINES: usize = 80;

/// The box tree of an MP4's first bytes, one line per box, indented by depth.
/// Sample entries are opened; `schm` shows its scheme and `ftyp` its brand.
pub fn mp4_boxes(head: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    walk(head, 0, &mut lines);
    lines.truncate(MAX_BOX_LINES);
    lines
}

fn walk(bytes: &[u8], depth: usize, lines: &mut Vec<String>) {
    let mut at = 0;
    while at + 8 <= bytes.len() {
        let size32 = u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        let kind: [u8; 4] = [bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]];
        let (header, size) = match size32 {
            0 => (8, (bytes.len() - at) as u64),
            1 if at + 16 <= bytes.len() => {
                let mut large = [0; 8];
                large.copy_from_slice(&bytes[at + 8..at + 16]);
                (16, u64::from_be_bytes(large))
            }
            1 => break,
            size => (8, u64::from(size)),
        };
        if size < header as u64 || !kind.iter().all(u8::is_ascii_graphic) {
            lines.push(format!("{}(unreadable box)", "  ".repeat(depth)));
            break;
        }
        let end = at.saturating_add(usize::try_from(size).unwrap_or(usize::MAX));
        let body = &bytes[(at + header).min(bytes.len())..end.min(bytes.len())];
        let truncated = if end > bytes.len() {
            " (continues past what was read)"
        } else {
            ""
        };
        let name = String::from_utf8_lossy(&kind);
        lines.push(format!(
            "{}{name} {size}{}{truncated}",
            "  ".repeat(depth),
            detail(&kind, body)
        ));
        if CONTAINERS.contains(&&kind) {
            walk(body, depth + 1, lines);
        } else if &kind == b"stsd" && body.len() > 8 {
            walk(&body[8..], depth + 1, lines);
        } else if is_audio_entry(&kind) && body.len() > AUDIO_ENTRY {
            walk(&body[AUDIO_ENTRY..], depth + 1, lines);
        }
        if end >= bytes.len() {
            break;
        }
        at = end;
    }
}

fn is_audio_entry(kind: &[u8; 4]) -> bool {
    matches!(
        kind,
        b"mp4a" | b"enca" | b"drms" | b"alac" | b"ec-3" | b"ac-3"
    )
}

fn detail(kind: &[u8; 4], body: &[u8]) -> String {
    match kind {
        b"ftyp" if body.len() >= 4 => format!(" brand={}", String::from_utf8_lossy(&body[..4])),
        b"frma" if body.len() >= 4 => format!(" format={}", String::from_utf8_lossy(&body[..4])),
        b"schm" if body.len() >= 8 => format!(" scheme={}", String::from_utf8_lossy(&body[4..8])),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_reduced_to_their_scheme_and_segments_counted() {
        let playlist = "#EXTM3U\n\
            #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://itunes.apple.com/p1/c23\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
            #EXT-X-MAP:URI=\"https://cdn.invalid/a/track.mp4\",BYTERANGE=\"10@0\"\n\
            #EXTINF:10,\n#EXT-X-BYTERANGE:100@10\ntrack.mp4\n\
            #EXTINF:10,\n#EXT-X-BYTERANGE:100\ntrack.mp4?x=1\n";
        assert_eq!(
            summarise(playlist),
            [
                "#EXTM3U",
                "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd:…\",KEYFORMAT=\"com.apple.streamingkeydelivery\"",
                "#EXT-X-MAP:URI=\"https:…\",BYTERANGE=\"10@0\"",
                "2 URI line(s), 1 distinct file(s)",
            ]
        );
        assert_eq!(redact("#X:URI=\"relative/a.mp4\""), "#X:URI=\"…\"");
    }

    #[test]
    fn the_best_variant_is_the_highest_bandwidth() {
        let master = "#EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS=\"mp4a.40.5\"\nlow.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=256000,CODECS=\"mp4a.40.2\"\nhigh.m3u8\n";
        assert_eq!(best_variant(master), Some("high.m3u8"));
        assert_eq!(best_variant("#EXTM3U\n"), None);
    }

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(body.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn a_protected_sample_entry_shows_its_format_and_scheme() {
        let schm = boxed(b"schm", &[0, 0, 0, 0, b'c', b'b', b'c', b's', 0, 1, 0, 0]);
        let sinf = boxed(b"sinf", &[boxed(b"frma", b"mp4a"), schm].concat());
        let enca = boxed(b"enca", &[vec![0; AUDIO_ENTRY], sinf].concat());
        let stsd = boxed(b"stsd", &[vec![0, 0, 0, 0, 0, 0, 0, 1], enca].concat());
        let moov = boxed(
            b"moov",
            &boxed(
                b"trak",
                &boxed(b"mdia", &boxed(b"minf", &boxed(b"stbl", &stsd))),
            ),
        );
        let file = [boxed(b"ftyp", b"M4A \0\0\0\0"), moov].concat();
        let lines = mp4_boxes(&file);
        assert_eq!(lines[0], "ftyp 16 brand=M4A ");
        assert!(
            lines
                .iter()
                .any(|line| line.trim() == "frma 12 format=mp4a")
        );
        assert!(
            lines
                .iter()
                .any(|line| line.trim() == "schm 20 scheme=cbcs")
        );
    }
}
