//! Deflate state shared between connections that negotiated no context takeover.
//!
//! Without context takeover nothing carries over from one message to the next, so a
//! connection only needs deflate state while a message is being compressed or
//! decompressed. Between messages the state sits in a [`DeflatePool`], and memory
//! scales with messages in flight instead of open connections.
use std::{
    fmt,
    sync::{Arc, Mutex, PoisonError},
};

use crossbeam_queue::ArrayQueue;

use super::{Deflate, Inflate};
use crate::CompressionLevel;

/// Idle compressors for one compression level and window size.
pub(super) type DeflateQueue = Arc<ArrayQueue<Deflate>>;
/// Idle decompressors for one window size.
pub(super) type InflateQueue = Arc<ArrayQueue<Inflate>>;

/// One queue per combination of settings, looked up when a connection is set up.
type Queues<K, T> = Mutex<Vec<(K, Arc<ArrayQueue<T>>)>>;

/// What a pooled compressor was built with. Only identical settings can share state.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DeflateKey {
    level: CompressionLevel,
    window_bits: Option<u8>,
}

/// Shares permessage-deflate state between connections that do not keep context.
///
/// When a direction of a connection negotiated no context takeover
/// (`server_no_context_takeover` or `client_no_context_takeover`), its compressor or
/// decompressor is taken from the pool for one message and returned afterwards. Cloning
/// the pool is cheap, and every clone shares the same state.
///
/// `capacity` bounds how many idle compressors, and separately how many idle
/// decompressors, are kept for each combination of settings. A message that finds the
/// pool empty builds new state, and state returned to a full pool is dropped. A
/// capacity of 0 keeps nothing.
///
/// # Many idle connections
///
/// A server can impose no context takeover in both directions, even on clients that
/// did not offer it, which RFC 7692 allows. With a pool, small socket buffers and a
/// compression threshold, an idle connection then costs about 2 KiB instead of the
/// roughly 370 KiB of a connection that keeps its own deflate state. The price is a
/// lower compression ratio, since no message can refer to earlier ones.
///
/// ```rust
/// use yawc::{DeflatePool, Options};
///
/// // One pool for the whole server; every connection gets a clone of `options`.
/// let pool = DeflatePool::new(64);
/// let options = Options::default()
///     .with_low_latency_compression()
///     .server_no_context_takeover()
///     .client_no_context_takeover()
///     .with_deflate_pool(pool)
///     .with_compression_threshold(64)
///     .with_read_buffer_capacity(1024)
///     .with_write_buffer_capacity(1024);
/// ```
///
/// Size the pool for the messages compressed at the same moment, roughly the number
/// of runtime worker threads, not for the number of connections.
#[derive(Clone)]
pub struct DeflatePool {
    shared: Arc<Shared>,
}

struct Shared {
    capacity: usize,
    // A handful of entries at most.
    deflate: Queues<DeflateKey, Deflate>,
    inflate: Queues<Option<u8>, Inflate>,
}

impl DeflatePool {
    /// Creates a pool that keeps up to `capacity` idle objects per setting.
    pub fn new(capacity: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                capacity,
                deflate: Mutex::new(Vec::new()),
                inflate: Mutex::new(Vec::new()),
            }),
        }
    }

    /// The queue for compressors with these settings, or `None` if nothing is kept.
    pub(super) fn deflate_queue(
        &self,
        level: CompressionLevel,
        window_bits: Option<u8>,
    ) -> Option<DeflateQueue> {
        let key = DeflateKey {
            level,
            window_bits: effective_window_bits(window_bits),
        };
        queue_for(&self.shared.deflate, key, self.shared.capacity)
    }

    /// The queue for decompressors with this window size, or `None` if nothing is kept.
    pub(super) fn inflate_queue(&self, window_bits: Option<u8>) -> Option<InflateQueue> {
        let key = effective_window_bits(window_bits);
        queue_for(&self.shared.inflate, key, self.shared.capacity)
    }
}

impl fmt::Debug for DeflatePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeflatePool")
            .field("capacity", &self.shared.capacity)
            .finish_non_exhaustive()
    }
}

/// Window bits only change the state with the `zlib` feature; without it every
/// connection uses the default window and shares one queue.
fn effective_window_bits(window_bits: Option<u8>) -> Option<u8> {
    if cfg!(feature = "zlib") {
        window_bits
    } else {
        None
    }
}

fn queue_for<K: PartialEq, T>(
    queues: &Queues<K, T>,
    key: K,
    capacity: usize,
) -> Option<Arc<ArrayQueue<T>>> {
    if capacity == 0 {
        return None;
    }
    // The list is only ever appended to, so a panic elsewhere cannot leave it
    // inconsistent and a poisoned lock is safe to use.
    let mut queues = queues.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, queue)) = queues.iter().find(|(existing, _)| *existing == key) {
        return Some(Arc::clone(queue));
    }
    let queue = Arc::new(ArrayQueue::new(capacity));
    queues.push((key, Arc::clone(&queue)));
    Some(queue)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::{
        compression::{CompressionConfig, WebSocketExtensions},
        Role, WebSocketError,
    };

    const NO_CONTEXT: &str =
        "permessage-deflate; server_no_context_takeover; client_no_context_takeover";

    fn config(extensions: &str, pool: &DeflatePool) -> CompressionConfig {
        let extensions = WebSocketExtensions::from_str(extensions).unwrap();
        CompressionConfig::resolve(
            Some(&extensions),
            Some(CompressionLevel::fast()),
            Role::Server,
        )
        .unwrap()
        .unwrap()
        .with_pool(Some(pool.clone()))
    }

    fn idle_compressors(pool: &DeflatePool) -> usize {
        pool.deflate_queue(CompressionLevel::fast(), None)
            .unwrap()
            .len()
    }

    fn idle_decompressors(pool: &DeflatePool) -> usize {
        pool.inflate_queue(None).unwrap().len()
    }

    #[test]
    fn state_returns_to_the_pool_after_each_message() {
        let pool = DeflatePool::new(4);
        let config = config(NO_CONTEXT, &pool);
        let mut compressor = config.compressor();
        let mut decompressor = config.decompressor();

        for _ in 0..3 {
            let wire = compressor.compress(b"hello hello hello", true).unwrap();
            assert_eq!(idle_compressors(&pool), 1);
            let plain = decompressor.decompress(&wire, true, 1024).unwrap();
            assert_eq!(plain.as_ref(), b"hello hello hello");
            assert_eq!(idle_decompressors(&pool), 1);
        }
    }

    #[test]
    fn a_fragmented_message_keeps_its_state_until_the_last_frame() {
        let pool = DeflatePool::new(4);
        let mut compressor = config(NO_CONTEXT, &pool).compressor();

        compressor.compress(b"first half ", false).unwrap();
        assert_eq!(idle_compressors(&pool), 0);
        compressor.compress(b"second half", true).unwrap();
        assert_eq!(idle_compressors(&pool), 1);
    }

    #[test]
    fn connections_reuse_each_others_state() {
        let pool = DeflatePool::new(4);
        let config = config(NO_CONTEXT, &pool);
        let mut connections = [config.compressor(), config.compressor()];
        let mut decompressor = config.decompressor();

        for (connection, message) in [
            (0, &b"from the first connection"[..]),
            (1, &b"from the second connection"[..]),
            (0, &b"first again"[..]),
        ] {
            let wire = connections[connection].compress(message, true).unwrap();
            assert_eq!(idle_compressors(&pool), 1);
            let plain = decompressor.decompress(&wire, true, 1024).unwrap();
            assert_eq!(plain.as_ref(), message);
        }
    }

    #[test]
    fn context_takeover_never_uses_the_pool() {
        let pool = DeflatePool::new(4);
        let mut compressor = config("permessage-deflate", &pool).compressor();
        compressor.compress(b"hello", true).unwrap();
        assert_eq!(idle_compressors(&pool), 0);
    }

    #[test]
    fn a_full_pool_drops_returned_state() {
        let pool = DeflatePool::new(1);
        let config = config(NO_CONTEXT, &pool);
        let mut first = config.compressor();
        let mut second = config.compressor();
        // Both take state before either returns it, so two exist at once.
        first.compress(b"a", false).unwrap();
        second.compress(b"b", false).unwrap();
        first.compress(b"", true).unwrap();
        second.compress(b"", true).unwrap();
        assert_eq!(idle_compressors(&pool), 1);
    }

    #[test]
    fn failed_state_is_not_returned() {
        let pool = DeflatePool::new(4);
        let config = config(NO_CONTEXT, &pool);
        let wire = config.compressor().compress(&[b'a'; 4096], true).unwrap();
        let mut decompressor = config.decompressor();
        assert!(matches!(
            decompressor.decompress(&wire, true, 16),
            Err(WebSocketError::FrameTooLarge)
        ));
        assert_eq!(idle_decompressors(&pool), 0);
    }

    #[test]
    fn zero_capacity_keeps_nothing() {
        let pool = DeflatePool::new(0);
        assert!(pool.deflate_queue(CompressionLevel::fast(), None).is_none());
        assert!(pool.inflate_queue(None).is_none());
    }
}
