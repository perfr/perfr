// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The zstd stream of a recording made with `perf record -z`.
//!
//! `perf` compresses what it reads from the kernel's rings through one
//! zstd stream and flushes it into a record whenever it writes: a single
//! frame for the whole recording, never ended, cut into records wherever
//! a write fell. So the pieces are decoded as one stream, a block at a
//! time, and what a piece decodes to is handed out at once.
//!
//! `ruzstd`'s frame decoder holds a window of decoded bytes back until the
//! frame ends, which this frame never does; the last half megabyte of
//! records would stay inside it. Its block decoder is driven directly
//! here, and the window is put back after each drain.

use ruzstd::decoding::block_decoder;
use ruzstd::decoding::scratch::DecoderScratch;
use ruzstd::frame::{read_frame_header, ReadFrameHeaderError};

/// The longest frame header: magic, descriptor, window, dictionary id,
/// content size.
const FRAME_HEADER_MAX: usize = 18;
const BLOCK_HEADER_LEN: usize = 3;
/// The most one piece may decode to. A piece is under 64 KiB and holds
/// what a ring gave up in one read; a block of one repeated byte is four
/// bytes for 128 KiB, and a piece made of those would decode to 2 GiB.
const PIECE_LIMIT: usize = 256 << 20;

#[derive(Default)]
pub struct Decoder {
    /// Compressed bytes not decoded yet: a block is decoded whole.
    packed: Vec<u8>,
    frame: Option<Frame>,
}

struct Frame {
    scratch: DecoderScratch,
    window: usize,
    /// How many bytes at the front of the scratch buffer were handed out
    /// by an earlier call and are only there to be copied from.
    kept: usize,
}

impl Decoder {
    /// Decodes what `piece` completes and appends it to `out`.
    pub fn push(&mut self, piece: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        self.packed.extend_from_slice(piece);
        let limit = out.len().saturating_add(PIECE_LIMIT);
        let mut at = 0;
        loop {
            let rest = &self.packed[at..];
            let Some(frame) = self.frame.as_mut() else {
                let mut source = rest;
                match read_frame_header(&mut source) {
                    Ok((header, used)) => {
                        let window = header
                            .header
                            .window_size()
                            .map_err(|e| format!("zstd: {e}"))?;
                        let window =
                            usize::try_from(window).map_err(|_| "zstd: window too large")?;
                        self.frame = Some(Frame {
                            scratch: DecoderScratch::new(window),
                            window,
                            kept: 0,
                        });
                        at += usize::from(used);
                        continue;
                    }
                    // Not all of a header yet.
                    Err(_) if rest.len() < FRAME_HEADER_MAX => break,
                    Err(ReadFrameHeaderError::SkipFrame { length, .. }) => {
                        let end = 8usize.saturating_add(length as usize);
                        if rest.len() < end {
                            break;
                        }
                        at += end;
                        continue;
                    }
                    Err(e) => return Err(format!("zstd: {e}")),
                }
            };
            if rest.len() < BLOCK_HEADER_LEN {
                break;
            }
            let mut source = rest;
            let mut decoder = block_decoder::new();
            let (header, header_len) = decoder
                .read_block_header(&mut source)
                .map_err(|e| format!("zstd: {e}"))?;
            if source.len() < header.content_size as usize {
                break;
            }
            let body = decoder
                .decode_block_content(&header, &mut frame.scratch, &mut source)
                .map_err(|e| format!("zstd: {e}"))?;
            at += usize::from(header_len) + body as usize;
            frame.drain(out);
            if out.len() > limit {
                return Err("zstd: a compressed record decodes to more than 256 MiB".into());
            }
            if header.last_block {
                // perf does not end its frame, but a frame that ends is
                // followed by another or by nothing.
                self.frame = None;
            }
        }
        self.packed.drain(..at);
        Ok(())
    }
}

impl Frame {
    /// Hands out what has been decoded since the last call, and leaves the
    /// window in place for the blocks to come to copy from.
    fn drain(&mut self, out: &mut Vec<u8>) {
        let all = self.scratch.buffer.drain();
        out.extend_from_slice(&all[self.kept..]);
        let keep = &all[all.len().saturating_sub(self.window)..];
        self.scratch.buffer.push(keep);
        self.kept = keep.len();
    }
}
