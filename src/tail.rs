//! The tail buffer.
//!
//! A bounded tail of one stream, kept as bytes and decoded
//! late, with the bounds it is kept against.

use std::sync::Mutex;

/// The widest tail of one task stream the report carries, in
/// characters. The tail buffer's text trims its decode to the
/// last TAIL_CHARS characters.
pub const TAIL_CHARS: usize = 4000;
/// The byte bound the tail buffer holds a stream's tail against.
/// The buffer cuts back to this bound once it stands at twice
/// it, so the retained window moves about once per bound of new
/// bytes instead of once per chunk.
pub const TAIL_BYTES: usize = 1 << 16;

/// A bounded tail of one stream, kept as bytes and decoded late.
pub struct TailBuffer {
    buf: Mutex<Vec<u8>>,
    limit: usize,
}

impl TailBuffer {
    pub fn new() -> TailBuffer {
        TailBuffer {
            buf: Mutex::new(Vec::new()),
            limit: TAIL_BYTES,
        }
    }

    pub fn feed(&self, chunk: &[u8]) {
        let mut buf = self.buf.lock().unwrap();
        buf.extend_from_slice(chunk);
        // The drain waits until the buffer stands at twice
        // the limit and then cuts back to the limit, so the
        // retained window moves about once per limit of new
        // bytes instead of once per chunk. text() keeps only
        // the last TAIL_CHARS characters, and a buffer at the
        // limit already decodes to more characters than that,
        // so the deeper head the batching leaves standing is
        // discarded unread either way.
        if buf.len() >= 2 * self.limit {
            let overflow = buf.len() - self.limit;
            buf.drain(..overflow);
        }
    }

    pub fn text(&self) -> String {
        let raw = {
            let buf = self.buf.lock().unwrap();
            buf.clone()
        };
        let decoded = String::from_utf8_lossy(&raw).into_owned();
        let count = decoded.chars().count();
        if count <= TAIL_CHARS {
            decoded
        } else {
            decoded.chars().skip(count - TAIL_CHARS).collect()
        }
    }
}

impl Default for TailBuffer {
    fn default() -> Self {
        Self::new()
    }
}
