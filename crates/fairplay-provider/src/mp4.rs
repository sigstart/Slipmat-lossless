// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! Just enough MP4 (ISO/IEC 14496-12) and Common Encryption (23001-7) to turn
//! Apple's encrypted fragmented ALAC into a clear stream Symphonia can read.
//!
//! - **The init segment** is rewritten: Symphonia has no `enca` sample entry,
//!   so the entry takes back its original format from `frma`, loses `sinf`,
//!   and the duplicate entry Apple adds for a second IV is dropped.
//! - **Fragments** keep their bytes and sizes, so every offset in them stays
//!   valid. Only the protected ranges of each sample change, in place, and an
//!   absolute `base_data_offset` is moved by however much shorter the init
//!   became. Symphonia ignores the `senc`, `saiz` and `saio` left in `traf`.
//!
//! Every size comes from the network, so each is checked before it is used.

use std::ops::Range;

use anyhow::{Context, Result, bail, ensure};

pub type FourCc = [u8; 4];

/// The largest top-level box accepted, which bounds a fragment in memory.
pub const MAX_BOX: u64 = 64 << 20;
/// More samples than this in one `trun` is a malformed file, not a long one.
const MAX_SAMPLES: u32 = 1 << 20;

pub struct Header {
    pub kind: FourCc,
    pub header_len: usize,
    pub size: u64,
}

/// `None` until `bytes` holds the whole header.
pub fn header(bytes: &[u8]) -> Result<Option<Header>> {
    let Some(start) = bytes.get(..8) else {
        return Ok(None);
    };
    let kind = start[4..8].try_into()?;
    let (size, header_len) = match u32::from_be_bytes(start[..4].try_into()?) {
        0 => bail!("MP4 box without a size"),
        1 => {
            let Some(large) = bytes.get(8..16) else {
                return Ok(None);
            };
            (u64::from_be_bytes(large.try_into()?), 16)
        }
        size => (u64::from(size), 8),
    };
    ensure!(size >= header_len as u64, "MP4 box smaller than its header");
    Ok(Some(Header {
        kind,
        header_len,
        size,
    }))
}

/// The children of a box payload, as `(kind, whole box)` slices.
fn children(mut payload: &[u8]) -> Result<Vec<(FourCc, &[u8], usize)>> {
    let mut boxes = Vec::new();
    while !payload.is_empty() {
        let header = header(payload)?.context("truncated MP4 box header")?;
        let size = usize::try_from(header.size)?;
        let whole = payload.get(..size).context("MP4 box overruns its parent")?;
        boxes.push((header.kind, whole, header.header_len));
        payload = &payload[size..];
    }
    Ok(boxes)
}

/// A box of the init segment, kept as a tree so sizes can be recomputed.
#[derive(Clone)]
struct Node {
    kind: FourCc,
    /// Fields before the children: full-box flags, a sample entry's fields.
    prefix: Vec<u8>,
    body: Body,
}

#[derive(Clone)]
enum Body {
    Opaque(Vec<u8>),
    Children(Vec<Node>),
}

const CONTAINERS: [&FourCc; 10] = [
    b"moov", b"trak", b"mdia", b"minf", b"stbl", b"mvex", b"edts", b"dinf", b"sinf", b"schi",
];

impl Node {
    fn parse(kind: FourCc, payload: &[u8], in_stsd: bool) -> Result<Self> {
        let (prefix_len, has_children) = if in_stsd {
            (sample_entry_prefix(payload)?, true)
        } else if &kind == b"stsd" {
            (8, true)
        } else {
            (0, CONTAINERS.contains(&&kind))
        };
        let prefix = payload
            .get(..prefix_len)
            .context("truncated MP4 box fields")?;
        let rest = &payload[prefix_len..];
        let body = if has_children {
            let entries = &kind == b"stsd";
            Body::Children(
                children(rest)?
                    .into_iter()
                    .map(|(kind, whole, header_len)| {
                        Self::parse(kind, &whole[header_len..], entries)
                    })
                    .collect::<Result<_>>()?,
            )
        } else {
            Body::Opaque(rest.to_vec())
        };
        Ok(Self {
            kind,
            prefix: prefix.to_vec(),
            body,
        })
    }

    fn opaque(kind: FourCc, payload: Vec<u8>) -> Self {
        Self {
            kind,
            prefix: Vec::new(),
            body: Body::Opaque(payload),
        }
    }

    fn container(kind: FourCc, children: Vec<Self>) -> Self {
        Self {
            kind,
            prefix: Vec::new(),
            body: Body::Children(children),
        }
    }

    fn payload_len(&self) -> u64 {
        self.prefix.len() as u64
            + match &self.body {
                Body::Opaque(bytes) => bytes.len() as u64,
                Body::Children(children) => children.iter().map(Self::len).sum(),
            }
    }

    fn len(&self) -> u64 {
        let payload = self.payload_len();
        if payload + 8 > u64::from(u32::MAX) {
            payload + 16
        } else {
            payload + 8
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let size = self.len();
        if size > u64::from(u32::MAX) {
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(&self.kind);
            out.extend_from_slice(&size.to_be_bytes());
        } else {
            out.extend_from_slice(&(size as u32).to_be_bytes());
            out.extend_from_slice(&self.kind);
        }
        out.extend_from_slice(&self.prefix);
        match &self.body {
            Body::Opaque(bytes) => out.extend_from_slice(bytes),
            Body::Children(children) => children.iter().for_each(|child| child.encode(out)),
        }
    }

    fn children_mut(&mut self) -> Result<&mut Vec<Self>> {
        match &mut self.body {
            Body::Children(children) => Ok(children),
            Body::Opaque(_) => bail!("MP4 {} is not a container", name(&self.kind)),
        }
    }

    fn child_mut(&mut self, kind: &FourCc) -> Result<&mut Self> {
        let parent = self.kind;
        self.children_mut()?
            .iter_mut()
            .find(|child| &child.kind == kind)
            .with_context(|| format!("MP4 {} has no {}", name(&parent), name(kind)))
    }

    fn child(&self, kind: &FourCc) -> Option<&Self> {
        match &self.body {
            Body::Children(children) => children.iter().find(|child| &child.kind == kind),
            Body::Opaque(_) => None,
        }
    }

    fn bytes(&self) -> &[u8] {
        match &self.body {
            Body::Opaque(bytes) => bytes,
            Body::Children(_) => &[],
        }
    }
}

/// An audio sample entry's fields before its child boxes, by QuickTime
/// sound description version.
fn sample_entry_prefix(payload: &[u8]) -> Result<usize> {
    let version = payload.get(8..10).context("truncated MP4 sample entry")?;
    match u16::from_be_bytes([version[0], version[1]]) {
        0 => Ok(28),
        1 => Ok(44),
        2 => Ok(64),
        _ => bail!("unknown MP4 sound description version"),
    }
}

fn name(kind: &FourCc) -> String {
    String::from_utf8_lossy(kind).into_owned()
}

/// What decrypting the fragments needs from the init segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Track {
    pub id: u32,
    pub iv_size: u8,
    pub default_sample_size: u32,
}

/// Rewrites `moov` as a clear init segment, returned with `ftyp` before it.
pub fn clear_init(ftyp: Option<&[u8]>, moov: &[u8]) -> Result<(Vec<u8>, Track)> {
    let header = header(moov)?.context("truncated moov")?;
    ensure!(&header.kind == b"moov", "expected moov");
    let mut moov = Node::parse(*b"moov", &moov[header.header_len..], false)?;
    let children = moov.children_mut()?;
    children.retain(|child| &child.kind != b"pssh");
    ensure!(
        children
            .iter()
            .filter(|child| &child.kind == b"trak")
            .count()
            == 1,
        "expected exactly one MP4 track"
    );

    let trak = moov.child_mut(b"trak")?;
    let id = track_id(trak.child(b"tkhd").context("MP4 track has no tkhd")?)?;
    let stbl = trak
        .child_mut(b"mdia")?
        .child_mut(b"minf")?
        .child_mut(b"stbl")?;
    stbl.children_mut()?
        .retain(|child| !is_encryption_group(child));
    let iv_size = clear_sample_entry(stbl.child_mut(b"stsd")?)?;

    let default_sample_size = match moov.child(b"mvex") {
        Some(mvex) => trex_default_size(mvex, id)?,
        None => {
            moov.children_mut()?
                .push(Node::container(*b"mvex", vec![trex(id)]));
            0
        }
    };

    let mut out = ftyp.map(<[u8]>::to_vec).unwrap_or_default();
    moov.encode(&mut out);
    Ok((
        out,
        Track {
            id,
            iv_size,
            default_sample_size,
        },
    ))
}

fn track_id(tkhd: &Node) -> Result<u32> {
    let bytes = tkhd.bytes();
    let at = match bytes.first() {
        Some(0) => 12,
        Some(1) => 20,
        _ => bail!("unknown tkhd version"),
    };
    read_u32(bytes, at)
}

fn is_encryption_group(node: &Node) -> bool {
    (&node.kind == b"sbgp" || &node.kind == b"sgpd")
        && matches!(node.bytes().get(4..8), Some(b"seig" | b"seam"))
}

/// Keeps one sample entry, made clear. Returns the per-sample IV size.
fn clear_sample_entry(stsd: &mut Node) -> Result<u8> {
    let entries = stsd.children_mut()?;
    match entries.as_slice() {
        [_] => {}
        [first, second] if first.kind == second.kind => {}
        [_, _] => bail!("MP4 sample entries differ in format"),
        _ => bail!("expected one or two MP4 sample entries"),
    }
    entries.truncate(1);
    let entry = &mut entries[0];
    let sinf_at = entry
        .children_mut()?
        .iter()
        .position(|child| &child.kind == b"sinf")
        .context("the track is not encrypted")?;
    let sinf = entry.children_mut()?.remove(sinf_at);
    let original = sinf.child(b"frma").context("sinf has no frma")?.bytes();
    entry.kind = original.try_into().context("invalid frma")?;
    ensure!(&entry.kind == b"alac", "the track is not ALAC");
    let scheme = sinf.child(b"schm").context("sinf has no schm")?.bytes();
    ensure!(scheme.get(4..8) == Some(b"cbcs"), "the track is not cbcs");
    let tenc = sinf
        .child(b"schi")
        .and_then(|schi| schi.child(b"tenc"))
        .context("sinf has no tenc")?
        .bytes();
    let iv_size = tenc_iv_size(tenc)?;
    stsd.prefix
        .get_mut(4..8)
        .context("truncated stsd")?
        .copy_from_slice(&1u32.to_be_bytes());
    Ok(iv_size)
}

/// The per-sample IV size, after refusing pattern encryption: ALAC protects
/// whole blocks, so a skip pattern means this is not the stream expected.
fn tenc_iv_size(tenc: &[u8]) -> Result<u8> {
    ensure!(tenc.len() >= 24, "truncated tenc");
    if tenc[0] > 0 {
        ensure!(tenc[5] & 0x0f == 0, "pattern encryption is not supported");
    }
    Ok(tenc[7])
}

fn trex_default_size(mvex: &Node, id: u32) -> Result<u32> {
    let Body::Children(children) = &mvex.body else {
        bail!("invalid mvex");
    };
    for trex in children.iter().filter(|child| &child.kind == b"trex") {
        if read_u32(trex.bytes(), 4)? == id {
            return read_u32(trex.bytes(), 16);
        }
    }
    bail!("mvex has no trex for the track")
}

fn trex(id: u32) -> Node {
    let mut payload = vec![0; 4];
    for field in [id, 1, 4096, 0, 0] {
        payload.extend_from_slice(&field.to_be_bytes());
    }
    Node::opaque(*b"trex", payload)
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32> {
    let field = bytes.get(at..at + 4).context("truncated MP4 field")?;
    Ok(u32::from_be_bytes(field.try_into()?))
}

/// A forward reader over one box's fields.
struct Fields<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Fields<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let field = self
            .bytes
            .get(self.at..self.at + len)
            .context("truncated MP4 field")?;
        self.at += len;
        Ok(field)
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }

    fn skip_if(&mut self, present: bool, len: usize) -> Result<()> {
        if present {
            self.take(len)?;
        }
        Ok(())
    }
}

const BASE_DATA_OFFSET: u32 = 0x1;
const DEFAULT_BASE_IS_MOOF: u32 = 0x2_0000;

/// A fragment ready for decryption: `moof` and `mdat` together, and the
/// byte ranges of it to decrypt, each a whole number of 16-byte blocks.
pub struct Fragment {
    pub bytes: Vec<u8>,
    pub protected: Vec<Range<usize>>,
}

/// Joins a `moof` read at `moof_at` with its `mdat`, finds the protected
/// ranges, and moves any absolute base offset back by `shift`.
pub fn fragment(
    mut moof: Vec<u8>,
    moof_at: u64,
    mdat: &[u8],
    track: &Track,
    shift: u64,
) -> Result<Fragment> {
    let moof_header = header(&moof)?.context("truncated moof")?;
    let mdat_header = header(mdat)?.context("truncated mdat")?;
    let data = moof.len() + mdat_header.header_len..moof.len() + mdat.len();
    let mut protected = Vec::new();
    let mut base_offsets = Vec::new();
    let mut traf_end = 0;
    for (kind, traf, header_len) in children(&moof[moof_header.header_len..])? {
        if &kind != b"traf" {
            continue;
        }
        let traf_at = offset_in(&moof, traf);
        let samples = traf_samples(&traf[header_len..], track, moof_at, traf_end, &data)?;
        traf_end = samples.data_end;
        protected.extend(samples.protected);
        if let Some(field) = samples.base_data_offset {
            base_offsets.push(traf_at + header_len + field);
        }
    }
    for at in base_offsets {
        let field = moof.get_mut(at..at + 8).context("truncated tfhd")?;
        let absolute = u64::from_be_bytes((&*field).try_into()?);
        let moved = absolute
            .checked_sub(shift)
            .context("MP4 base offset before the init segment")?;
        field.copy_from_slice(&moved.to_be_bytes());
    }
    moof.extend_from_slice(mdat);
    Ok(Fragment {
        bytes: moof,
        protected,
    })
}

fn offset_in(outer: &[u8], inner: &[u8]) -> usize {
    inner.as_ptr() as usize - outer.as_ptr() as usize
}

struct TrafSamples {
    protected: Vec<Range<usize>>,
    /// Where this traf's sample data ends, relative to the moof's start.
    data_end: usize,
    /// The `base_data_offset` field's position in the traf payload.
    base_data_offset: Option<usize>,
}

/// One track fragment's sample positions, relative to the moof's start, and
/// the protected ranges within them. Every sample must lie inside `data`.
fn traf_samples(
    traf: &[u8],
    track: &Track,
    moof_at: u64,
    previous_end: usize,
    data: &Range<usize>,
) -> Result<TrafSamples> {
    let boxes = children(traf)?;
    let (_, tfhd, tfhd_header) = boxes
        .iter()
        .find(|(kind, ..)| kind == b"tfhd")
        .context("traf has no tfhd")?;
    let mut fields = Fields::new(&tfhd[*tfhd_header..]);
    let flags = fields.u32()? & 0x00ff_ffff;
    ensure!(fields.u32()? == track.id, "MP4 fragment for another track");
    let base_field = offset_in(traf, tfhd) + tfhd_header + fields.at;
    let base = if flags & BASE_DATA_OFFSET != 0 {
        let absolute = fields.u64()?;
        usize::try_from(
            absolute
                .checked_sub(moof_at)
                .context("MP4 base offset before its moof")?,
        )?
    } else if flags & DEFAULT_BASE_IS_MOOF != 0 {
        0
    } else {
        previous_end
    };
    fields.skip_if(flags & 0x2 != 0, 4)?;
    fields.skip_if(flags & 0x8 != 0, 4)?;
    let default_size = if flags & 0x10 != 0 {
        fields.u32()?
    } else {
        track.default_sample_size
    };

    let mut subsamples = boxes
        .iter()
        .find(|(kind, ..)| kind == b"senc")
        .map(|(_, senc, header_len)| Senc::new(&senc[*header_len..], track.iv_size))
        .transpose()?
        .context("traf has no senc")?;
    let mut protected = Vec::new();
    let mut position = base;
    for (_, trun, header_len) in boxes.iter().filter(|(kind, ..)| kind == b"trun") {
        let mut fields = Fields::new(&trun[*header_len..]);
        let flags = fields.u32()? & 0x00ff_ffff;
        let count = fields.u32()?;
        ensure!(count <= MAX_SAMPLES, "too many MP4 samples");
        if flags & 0x1 != 0 {
            let offset = i64::from(i32::from_be_bytes(fields.take(4)?.try_into()?));
            position =
                usize::try_from(base as i64 + offset).context("MP4 sample data before its moof")?;
        }
        fields.skip_if(flags & 0x4 != 0, 4)?;
        for _ in 0..count {
            fields.skip_if(flags & 0x100 != 0, 4)?;
            let size = if flags & 0x200 != 0 {
                fields.u32()?
            } else {
                default_size
            };
            fields.skip_if(flags & 0x400 != 0, 4)?;
            fields.skip_if(flags & 0x800 != 0, 4)?;
            let sample = position..position + usize::try_from(size)?;
            ensure!(
                sample.start >= data.start && sample.end <= data.end,
                "MP4 sample outside its mdat"
            );
            subsamples.protect(sample.clone(), &mut protected)?;
            position = sample.end;
        }
    }
    Ok(TrafSamples {
        protected,
        data_end: position,
        base_data_offset: (flags & BASE_DATA_OFFSET != 0).then_some(base_field),
    })
}

/// The sample encryption box: for each sample in order, its subsamples.
struct Senc<'a> {
    fields: Fields<'a>,
    remaining: u32,
    iv_size: usize,
    has_subsamples: bool,
}

impl<'a> Senc<'a> {
    fn new(payload: &'a [u8], iv_size: u8) -> Result<Self> {
        let mut fields = Fields::new(payload);
        let flags = fields.u32()? & 0x00ff_ffff;
        ensure!(flags & 0x1 == 0, "senc overrides the track's encryption");
        Ok(Self {
            remaining: fields.u32()?,
            fields,
            iv_size: usize::from(iv_size),
            has_subsamples: flags & 0x2 != 0,
        })
    }

    /// Adds the protected parts of the next sample, which occupies `sample`.
    fn protect(&mut self, sample: Range<usize>, out: &mut Vec<Range<usize>>) -> Result<()> {
        ensure!(self.remaining > 0, "senc has fewer entries than samples");
        self.remaining -= 1;
        self.fields.take(self.iv_size)?;
        if !self.has_subsamples {
            out.extend(blocks(sample.clone()));
            return Ok(());
        }
        let mut at = sample.start;
        for _ in 0..self.fields.u16()? {
            let clear = usize::from(self.fields.u16()?);
            let protected = usize::try_from(self.fields.u32()?)?;
            let start = at + clear;
            let end = start + protected;
            ensure!(
                end <= sample.end,
                "MP4 subsample past the end of its sample"
            );
            out.extend(blocks(start..end));
            at = end;
        }
        Ok(())
    }
}

/// The whole 16-byte blocks of a range; a trailing partial block is clear.
fn blocks(range: Range<usize>) -> Option<Range<usize>> {
    let len = range.len() & !0xf;
    (len > 0).then(|| range.start..range.start + len)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    pub fn full(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut fields = vec![version];
        fields.extend_from_slice(&flags.to_be_bytes()[1..]);
        fields.extend_from_slice(payload);
        boxed(kind, &fields)
    }

    /// A reversible stand-in for FairPlay: the same call scrambles and restores.
    pub fn fake_cipher(block: &mut [u8]) {
        block.iter_mut().for_each(|byte| *byte ^= 0xa5);
    }

    fn payload(whole: &[u8]) -> &[u8] {
        &whole[header(whole).unwrap().unwrap().header_len..]
    }

    /// Re-dresses a clear fragmented ALAC file the way Apple ships one: an
    /// `enca` entry with `sinf`, a duplicate entry, a `pssh`, a `senc` in
    /// every fragment, and each sample's protected blocks scrambled with
    /// [`fake_cipher`] after four clear bytes.
    pub fn encrypt_like_apple(clear: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for (kind, whole, _) in children(clear).unwrap() {
            match &kind {
                b"moov" => out.extend(encrypt_init(whole)),
                b"moof" => {}
                b"mdat" => {}
                _ => out.extend_from_slice(whole),
            }
        }
        let boxes = children(clear).unwrap();
        let mut at = 0;
        let mut encrypted = Vec::new();
        for (index, (kind, whole, _)) in boxes.iter().enumerate() {
            if kind == b"moof" {
                let (_, mdat, _) = boxes[index + 1];
                encrypted.extend(encrypt_fragment(whole, mdat));
            }
            at += whole.len();
        }
        assert_eq!(at, clear.len());
        out.extend(encrypted);
        out
    }

    fn encrypt_init(moov: &[u8]) -> Vec<u8> {
        let mut moov = Node::parse(*b"moov", payload(moov), false).unwrap();
        let stsd = moov
            .child_mut(b"trak")
            .unwrap()
            .child_mut(b"mdia")
            .unwrap()
            .child_mut(b"minf")
            .unwrap()
            .child_mut(b"stbl")
            .unwrap()
            .child_mut(b"stsd")
            .unwrap();
        let mut tenc = vec![0, 0, 1, 0];
        tenc.extend_from_slice(&[0x11; 16]);
        tenc.push(16);
        tenc.extend_from_slice(&[0x22; 16]);
        let sinf = Node::container(
            *b"sinf",
            vec![
                Node::opaque(*b"frma", b"alac".to_vec()),
                Node::opaque(
                    *b"schm",
                    [&[0, 0, 0, 0][..], b"cbcs", &[0, 1, 0, 0]].concat(),
                ),
                Node::container(
                    *b"schi",
                    vec![Node::opaque(*b"tenc", [&[1, 0, 0, 0][..], &tenc].concat())],
                ),
            ],
        );
        let entries = stsd.children_mut().unwrap();
        assert_eq!(&entries[0].kind, b"alac");
        entries[0].kind = *b"enca";
        entries[0].children_mut().unwrap().push(sinf);
        let duplicate = entries[0].clone();
        entries.push(duplicate);
        stsd.prefix[4..8].copy_from_slice(&2u32.to_be_bytes());
        moov.children_mut()
            .unwrap()
            .push(Node::opaque(*b"pssh", vec![0; 24]));
        let mut out = Vec::new();
        moov.encode(&mut out);
        out
    }

    /// The fixture's fragments: one traf, a tfhd with a default sample size,
    /// and one trun with a data offset.
    fn encrypt_fragment(moof: &[u8], mdat: &[u8]) -> Vec<u8> {
        let (_, traf, _) = children(payload(moof))
            .unwrap()
            .into_iter()
            .find(|(kind, ..)| kind == b"traf")
            .unwrap();
        let mut parts: Vec<Vec<u8>> = children(payload(traf))
            .unwrap()
            .into_iter()
            .map(|(_, whole, _)| whole.to_vec())
            .collect();
        let tfhd = parts.iter().find(|part| &part[4..8] == b"tfhd").unwrap();
        let sample_size = read_u32(payload(tfhd), 12).unwrap();
        let trun_at = parts
            .iter()
            .position(|part| &part[4..8] == b"trun")
            .unwrap();
        let count = read_u32(payload(&parts[trun_at]), 4).unwrap();

        let mut senc = count.to_be_bytes().to_vec();
        for _ in 0..count {
            senc.extend_from_slice(&1u16.to_be_bytes());
            senc.extend_from_slice(&4u16.to_be_bytes());
            senc.extend_from_slice(&(sample_size - 4).to_be_bytes());
        }
        let senc = full(b"senc", 0, 0x2, &senc);
        let grown = senc.len() as i32;
        let offset_at = 8 + 8;
        let offset =
            i32::from_be_bytes(parts[trun_at][offset_at..offset_at + 4].try_into().unwrap());
        parts[trun_at][offset_at..offset_at + 4].copy_from_slice(&(offset + grown).to_be_bytes());
        parts.push(senc);
        let other: Vec<Vec<u8>> = children(payload(moof))
            .unwrap()
            .into_iter()
            .filter(|(kind, ..)| kind != b"traf")
            .map(|(_, whole, _)| whole.to_vec())
            .collect();
        let moof = boxed(
            b"moof",
            &[other.concat(), boxed(b"traf", &parts.concat())].concat(),
        );

        let mut mdat = mdat.to_vec();
        let start = header(&mdat).unwrap().unwrap().header_len;
        for sample in 0..count as usize {
            let at = start + sample * sample_size as usize;
            if let Some(range) = blocks(at + 4..at + sample_size as usize) {
                fake_cipher(&mut mdat[range]);
            }
        }
        [moof, mdat].concat()
    }

    #[test]
    fn headers_need_their_whole_size_field() {
        assert!(header(&[0, 0, 0]).unwrap().is_none());
        assert!(
            header(&[0, 0, 0, 1, b'm', b'd', b'a', b't', 0])
                .unwrap()
                .is_none()
        );
        assert!(header(&[0, 0, 0, 0, b'm', b'd', b'a', b't']).is_err());
        assert!(header(&[0, 0, 0, 4, b'm', b'd', b'a', b't']).is_err());
        let large = [0, 0, 0, 1, b'm', b'd', b'a', b't', 0, 0, 0, 0, 0, 0, 1, 0];
        assert_eq!(header(&large).unwrap().unwrap().size, 256);
    }

    #[test]
    fn a_child_overrunning_its_parent_is_an_error() {
        let mut parent = boxed(b"free", &[0; 4]);
        parent[3] = 200;
        assert!(children(&parent).is_err());
    }

    #[test]
    fn blocks_drop_a_trailing_partial_block() {
        assert_eq!(blocks(3..40), Some(3..35));
        assert_eq!(blocks(0..15), None);
    }

    fn track() -> Track {
        Track {
            id: 1,
            iv_size: 0,
            default_sample_size: 0,
        }
    }

    fn moof(traf_children: &[Vec<u8>]) -> Vec<u8> {
        boxed(b"moof", &boxed(b"traf", &traf_children.concat()))
    }

    fn tfhd_default_base(size: u32) -> Vec<u8> {
        let mut fields = 1u32.to_be_bytes().to_vec();
        fields.extend_from_slice(&size.to_be_bytes());
        full(b"tfhd", 0, DEFAULT_BASE_IS_MOOF | 0x10, &fields)
    }

    fn trun(count: u32, data_offset: i32) -> Vec<u8> {
        let mut fields = count.to_be_bytes().to_vec();
        fields.extend_from_slice(&data_offset.to_be_bytes());
        full(b"trun", 0, 0x1, &fields)
    }

    fn senc(subsamples: &[&[(u16, u32)]]) -> Vec<u8> {
        let mut fields = (subsamples.len() as u32).to_be_bytes().to_vec();
        for sample in subsamples {
            fields.extend_from_slice(&(sample.len() as u16).to_be_bytes());
            for (clear, protected) in *sample {
                fields.extend_from_slice(&clear.to_be_bytes());
                fields.extend_from_slice(&protected.to_be_bytes());
            }
        }
        full(b"senc", 0, 0x2, &fields)
    }

    #[test]
    fn subsamples_map_to_whole_blocks_inside_the_mdat() {
        let parts = |data_offset| {
            moof(&[
                tfhd_default_base(40),
                trun(2, data_offset),
                senc(&[&[(4, 36)], &[(0, 20), (2, 18)]]),
            ])
        };
        let probe = parts(0);
        let data_offset = (probe.len() + 8) as i32;
        let moof = parts(data_offset);
        let mdat = boxed(b"mdat", &[0; 80]);
        let fragment = fragment(moof.clone(), 0, &mdat, &track(), 0).unwrap();
        let start = moof.len() + 8;
        assert_eq!(
            fragment.protected,
            vec![
                start + 4..start + 36,
                start + 40..start + 56,
                start + 62..start + 78
            ]
        );
        assert_eq!(fragment.bytes.len(), moof.len() + mdat.len());
    }

    #[test]
    fn malformed_fragments_are_errors_not_panics() {
        let mdat = boxed(b"mdat", &[0; 32]);
        let cases = [
            // The sample runs past the mdat.
            moof(&[tfhd_default_base(64), trun(1, 60), senc(&[&[]])]),
            // senc has fewer entries than samples.
            moof(&[tfhd_default_base(16), trun(2, 60), senc(&[&[(0, 16)]])]),
            // A subsample runs past its sample.
            moof(&[tfhd_default_base(16), trun(1, 60), senc(&[&[(0, 32)]])]),
            // No senc at all.
            moof(&[tfhd_default_base(16), trun(1, 60)]),
            // An absurd sample count.
            moof(&[tfhd_default_base(1), trun(u32::MAX, 60), senc(&[])]),
            // A truncated tfhd.
            moof(&[full(b"tfhd", 0, 0x10, &[0, 0, 0, 1])]),
        ];
        for moof in cases {
            assert!(fragment(moof, 0, &mdat, &track(), 0).is_err());
        }
    }

    #[test]
    fn an_absolute_base_offset_moves_with_the_init_segment() {
        let mut fields = 1u32.to_be_bytes().to_vec();
        fields.extend_from_slice(&1000u64.to_be_bytes());
        fields.extend_from_slice(&16u32.to_be_bytes());
        let tfhd = full(b"tfhd", 0, BASE_DATA_OFFSET | 0x10, &fields);
        // A senc without subsamples: the whole sample is protected.
        let senc = full(b"senc", 0, 0, &1u32.to_be_bytes());
        let moof = moof(&[tfhd, full(b"trun", 0, 0, &1u32.to_be_bytes()), senc]);
        // The moof was read at 900, so its sample is 100 bytes in, which the
        // mdat header and payload must cover.
        let padding = 100 - moof.len() - 8;
        let mut payload = vec![0; padding];
        payload.extend_from_slice(&[0; 16]);
        let mdat = boxed(b"mdat", &payload);
        let fragment = fragment(moof, 900, &mdat, &track(), 300).unwrap();
        assert_eq!(fragment.protected, vec![100..116]);
        let at = fragment
            .bytes
            .windows(8)
            .position(|window| window == 700u64.to_be_bytes())
            .unwrap();
        assert!(at > 0, "the base offset is rewritten to 1000 - 300");
    }

    #[test]
    fn pattern_encryption_is_refused() {
        let mut tenc = vec![1, 0, 0, 0, 0, 0x19, 1, 0];
        tenc.extend_from_slice(&[0; 16]);
        assert!(tenc_iv_size(&tenc).is_err());
        tenc[5] = 0;
        assert_eq!(tenc_iv_size(&tenc).unwrap(), 0);
        assert!(tenc_iv_size(&tenc[..10]).is_err());
    }
}
