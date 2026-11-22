// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{self, Cursor, Read};

use slipmat_playback::{AlacDecoder, DecodeStep, PcmFormat, Undecodable};

const STEREO_16: &[u8] = include_bytes!("fixtures/stereo-16.m4a");
const STEREO_16_NEXT: &[u8] = include_bytes!("fixtures/stereo-16-next.m4a");
const STEREO_24: &[u8] = include_bytes!("fixtures/stereo-24.m4a");
const STEREO_16_FRAGMENTED: &[u8] = include_bytes!("fixtures/stereo-16-fragmented.m4a");
const STANDARD_PACKET_FRAMES: usize = 4096;

fn open(bytes: &'static [u8]) -> AlacDecoder {
    AlacDecoder::open(Box::new(Cursor::new(bytes))).unwrap()
}

fn check_frames(block: &[i32], start: usize, frames: usize, depth: u8, steps: [i32; 2]) {
    assert_eq!(block.len(), frames * 2);
    let minimum = -(1_i32 << (depth - 1));
    let maximum = -minimum - 1;
    for (offset, pair) in block.chunks_exact(2).enumerate() {
        let frame = (start + offset) as i32;
        assert_eq!(
            pair,
            [minimum + frame * steps[0], maximum - frame * steps[1]],
            "frame {frame}",
        );
    }
}

#[test]
fn decodes_exact_stereo_samples_across_packet_boundaries() {
    for (fixture, rate, depth, tail, steps) in [
        (STEREO_16, 44_100, 16, 3, [3, 7]),
        (STEREO_24, 96_000, 24, 17, [19, 31]),
    ] {
        let mut decoder = open(fixture);
        assert_eq!(
            decoder.format(),
            PcmFormat {
                sample_rate: rate,
                bits_per_sample: depth,
                channels: 2
            },
        );
        check_frames(
            decoder.next_block().unwrap().unwrap(),
            0,
            STANDARD_PACKET_FRAMES,
            depth,
            steps,
        );
        check_frames(
            decoder.next_block().unwrap().unwrap(),
            STANDARD_PACKET_FRAMES,
            tail,
            depth,
            steps,
        );
        assert!(decoder.next_block().unwrap().is_none());
        assert!(decoder.next_block().unwrap().is_none());
    }
}

#[test]
fn hands_off_to_one_prefetched_block_without_losing_or_repeating_frames() {
    let mut current = open(STEREO_16);
    let mut next = open(STEREO_16_NEXT);
    assert_eq!(current.format(), next.format());

    check_frames(
        current.next_block().unwrap().unwrap(),
        0,
        STANDARD_PACKET_FRAMES,
        16,
        [3, 7],
    );

    let tail = current.next_block().unwrap().unwrap();
    let prefetched = next.next_block().unwrap().unwrap();
    check_frames(tail, STANDARD_PACKET_FRAMES, 3, 16, [3, 7]);
    assert!(current.next_block().unwrap().is_none());
    check_frames(prefetched, 4099, STANDARD_PACKET_FRAMES, 16, [3, 7]);
    check_frames(next.next_block().unwrap().unwrap(), 8195, 5, 16, [3, 7]);
    assert!(next.next_block().unwrap().is_none());
}

#[test]
fn truncated_media_reports_an_error_instead_of_clean_eof() {
    let mut decoder = open(&STEREO_16[..STEREO_16.len() - 1]);
    loop {
        match decoder.next_block() {
            Ok(Some(_)) => {}
            Ok(None) => panic!("truncated media returned clean EOF"),
            Err(_) => break,
        }
    }
    assert!(decoder.next_block().unwrap().is_none());
}

#[test]
fn streams_exact_pcm_across_non_seekable_fragments() {
    struct ShortReads(&'static [u8]);

    impl Read for ShortReads {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let limit = buf.len().min(37);
            self.0.read(&mut buf[..limit])
        }
    }

    let mut decoder = AlacDecoder::open_fragmented(ShortReads(STEREO_16_FRAGMENTED)).unwrap();

    assert_eq!(
        decoder.format(),
        PcmFormat {
            sample_rate: 44_100,
            bits_per_sample: 16,
            channels: 2
        }
    );
    check_frames(
        decoder.next_block().unwrap().unwrap(),
        0,
        STANDARD_PACKET_FRAMES,
        16,
        [3, 7],
    );
    check_frames(
        decoder.next_block().unwrap().unwrap(),
        STANDARD_PACKET_FRAMES,
        3,
        16,
        [3, 7],
    );
    assert!(decoder.next_block().unwrap().is_none());
}

#[test]
fn rejects_truncated_fragmented_streams() {
    let last_mdat = STEREO_16_FRAGMENTED
        .windows(4)
        .rposition(|kind| kind == b"mdat")
        .unwrap()
        - 4;
    let mut partial_header = STEREO_16_FRAGMENTED.to_vec();
    partial_header.extend_from_slice(&[0, 0, 0, 8, b'f']);
    for input in [
        &STEREO_16_FRAGMENTED[..last_mdat],
        &STEREO_16_FRAGMENTED[..STEREO_16_FRAGMENTED.len() - 1],
        &partial_header,
    ] {
        let mut decoder = AlacDecoder::open_fragmented(Cursor::new(input.to_vec())).unwrap();
        assert!(decoder.next_block().unwrap().is_some());
        loop {
            match decoder.next_block() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("truncated fragmented media returned clean EOF"),
                Err(error) => {
                    assert!(error.downcast_ref::<Undecodable>().is_some(), "{error:#}");
                    break;
                }
            }
        }
    }
}

/// What a wrongly decrypted fragment looks like to the decoder: valid boxes,
/// audio that is noise. It must say so, and how far it got first.
#[test]
fn a_corrupt_frame_is_an_audio_failure_after_the_audio_before_it() {
    let last_mdat = STEREO_16_FRAGMENTED
        .windows(4)
        .rposition(|kind| kind == b"mdat")
        .unwrap()
        + 4;
    let mut input = STEREO_16_FRAGMENTED.to_vec();
    // 0x40 opens each frame with a CCE element, which ALAC refuses. Not 0xff:
    // that is the end tag, and decodes silently to empty frames.
    for byte in &mut input[last_mdat..] {
        *byte = 0x40;
    }
    let mut decoder = AlacDecoder::open_fragmented(Cursor::new(input)).unwrap();
    let error = loop {
        match decoder.next_block() {
            Ok(Some(_)) => {}
            Ok(None) => panic!("noise decoded as audio"),
            Err(error) => break error,
        }
    };
    let failed = error.downcast_ref::<Undecodable>().copied();
    assert!(
        matches!(
            failed,
            Some(Undecodable {
                step: DecodeStep::Audio,
                decoded_ms,
                decoded_frames
            }) if decoded_ms > 0 && decoded_frames > 0
        ),
        "{failed:?}: {error:#}"
    );
}

/// The provider opens the decoder before the audio output, so input that is
/// not a fragmented stream must fail here, before anything reaches PulseAudio.
#[test]
fn invalid_input_fails_while_opening_the_decoder() {
    for input in [&[][..], b"invalid M4A stream", STEREO_16] {
        let Err(error) = AlacDecoder::open_fragmented(Cursor::new(input.to_vec())) else {
            panic!("opened {} bytes of invalid input", input.len());
        };
        assert!(format!("{error:#}").contains("reading MP4"), "{error:#}");
    }
}
