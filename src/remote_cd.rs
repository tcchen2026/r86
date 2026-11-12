//! Host-only synchronous CD reads supplied by a dedicated socket connection.
//! The core owns reads; existing socket workers deliver replies and disconnects.
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const CHUNK_BYTES: usize = 256 * 1024;
const CACHE_CHUNKS: usize = 64 * 1024 * 1024 / CHUNK_BYTES;
const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
pub struct ReadRequest {
    pub id: u64,
    pub offset: usize,
    pub length: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub requests: u64,
    pub chunks: u64,
    pub bytes: u64,
    pub stall_us: u64,
    pub max_stall_us: u64,
}

struct Pending {
    request: ReadRequest,
    data: Option<Vec<u8>>,
}

#[derive(Default)]
struct State {
    error: Option<String>,
    pending: Option<Pending>,
    cache: VecDeque<(usize, Vec<u8>)>,
    stats: Stats,
}

struct Inner {
    bytes: usize,
    send: Box<dyn Fn(ReadRequest) -> Result<(), String> + Send + Sync>,
    state: Mutex<State>,
    ready: Condvar,
    timeout: Duration,
    cache_chunks: usize,
}

#[derive(Clone)]
pub struct RemoteCd(Arc<Inner>);

impl RemoteCd {
    pub fn new(
        bytes: u64,
        send: impl Fn(ReadRequest) -> Result<(), String> + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let bytes = usize::try_from(bytes).map_err(|_| "Remote CD is too large for this host")?;
        if bytes == 0 || bytes % 2048 != 0 || bytes as u64 / 2048 > u64::from(u32::MAX) {
            return Err(
                "Remote CD size must be a nonzero multiple of 2048 within ATAPI capacity".into(),
            );
        }
        Ok(Self(Arc::new(Inner {
            bytes,
            send: Box::new(send),
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            timeout: READ_TIMEOUT,
            cache_chunks: CACHE_CHUNKS,
        })))
    }

    pub fn same_disc(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn error(&self) -> Option<String> {
        self.0.state.lock().unwrap().error.clone()
    }

    pub fn len(&self) -> usize {
        if self.0.state.lock().unwrap().error.is_some() {
            0
        } else {
            self.0.bytes
        }
    }

    pub fn stats(&self) -> Stats {
        self.0.state.lock().unwrap().stats
    }

    /// Wake a blocked core before enqueueing any ordinary disconnect request.
    pub fn disconnect(&self, error: &str) {
        let mut state = self.0.state.lock().unwrap();
        Self::fail(&mut state, error);
        self.0.ready.notify_one();
    }

    fn fail(state: &mut State, error: &str) {
        if state.error.is_none() {
            state.error = Some(error.to_string());
        }
        state.pending = None;
        state.cache.clear();
    }

    /// Binary replies carry no id: exactly one requested chunk may be pending.
    pub fn reply(&self, data: Vec<u8>) -> Result<(), String> {
        let mut state = self.0.state.lock().unwrap();
        let error = match &mut state.pending {
            Some(p) if p.data.is_none() && data.len() == p.request.length => {
                p.data = Some(data);
                self.0.ready.notify_one();
                return Ok(());
            }
            Some(p) => format!(
                "Remote CD reply has {} bytes; expected {} once",
                data.len(),
                p.request.length
            ),
            None => "Remote CD reply has no outstanding read".into(),
        };
        Self::fail(&mut state, &error);
        self.0.ready.notify_one();
        Err(error)
    }

    pub fn read_error(&self, id: u64, error: &str) -> Result<(), String> {
        let mut state = self.0.state.lock().unwrap();
        let valid = state
            .pending
            .as_ref()
            .is_some_and(|p| p.request.id == id && p.data.is_none());
        let error = if valid {
            format!("Remote CD read failed: {error}")
        } else {
            "Remote CD error has no matching outstanding read".into()
        };
        Self::fail(&mut state, &error);
        self.0.ready.notify_one();
        if valid { Ok(()) } else { Err(error) }
    }

    pub fn read_into(&self, offset: usize, len: usize, out: &mut Vec<u8>) -> io::Result<()> {
        if offset.checked_add(len).is_none_or(|end| end > self.0.bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Remote CD read out of range",
            ));
        }
        let original = out.len();
        let result = self.copy_chunks(offset, len, out);
        if result.is_err() {
            out.truncate(original);
        }
        result
    }

    fn copy_chunks(&self, mut offset: usize, mut len: usize, out: &mut Vec<u8>) -> io::Result<()> {
        while len != 0 {
            let chunk = offset / CHUNK_BYTES * CHUNK_BYTES;
            let mut state = self.0.state.lock().unwrap();
            if let Some(error) = &state.error {
                return Err(io::Error::other(error.clone()));
            }
            if !state.cache.iter().rev().any(|(at, _)| *at == chunk) {
                assert!(
                    state.pending.is_none(),
                    "only the core thread reads a remote CD"
                );
                state.stats.requests += 1;
                let request = ReadRequest {
                    id: state.stats.requests,
                    offset: chunk,
                    length: CHUNK_BYTES.min(self.0.bytes - chunk),
                };
                state.pending = Some(Pending {
                    request,
                    data: None,
                });
                let start = Instant::now();
                drop(state);
                if let Err(error) = (self.0.send)(request) {
                    self.disconnect(&error);
                }
                state = self.0.state.lock().unwrap();
                while state.error.is_none() && state.pending.as_ref().unwrap().data.is_none() {
                    let left = self.0.timeout.saturating_sub(start.elapsed());
                    if left.is_zero() {
                        Self::fail(&mut state, "Remote CD read timed out after 10 seconds");
                        break;
                    }
                    state = self.0.ready.wait_timeout(state, left).unwrap().0;
                }
                let elapsed = start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                state.stats.stall_us += elapsed;
                state.stats.max_stall_us = state.stats.max_stall_us.max(elapsed);
                if let Some(error) = &state.error {
                    return Err(io::Error::other(error.clone()));
                }
                let data = state.pending.take().unwrap().data.unwrap();
                state.stats.chunks += 1;
                state.stats.bytes += data.len() as u64;
                if state.cache.len() == self.0.cache_chunks {
                    state.cache.pop_front();
                }
                state.cache.push_back((chunk, data));
            }
            let data = &state
                .cache
                .iter()
                .rev()
                .find(|(at, _)| *at == chunk)
                .unwrap()
                .1;
            let begin = offset - chunk;
            let copied = len.min(data.len() - begin);
            out.extend_from_slice(&data[begin..begin + copied]);
            offset += copied;
            len -= copied;
        }
        Ok(())
    }
}

impl std::fmt::Debug for RemoteCd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.0.state.lock().unwrap();
        f.debug_struct("RemoteCd")
            .field("bytes", &self.0.bytes)
            .field("error", &state.error)
            .field("stats", &state.stats)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn source(bytes: usize) -> (RemoteCd, mpsc::Receiver<ReadRequest>) {
        let (tx, rx) = mpsc::channel();
        (
            RemoteCd::new(bytes as u64, move |r| tx.send(r).map_err(|e| e.to_string())).unwrap(),
            rx,
        )
    }

    #[test]
    fn remote_cd_aligned_cache_short_tail_and_fifo_eviction() {
        let (mut remote, rx) = source(CHUNK_BYTES * 2 + 2048);
        Arc::get_mut(&mut remote.0).unwrap().cache_chunks = 2;
        let sender = remote.clone();
        let worker = std::thread::spawn(move || {
            for (id, chunk, length) in [
                (1, 0, CHUNK_BYTES),
                (2, 1, CHUNK_BYTES),
                (3, 2, 2048),
                (4, 0, CHUNK_BYTES),
            ] {
                let r = rx.recv().unwrap();
                assert_eq!(
                    (r.id, r.offset, r.length),
                    (id, chunk * CHUNK_BYTES, length)
                );
                sender.reply(vec![chunk as u8; length]).unwrap();
            }
        });
        let mut out = vec![9];
        remote.read_into(CHUNK_BYTES - 5, 15, &mut out).unwrap();
        assert_eq!(out, [&[9][..], &[0; 5], &[1; 10]].concat());
        remote.read_into(0, 1, &mut Vec::new()).unwrap();
        assert_eq!(remote.stats().chunks, 2); // Hit does not refetch or reorder FIFO.
        let mut tail = Vec::new();
        remote
            .read_into(CHUNK_BYTES * 2 + 2040, 8, &mut tail)
            .unwrap();
        assert_eq!(tail, [2; 8]);
        remote.read_into(0, 1, &mut Vec::new()).unwrap();
        worker.join().unwrap();
        assert_eq!(remote.stats().bytes, (CHUNK_BYTES * 3 + 2048) as u64);
        remote.disconnect("socket closed");
        assert_eq!(remote.len(), 0);
        assert!(remote.read_into(0, 1, &mut Vec::new()).is_err());
        assert_eq!(remote.stats().requests, 4);
    }

    #[test]
    fn remote_cd_wrong_length_discards_partial_read_and_never_retries() {
        let (remote, rx) = source(CHUNK_BYTES + 2048);
        let sender = remote.clone();
        let worker = std::thread::spawn(move || {
            let r = rx.recv().unwrap();
            sender.reply(vec![0xa5; r.length]).unwrap();
            let r = rx.recv().unwrap();
            assert!(sender.reply(vec![0; r.length - 1]).is_err());
        });
        let mut out = vec![7];
        assert!(remote.read_into(CHUNK_BYTES - 8, 16, &mut out).is_err());
        assert_eq!(out, [7]);
        assert!(remote.read_into(0, 1, &mut out).is_err());
        assert_eq!(remote.stats().requests, 2);
        assert_eq!(remote.len(), 0);
        worker.join().unwrap();
    }

    #[test]
    fn remote_cd_error_id_must_match_pending_read() {
        for matching in [true, false] {
            let (remote, rx) = source(2048);
            let sender = remote.clone();
            let worker = std::thread::spawn(move || {
                let r = rx.recv().unwrap();
                assert_eq!(
                    sender
                        .read_error(r.id + u64::from(!matching), "file removed")
                        .is_ok(),
                    matching
                );
            });
            assert!(remote.read_into(0, 2048, &mut Vec::new()).is_err());
            assert_eq!(remote.len(), 0);
            worker.join().unwrap();
        }
    }

    #[test]
    fn remote_cd_disconnect_wakes_a_blocked_read() {
        let (remote, rx) = source(2048);
        let sender = remote.clone();
        let worker = std::thread::spawn(move || {
            rx.recv().unwrap();
            sender.disconnect("socket closed");
        });
        let mut data = vec![7];
        let error = remote.read_into(0, 2048, &mut data).unwrap_err();
        assert_eq!(error.to_string(), "socket closed");
        assert_eq!(data, [7]);
        assert_eq!(remote.len(), 0);
        worker.join().unwrap();
    }
}
