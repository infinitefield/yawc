//! Deflate state shared between connections that negotiated no context takeover.
//!
//! Without context takeover nothing carries over from one message to the next, so a
//! connection only needs deflate state while a message is being compressed or
//! decompressed. Between messages the state sits in a [`DeflatePool`], and memory
//! scales with messages in flight instead of open connections.
use std::{array, fmt, sync::Arc};

use crossbeam_queue::ArrayQueue;

use super::{Deflate, Inflate, WINDOW_BITS};
use crate::CompressionLevel;

/// One queue per window size deflate accepts. Only the `zlib` backends take a window
/// size; the default backend always uses 15 bits and needs a single queue.
const WINDOW_SIZES: usize = if cfg!(feature = "zlib") {
    (*WINDOW_BITS.end() - *WINDOW_BITS.start() + 1) as usize
} else {
    1
};

/// Shares permessage-deflate state between connections that do not keep context.
///
/// When a direction of a connection negotiated no context takeover
/// (`server_no_context_takeover` or `client_no_context_takeover`), its compressor or
/// decompressor is taken from the pool for one message and returned afterwards. Cloning
/// the pool is cheap, and every clone shares the same state.
///
/// `capacity` bounds how many idle compressors, and separately how many idle
/// decompressors, are kept for each window size. A message that finds the pool empty
/// builds new state, and state returned to a full pool is dropped. A capacity of 0
/// keeps nothing. Connections sharing a pool should use the same compression level;
/// a compressor built with another level is dropped instead of reused.
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
    /// `None` when the capacity is 0, since an `ArrayQueue` cannot be empty-sized.
    shared: Option<Arc<Shared>>,
}

struct Shared {
    capacity: usize,
    deflate: [ArrayQueue<Deflate>; WINDOW_SIZES],
    inflate: [ArrayQueue<Inflate>; WINDOW_SIZES],
}

impl DeflatePool {
    /// Creates a pool that keeps up to `capacity` idle objects per window size.
    pub fn new(capacity: usize) -> Self {
        let shared = (capacity > 0).then(|| {
            Arc::new(Shared {
                capacity,
                deflate: array::from_fn(|_| ArrayQueue::new(capacity)),
                inflate: array::from_fn(|_| ArrayQueue::new(capacity)),
            })
        });
        Self { shared }
    }

    /// Takes an idle compressor built with `level` and `window_bits`, if there is one.
    pub(super) fn take_deflate(
        &self,
        level: CompressionLevel,
        window_bits: Option<u8>,
    ) -> Option<Deflate> {
        let shared = self.shared.as_ref()?;
        let deflate = shared.deflate[slot(window_bits)].pop()?;
        (deflate.level == level).then_some(deflate)
    }

    /// Returns a reset compressor. A full pool drops it.
    pub(super) fn put_deflate(&self, window_bits: Option<u8>, deflate: Deflate) {
        if let Some(shared) = &self.shared {
            // A full pool drops the state, which bounds idle memory.
            let _ = shared.deflate[slot(window_bits)].push(deflate);
        }
    }

    /// Takes an idle decompressor built with `window_bits`, if there is one.
    pub(super) fn take_inflate(&self, window_bits: Option<u8>) -> Option<Inflate> {
        self.shared.as_ref()?.inflate[slot(window_bits)].pop()
    }

    /// Returns a reset decompressor. A full pool drops it.
    pub(super) fn put_inflate(&self, window_bits: Option<u8>, inflate: Inflate) {
        if let Some(shared) = &self.shared {
            // A full pool drops the state, which bounds idle memory.
            let _ = shared.inflate[slot(window_bits)].push(inflate);
        }
    }
}

impl fmt::Debug for DeflatePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let capacity = self.shared.as_ref().map_or(0, |shared| shared.capacity);
        f.debug_struct("DeflatePool")
            .field("capacity", &capacity)
            .finish_non_exhaustive()
    }
}

/// The queue index for a window size. `None` means the default of 15 bits.
#[cfg(feature = "zlib")]
fn slot(window_bits: Option<u8>) -> usize {
    let bits = window_bits.map_or(*WINDOW_BITS.end(), |bits| {
        bits.clamp(*WINDOW_BITS.start(), *WINDOW_BITS.end())
    });
    usize::from(bits - WINDOW_BITS.start())
}

/// The queue index for a window size. Without `zlib` there is only one.
#[cfg(not(feature = "zlib"))]
fn slot(_window_bits: Option<u8>) -> usize {
    0
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
        pool.shared.as_ref().unwrap().deflate[slot(None)].len()
    }

    fn idle_decompressors(pool: &DeflatePool) -> usize {
        pool.shared.as_ref().unwrap().inflate[slot(None)].len()
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
        let mut compressor = config(NO_CONTEXT, &pool).compressor();
        compressor.compress(b"hello", true).unwrap();
        assert!(pool.take_deflate(CompressionLevel::fast(), None).is_none());
    }

    #[test]
    fn state_built_with_another_level_is_not_reused() {
        let pool = DeflatePool::new(4);
        pool.put_deflate(None, Deflate::new(CompressionLevel::best()));
        assert!(pool.take_deflate(CompressionLevel::fast(), None).is_none());
        pool.put_deflate(None, Deflate::new(CompressionLevel::fast()));
        assert!(pool.take_deflate(CompressionLevel::fast(), None).is_some());
    }

    #[test]
    fn every_window_size_has_a_queue() {
        for bits in WINDOW_BITS {
            assert!(slot(Some(bits)) < WINDOW_SIZES);
        }
        assert_eq!(slot(None), slot(Some(*WINDOW_BITS.end())));
    }
}
