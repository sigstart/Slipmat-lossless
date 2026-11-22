// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The encrypted file, as a sequence of top-level boxes, turned into the
//! clear stream the decoder reads: the init segment first, then each
//! fragment once its samples are decrypted.
//!
//! [`Boxes`] cuts the download into whole boxes and remembers where the next
//! one starts, so an interrupted download resumes there with a `Range`
//! request. [`Clearer`] holds the init segment's details and pairs each `moof`
//! with its `mdat`; decryption itself happens on the fairplay thread.

use anyhow::{Context, Result, bail, ensure};

use crate::mp4::{self, FourCc, Fragment, MAX_BOX, Track};

/// Accumulates downloaded bytes and yields whole top-level boxes.
#[derive(Default)]
pub struct Boxes {
    pending: Vec<u8>,
    /// The file offset of `pending[0]`: where the next box starts.
    boundary: u64,
}

pub struct Whole {
    pub at: u64,
    pub kind: FourCc,
    pub bytes: Vec<u8>,
}

impl Boxes {
    pub fn push(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
    }

    /// Where to resume a download that stopped part way through a box.
    pub fn resume_at(&mut self) -> u64 {
        self.pending.clear();
        self.boundary
    }

    /// Whether the download stopped exactly between boxes.
    pub fn is_between_boxes(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn next(&mut self) -> Result<Option<Whole>> {
        let Some(header) = mp4::header(&self.pending)? else {
            return Ok(None);
        };
        ensure!(header.size <= MAX_BOX, "MP4 box too large");
        let size = usize::try_from(header.size)?;
        if self.pending.len() < size {
            return Ok(None);
        }
        let rest = self.pending.split_off(size);
        let bytes = std::mem::replace(&mut self.pending, rest);
        let at = self.boundary;
        self.boundary += header.size;
        Ok(Some(Whole {
            at,
            kind: header.kind,
            bytes,
        }))
    }
}

pub enum Piece {
    /// The clear init segment, sent before anything else.
    Init(Vec<u8>),
    /// A fragment to decrypt, which started at `at` in the file.
    Fragment { at: u64, fragment: Fragment },
}

/// Turns the file's boxes into clear pieces, in order.
#[derive(Default)]
pub struct Clearer {
    ftyp: Option<Vec<u8>>,
    track: Option<Track>,
    moof: Option<(u64, Vec<u8>)>,
    /// How much earlier each later box sits in the output than in the file.
    shift: u64,
}

impl Clearer {
    /// The track, once the init segment has been read.
    pub fn track(&self) -> Option<Track> {
        self.track
    }

    pub fn push(&mut self, whole: Whole) -> Result<Option<Piece>> {
        let Whole { at, kind, bytes } = whole;
        let Some(track) = self.track else {
            return match &kind {
                b"ftyp" => {
                    self.ftyp = Some(bytes);
                    Ok(None)
                }
                b"moov" => {
                    let (init, track) = mp4::clear_init(self.ftyp.as_deref(), &bytes)?;
                    self.track = Some(track);
                    self.shift = (at + bytes.len() as u64)
                        .checked_sub(init.len() as u64)
                        .context("the clear init segment grew")?;
                    Ok(Some(Piece::Init(init)))
                }
                b"moof" | b"mdat" => bail!("an MP4 fragment before the init segment"),
                _ => Ok(None),
            };
        };
        match &kind {
            b"moof" => {
                ensure!(self.moof.is_none(), "an MP4 fragment without its mdat");
                self.moof = Some((at, bytes));
                Ok(None)
            }
            b"mdat" => {
                let (moof_at, moof) = self.moof.take().context("an mdat without its moof")?;
                let fragment = mp4::fragment(moof, moof_at, &bytes, &track, self.shift)?;
                Ok(Some(Piece::Fragment {
                    at: moof_at,
                    fragment,
                }))
            }
            // Not forwarded, so everything after it moves up by its size.
            _ => {
                self.shift += bytes.len() as u64;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::tests::{boxed, encrypt_like_apple, fake_cipher};
    use slipmat_playback::AlacDecoder;

    const CLEAR: &[u8] =
        include_bytes!("../../slipmat-playback/tests/fixtures/stereo-16-fragmented.m4a");

    /// Runs an encrypted file through the provider's path, in uneven chunks.
    fn clear_stream(encrypted: &[u8], decrypt: bool) -> (Vec<u8>, usize) {
        let mut boxes = Boxes::default();
        let mut clearer = Clearer::default();
        let mut out = Vec::new();
        let mut decrypted = 0;
        for chunk in encrypted.chunks(7) {
            boxes.push(chunk);
            while let Some(whole) = boxes.next().unwrap() {
                match clearer.push(whole).unwrap() {
                    Some(Piece::Init(init)) => out.extend(init),
                    Some(Piece::Fragment { mut fragment, .. }) => {
                        for range in &fragment.protected {
                            if decrypt {
                                fake_cipher(&mut fragment.bytes[range.clone()]);
                            }
                            decrypted += 1;
                        }
                        out.extend(fragment.bytes);
                    }
                    None => {}
                }
            }
        }
        assert!(boxes.is_between_boxes());
        (out, decrypted)
    }

    fn pcm(file: Vec<u8>) -> anyhow::Result<Vec<i32>> {
        let mut decoder = AlacDecoder::open_fragmented(std::io::Cursor::new(file))?;
        let mut samples = Vec::new();
        while let Some(block) = decoder.next_block()? {
            samples.extend_from_slice(block);
        }
        Ok(samples)
    }

    #[test]
    fn an_apple_style_encrypted_track_decodes_to_the_clear_original() {
        let encrypted = encrypt_like_apple(CLEAR);
        let (clear, decrypted) = clear_stream(&encrypted, true);
        assert_eq!(decrypted, 2, "one protected range per fragment's sample");
        let original = pcm(CLEAR.to_vec()).unwrap();
        assert!(!original.is_empty());
        assert_eq!(pcm(clear).unwrap(), original);
    }

    #[test]
    fn the_same_track_left_encrypted_does_not_decode_to_it() {
        let (scrambled, _) = clear_stream(&encrypt_like_apple(CLEAR), false);
        assert_ne!(pcm(scrambled).ok(), Some(pcm(CLEAR.to_vec()).unwrap()));
    }

    #[test]
    fn the_init_segment_reports_its_alac_settings_and_fragments_their_frames() {
        let mut boxes = Boxes::default();
        let mut clearer = Clearer::default();
        boxes.push(&encrypt_like_apple(CLEAR));
        let mut frames = 0;
        while let Some(whole) = boxes.next().unwrap() {
            if let Some(Piece::Fragment { fragment, .. }) = clearer.push(whole).unwrap() {
                assert_eq!(fragment.shape.description, 0);
                frames += fragment.shape.frames;
            }
        }
        let track = clearer.track().unwrap();
        let config = track.configs[0].unwrap();
        assert_eq!((config.bit_depth, config.channels), (16, 2));
        // Two descriptions, as Apple's files carry; the fixture's are the same.
        assert_eq!(track.configs[1], Some(config));
        let protection = track.protections[0].unwrap();
        assert_eq!(protection.key_id, [0x11; 16]);
        assert_eq!(protection.iv_size, 0);
        assert_eq!(protection.constant_iv, Some((16, [0x22; 16])));
        assert_eq!(track.protections[1], Some(protection));
        // Counted from the sample durations, the frames are the decoder's.
        let decoded = pcm(CLEAR.to_vec()).unwrap().len() as u64 / u64::from(config.channels);
        assert_eq!(frames, decoded);
    }

    #[test]
    fn boxes_split_across_chunks_come_out_whole() {
        let first = boxed(b"free", &[1; 10]);
        let second = boxed(b"skip", &[2; 3]);
        let stream = [first.clone(), second.clone()].concat();
        let mut boxes = Boxes::default();
        let mut out = Vec::new();
        for chunk in stream.chunks(5) {
            boxes.push(chunk);
            while let Some(whole) = boxes.next().unwrap() {
                out.push((whole.at, whole.kind, whole.bytes));
            }
        }
        assert_eq!(
            out,
            vec![
                (0, *b"free", first.clone()),
                (first.len() as u64, *b"skip", second)
            ]
        );
        assert!(boxes.is_between_boxes());
    }

    #[test]
    fn an_interrupted_box_resumes_from_its_start() {
        let mut boxes = Boxes::default();
        boxes.push(&boxed(b"free", &[0; 4]));
        boxes.push(&boxed(b"mdat", &[0; 100])[..20]);
        assert!(boxes.next().unwrap().is_some());
        assert!(boxes.next().unwrap().is_none());
        assert!(!boxes.is_between_boxes());
        assert_eq!(boxes.resume_at(), 12);
        assert!(boxes.is_between_boxes());
    }

    #[test]
    fn an_oversized_box_is_refused_before_it_is_buffered() {
        let mut boxes = Boxes::default();
        boxes.push(&[0, 0, 0, 1, b'm', b'd', b'a', b't', 0, 0, 0, 1, 0, 0, 0, 0]);
        assert!(boxes.next().is_err());
    }

    #[test]
    fn fragments_need_the_init_segment_and_each_other() {
        let whole = |kind: &[u8; 4]| Whole {
            at: 0,
            kind: *kind,
            bytes: boxed(kind, &[]),
        };
        assert!(Clearer::default().push(whole(b"moof")).is_err());
        assert!(Clearer::default().push(whole(b"mdat")).is_err());
        assert!(Clearer::default().push(whole(b"free")).unwrap().is_none());
    }
}
