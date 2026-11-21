// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    io::{self, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Checks top-level box boundaries as bytes pass through to Symphonia.
pub(crate) struct FragmentedM4a<R> {
    source: R,
    header: [u8; 16],
    pending_header: std::ops::Range<usize>,
    payload_left: u64,
    saw_fragment: bool,
    needs_media_data: bool,
    failed: bool,
    eof: Arc<AtomicBool>,
}

impl<R: Read> FragmentedM4a<R> {
    pub(crate) fn new(source: R, eof: Arc<AtomicBool>) -> Self {
        Self {
            source,
            header: [0; 16],
            pending_header: 0..0,
            payload_left: 0,
            saw_fragment: false,
            needs_media_data: false,
            failed: false,
            eof,
        }
    }

    fn next_header(&mut self) -> io::Result<bool> {
        // Symphonia treats UnexpectedEof while scanning top-level headers as
        // normal EOF, so report incomplete boxes as InvalidData.
        if self.source.read(&mut self.header[..1])? == 0 {
            if self.needs_media_data || !self.saw_fragment {
                return Err(invalid("expected a complete MP4 fragment"));
            }
            self.eof.store(true, Ordering::Relaxed);
            return Ok(false);
        }
        self.source
            .read_exact(&mut self.header[1..8])
            .map_err(truncated)?;
        let [a, b, c, d, e, f, g, h, ..] = self.header;
        let (size, header_len) = match u32::from_be_bytes([a, b, c, d]) {
            0 => return Err(invalid("fragmented MP4 requires sized boxes")),
            1 => {
                let mut extended_size = [0; 8];
                self.source
                    .read_exact(&mut extended_size)
                    .map_err(truncated)?;
                self.header[8..].copy_from_slice(&extended_size);
                (u64::from_be_bytes(extended_size), 16)
            }
            size => (u64::from(size), 8),
        };
        self.payload_left = size
            .checked_sub(header_len as u64)
            .ok_or_else(|| invalid("invalid MP4 box size"))?;
        match &[e, f, g, h] {
            b"moof" if self.needs_media_data => {
                return Err(invalid("MP4 fragment is missing its media data"));
            }
            b"moof" => {
                self.saw_fragment = true;
                self.needs_media_data = true;
            }
            b"mdat" if !self.saw_fragment => {
                return Err(invalid("MP4 media data precedes the first fragment"));
            }
            b"mdat" => self.needs_media_data = false,
            _ => {}
        }
        self.pending_header = 0..header_len;
        Ok(true)
    }

    fn read_box(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.eof.load(Ordering::Relaxed) {
            return Ok(0);
        }
        if self.pending_header.is_empty() && self.payload_left == 0 && !self.next_header()? {
            return Ok(0);
        }
        if !self.pending_header.is_empty() {
            let len = buf.len().min(self.pending_header.len());
            let end = self.pending_header.start + len;
            buf[..len].copy_from_slice(&self.header[self.pending_header.start..end]);
            self.pending_header.start = end;
            return Ok(len);
        }
        let limit = self.payload_left.min(buf.len() as u64) as usize;
        let count = self.source.read(&mut buf[..limit]).map_err(truncated)?;
        if count == 0 {
            return Err(invalid("truncated MP4 box"));
        }
        self.payload_left -= count as u64;
        Ok(count)
    }
}

impl<R: Read> Read for FragmentedM4a<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // The demuxer can ignore I/O errors while skipping a box.
        if self.failed {
            return Err(invalid("failed to read fragmented MP4"));
        }
        let result = self.read_box(buf);
        if let Err(error) = &result {
            self.failed = error.kind() != io::ErrorKind::Interrupted;
        }
        result
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn truncated(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        invalid("truncated MP4 box")
    } else {
        error
    }
}
