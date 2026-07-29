//! One-writer / N-reader broadcast buffer.
//!
//! The input pump pushes PCM chunks in; every connected HTTP client gets its
//! own [`BroadcastReceiver`] cursor and reads chunks independently. Chunks are
//! evicted once the buffer exceeds [`MAX_BUFFERED`] bytes, so a slow or stalled
//! reader can never block the writer — it simply skips forward to the oldest
//! chunk still retained (dropping audio, never wedging the stream).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

/// Result of a blocking receive.
pub enum RecvResult {
    Data(Vec<u8>),
    Closed,
}

/// Rolling buffer of recently-written PCM, addressed by monotonic sequence.
pub struct BroadcastBuffer {
    inner: Mutex<Inner>,
    condvar: Condvar,
}

struct Inner {
    chunks: VecDeque<(u64, Vec<u8>)>, // (seq, payload)
    next_seq: u64,
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
        let seq = g.next_seq;
        g.next_seq += 1;
        g.total_bytes += data.len();
        g.chunks.push_back((seq, data.to_vec()));
        while g.total_bytes > g.max_buffered {
            match g.chunks.pop_front() {
                Some((_, old)) => g.total_bytes -= old.len(),
                None => break,
            }
        }
        self.condvar.notify_all();
    }

    /// Subscribe from the current write position — live only, no backlog.
    pub fn subscribe(self: &Arc<Self>) -> BroadcastReceiver {
        let next_seq = self.inner.lock().unwrap().next_seq;
        BroadcastReceiver {
            buf: Arc::clone(self),
            next_seq,
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
}

impl BroadcastReceiver {
    /// Block until the next chunk is available, or the buffer is closed.
    pub fn recv_blocking(&mut self) -> RecvResult {
        let mut g = self.buf.inner.lock().unwrap();
        loop {
            if let Some(&(front_seq, _)) = g.chunks.front() {
                // Lagging reader: fast-forward to the oldest retained chunk.
                if self.next_seq < front_seq {
                    tracing::debug!(
                        "broadcast: reader lagging, skipping {} → {front_seq}",
                        self.next_seq
                    );
                    self.next_seq = front_seq;
                }
                if self.next_seq < g.next_seq {
                    let idx = (self.next_seq - front_seq) as usize;
                    let chunk = g.chunks[idx].1.clone();
                    self.next_seq += 1;
                    return RecvResult::Data(chunk);
                }
            }
            if g.closed {
                return RecvResult::Closed;
            }
            g = self.buf.condvar.wait(g).unwrap();
        }
    }
}
