//! One-writer / N-reader broadcast buffer.
//!
//! The input pump pushes PCM chunks in; every connected HTTP client gets its
//! own [`BroadcastReceiver`] cursor and reads chunks independently. Chunks are
//! evicted once the buffer exceeds the size cap, so a slow or stalled reader
//! can never block the writer — it simply skips forward to the oldest chunk
//! still retained (dropping audio, never wedging the stream). Every position is
//! tracked in absolute stream bytes so subscribers learn exactly where their
//! stream begins and how much a forced skip dropped — the sync engine needs
//! both to map a player's playback time to a content position.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

/// Result of a blocking receive.
pub enum RecvResult {
    Data {
        chunk: Vec<u8>,
        /// Bytes of stream this reader lost just before `chunk` because it
        /// lagged past retention (0 in the normal case).
        skipped_bytes: u64,
    },
    Closed,
}

/// Rolling buffer of recently-written PCM, addressed by monotonic sequence.
pub struct BroadcastBuffer {
    inner: Mutex<Inner>,
    condvar: Condvar,
}

struct Chunk {
    seq: u64,
    /// Absolute byte position of this chunk's first byte in the global stream.
    start: u64,
    data: Vec<u8>,
}

struct Inner {
    chunks: VecDeque<Chunk>,
    next_seq: u64,
    /// Total bytes ever pushed — the absolute position of the live write head.
    pushed: u64,
    total_bytes: usize,
    max_buffered: usize,
    closed: bool,
}

/// Default retention: ~4 MB, roughly 23 s of 44.1 kHz S16LE stereo.
pub const MAX_BUFFERED: usize = 4 * 1024 * 1024;

impl BroadcastBuffer {
    pub fn new(max_buffered: usize) -> Arc<Self> {
        Arc::new(BroadcastBuffer {
            inner: Mutex::new(Inner {
                chunks: VecDeque::new(),
                next_seq: 0,
                pushed: 0,
                total_bytes: 0,
                max_buffered: max_buffered.max(64 * 1024),
                closed: false,
            }),
            condvar: Condvar::new(),
        })
    }

    /// Append a chunk, evicting the oldest chunks past the size cap.
    pub fn push(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            return;
        }
        let chunk = Chunk {
            seq: g.next_seq,
            start: g.pushed,
            data: data.to_vec(),
        };
        g.next_seq += 1;
        g.pushed += data.len() as u64;
        g.total_bytes += data.len();
        g.chunks.push_back(chunk);
        while g.total_bytes > g.max_buffered {
            match g.chunks.pop_front() {
                Some(old) => g.total_bytes -= old.data.len(),
                None => break,
            }
        }
        self.condvar.notify_all();
    }

    /// Subscribe from the current write position — live only, no backlog.
    /// The receiver's [`BroadcastReceiver::position`] is captured under the
    /// same lock, so it is exactly the byte at which this reader's stream
    /// begins (no race with concurrent pushes).
    pub fn subscribe(self: &Arc<Self>) -> BroadcastReceiver {
        let g = self.inner.lock().unwrap();
        BroadcastReceiver {
            buf: Arc::clone(self),
            next_seq: g.next_seq,
            position: g.pushed,
        }
    }

    /// Wake all readers with [`RecvResult::Closed`] so they can exit.
    pub fn close(&self) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        self.condvar.notify_all();
    }
}

/// An independent cursor into a [`BroadcastBuffer`].
pub struct BroadcastReceiver {
    buf: Arc<BroadcastBuffer>,
    next_seq: u64,
    /// Absolute stream position of the next byte this reader will receive.
    position: u64,
}

impl BroadcastReceiver {
    /// Absolute stream position of the next byte this reader will receive; at
    /// subscribe time, the byte at which its stream begins.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Block until the next chunk is available, or the buffer is closed.
    pub fn recv_blocking(&mut self) -> RecvResult {
        let mut g = self.buf.inner.lock().unwrap();
        loop {
            if let Some(front) = g.chunks.front() {
                // Lagging reader: fast-forward to the oldest retained chunk,
                // recording how much stream it lost.
                let mut skipped_bytes = 0;
                if self.next_seq < front.seq {
                    skipped_bytes = front.start - self.position;
                    tracing::debug!(
                        "broadcast: reader lagging, skipping {} → {} ({skipped_bytes} bytes lost)",
                        self.next_seq,
                        front.seq
                    );
                    self.next_seq = front.seq;
                    self.position = front.start;
                }
                if self.next_seq < g.next_seq {
                    let idx = (self.next_seq - g.chunks.front().unwrap().seq) as usize;
                    let chunk = g.chunks[idx].data.clone();
                    self.next_seq += 1;
                    self.position += chunk.len() as u64;
                    return RecvResult::Data {
                        chunk,
                        skipped_bytes,
                    };
                }
            }
            if g.closed {
                return RecvResult::Closed;
            }
            g = self.buf.condvar.wait(g).unwrap();
        }
    }
}
