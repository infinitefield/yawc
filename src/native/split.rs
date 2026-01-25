//! Split read/write halves for WebSocket connections.
//!
//! # Architecture Layer: Fragment Assembly
//!
//! This module implements the **middle layer** of the WebSocket processing stack,
//! sitting between the codec layer and the WebSocket layer. Its primary responsibility
//! is **fragment assembly** according to RFC 6455.
//!
//! ## ReadHalf Responsibilities
//!
//! [`ReadHalf`] handles fragmented message assembly:
//!
//! - **Fragment accumulation**: Collects continuation frames into complete messages
//! - **State tracking**: Maintains fragment state (opcode, compression flag, accumulated data)
//! - **Fragment timeout**: Enforces timeouts on incomplete fragmented messages
//! - **Size limits**: Enforces maximum message size after assembly
//! - **Masking removal**: Removes frame masks to prevent accidental echo
//!
//! ## What ReadHalf Does NOT Handle
//!
//! - **Frame decoding**: Handled by [`Codec`](crate::codec::Codec)
//! - **Decompression**: Handled by [`WebSocket`](crate::WebSocket)
//! - **UTF-8 validation**: Handled by [`WebSocket`](crate::WebSocket)
//!
//! ## Data Flow Example
//!
//! **Receiving a fragmented message:**
//!
//! ```text
//! Codec → ReadHalf:
//!   Frame(OpCode::Text, FIN=0, "Hello")
//!     → ReadHalf: Start accumulating, return None
//!
//!   Frame(OpCode::Continuation, FIN=0, " Wor")
//!     → ReadHalf: Continue accumulating, return None
//!
//!   Frame(OpCode::Continuation, FIN=1, "ld!")
//!     → ReadHalf: Assemble complete message
//!     → Returns Frame(OpCode::Text, FIN=1, "Hello World!")
//! ```
//!
//! ## WriteHalf Responsibilities
//!
//! [`WriteHalf`] handles frame transmission with compression support:
//!
//! - **Compression**: Compresses frames when permessage-deflate is enabled
//! - **Masking**: Applies masks to client frames (required by RFC 6455)
//! - **Connection closure**: Manages graceful WebSocket closure protocol
//!
//! # Sans-IO Design
//!
//! This module implements a **sans-io** (I/O-free) design pattern where the protocol logic
//! is completely separated from the I/O operations.
//!
//! ## Benefits
//!
//! 1. **Testability**: Protocol logic can be tested without actual network I/O
//! 2. **Transport Agnostic**: Works with TCP, Unix sockets, in-memory buffers, or custom transports
//! 3. **Flexibility**: Users can implement custom flow control and buffering strategies
//! 4. **Performance**: Enables zero-copy optimizations in user code
//!
//! # Thread Safety Through `split()`
//!
//! WebSocket connections can be split into separate read and write halves for concurrent
//! operation. This is achieved through the `split()` method on `WebSocket`:
//!
//! ## Using `futures::StreamExt::split()` (Recommended)
//!
//! ```no_run
//! use futures::{StreamExt, SinkExt};
//! use yawc::{WebSocket, Frame};
//!
//! # async fn example() -> yawc::Result<()> {
//! let ws = WebSocket::connect("wss://example.com".parse()?).await?;
//!
//! // Split into read and write halves
//! let (mut write, mut read) = ws.split();
//!
//! // Spawn a task to read messages
//! tokio::spawn(async move {
//!     while let Some(frame) = read.next().await {
//!         println!("Received: {:?}", frame.opcode());
//!     }
//! });
//!
//! // Write from the main task
//! write.send(Frame::text("Hello!")).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Thread Safety Guarantees
//!
//! 1. **No Shared Mutable State**: Read and write halves operate independently with no
//!    shared mutable state between them.
//!
//! 2. **Borrowing Rules**: Rust's ownership system ensures only one task can access each
//!    half at a time, preventing data races at compile time.
//!
//! 3. **Send + Sync**: Both halves implement `Send`, allowing them to be moved across
//!    thread boundaries safely.
//!
//! 4. **Protocol Correctness**: Each half maintains its own protocol state (fragmentation,
//!    compression windows) ensuring WebSocket protocol correctness even with concurrent
//!    read/write operations.
//!
//! ## Low-Level `split_stream()` (Advanced)
//!
//! For advanced use cases requiring direct access to the underlying stream and protocol
//! handlers, use the unsafe `split_stream()` method:
//!
//! ```no_run
//! # use yawc::WebSocket;
//! # async fn example() -> yawc::Result<()> {
//! let ws = WebSocket::connect("wss://example.com".parse()?).await?;
//!
//! // SAFETY: User must ensure the stream is not used after splitting
//! let (mut stream, mut read_half, mut write_half) = unsafe { ws.split_stream() };
//!
//! // Manual protocol handling required
//! # Ok(())
//! # }
//! ```
//!
//! **Warning**: This is unsafe because it requires manual protocol handling. Users must:
//! - Correctly handle control frames (Ping, Pong, Close)
//! - Maintain proper message ordering
//! - Handle compression state correctly
//!
//! Prefer `futures::StreamExt::split()` unless you have specific low-level requirements.

use std::{
    collections::VecDeque,
    future::poll_fn,
    task::{ready, Context, Poll},
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures::SinkExt;

use crate::{
    close::CloseCode,
    frame::{Frame, OpCode},
    Result, WebSocketError,
};

use super::Negotiation;

struct Fragmentation {
    started: Instant,
    opcode: OpCode,
    is_compressed: bool,
    bytes_read: usize,
    parts: VecDeque<Bytes>,
}

// ================ ReadHalf ====================

/// The read half of a WebSocket connection, responsible for receiving and processing incoming messages.
///
/// [`ReadHalf`] follows a sans-io design, meaning it does not handle I/O operations directly.
/// Instead, the user must provide a [`futures::Stream`](https://docs.rs/futures/latest/futures/stream/trait.Stream.html)
/// of frames on each function call. This allows the ReadHalf to be I/O agnostic and work with any underlying transport.
///
/// [`ReadHalf`] handles decompression and message fragmentation but does not manage WebSocket control frames.
/// Users are responsible for handling frames with [`OpCode::Ping`], [`OpCode::Pong`], and [`OpCode::Close`] codes.
///
/// After a [`OpCode::Close`] frame is received, the [`ReadHalf`] will no longer accept reads and will
/// return a [`WebSocketError::ConnectionClosed`] error for all subsequent read attempts.
///
/// # Warning
///
/// In most cases, you should **not** use [`ReadHalf`] directly. Instead, use
/// [`futures::StreamExt::split`](https://docs.rs/futures/latest/futures/stream/trait.StreamExt.html#method.split)
/// on the [`WebSocket`](super::WebSocket) to obtain a stream split that maintains all WebSocket protocol handling.
/// Direct use of [`ReadHalf`] bypasses important protocol management like automatic control frame handling.
///
///
/// # Example
/// ```no_run
/// use tokio::net::TcpStream;
/// use yawc::{WebSocket, frame::OpCode};
/// use futures::StreamExt;
/// use std::task::Context;
/// use std::pin::Pin;
/// use tokio_rustls::TlsConnector;
/// use url::Url;
///
/// #[tokio::main]
/// async fn main() -> yawc::Result<()> {
///     let url = "wss://api.example.com/ws".parse()?;
///     let ws = WebSocket::connect(url).await?;
///
///     let (mut stream, mut read_half, write_half) = unsafe { ws.split_stream() };
///
///     std::future::poll_fn(|cx| {
///         read_half.poll_frame(&mut stream, cx)
///     }).await?;
///
///     Ok(())
/// }
/// ```
pub struct ReadHalf {
    /// Indicates if the connection has been closed.
    pub(super) is_closed: bool,
    /// Fragment accumulation state
    fragment: Option<Fragmentation>,
    /// Maximum read buffer size for fragmented messages
    max_read_buffer: usize,
    /// Optional timeout for fragment assembly
    fragment_timeout: Option<Duration>,
}

impl ReadHalf {
    pub(super) fn new(opts: &Negotiation) -> Self {
        Self {
            is_closed: false,
            fragment: None,
            max_read_buffer: opts.max_read_buffer,
            fragment_timeout: opts.fragment_timeout,
        }
    }

    /// Processes an incoming WebSocket frame, handling message fragmentation and decompression if required.
    ///
    /// This function manages both data frames (text and binary) and control frames (ping, pong, and close).
    /// For fragmented messages, it accumulates the fragments until the message is complete, handling
    /// decompression if needed. If an uncompressed frame is received but compression is required, an error
    /// is returned.
    ///
    /// This function also removes the mask from every message to avoid forwarding in case the user is echoing the frame.
    ///
    /// - For `OpCode::Text` and `OpCode::Binary` frames:
    ///     - If the frame is final (`fin` flag set), it processes the frame as a complete message.
    ///     - If the frame is part of a fragmented message (`fin` flag not set), it starts accumulating
    ///       fragments or returns an error if a previous incomplete fragment exists.
    /// - For `OpCode::Continuation` frames:
    ///     - Continues accumulating data for the ongoing fragmented message.
    ///     - If the frame is final, the accumulated fragments are processed and assembled into a complete message.
    /// - For control frames:
    ///     - `Ping`, `Pong`, and `Close` frames are processed as-is if they are not fragmented.
    ///     - Control frames are returned to the user for optional custom handling.
    ///
    /// # Parameters
    /// - `frame`: The incoming frame to be processed.
    ///
    /// # Returns
    /// - `Ok(Some(Frame))` if the frame is ready for further processing.
    /// - `Ok(None)` if the frame is part of a fragmented message and not yet complete.
    /// - `Err(WebSocketError)` if an error occurs, such as invalid fragment or unsupported compression.
    fn on_frame(&mut self, mut frame: Frame) -> Result<Option<Frame>> {
        use bytes::BufMut;

        // Remove mask immediately
        frame.mask = None;

        // Handle fragmentation and assembly
        match frame.opcode {
            OpCode::Text | OpCode::Binary => {
                // Check for invalid fragmentation state
                if self.fragment.is_some() {
                    return Err(WebSocketError::InvalidFragment);
                }

                // Handle fragmented messages
                if !frame.fin {
                    self.fragment = Some(Fragmentation {
                        started: Instant::now(),
                        opcode: frame.opcode,
                        is_compressed: frame.is_compressed,
                        bytes_read: frame.payload.len(),
                        parts: VecDeque::from([frame.payload]),
                    });
                    return Ok(None);
                }

                // Non-fragmented message - return as-is
                Ok(Some(frame))
            }
            OpCode::Continuation => {
                let mut fragment = self
                    .fragment
                    .take()
                    .ok_or(WebSocketError::InvalidFragment)?;

                fragment.bytes_read += frame.payload.len();

                // Check buffer size
                if fragment.bytes_read >= self.max_read_buffer {
                    return Err(WebSocketError::FrameTooLarge);
                }

                fragment.parts.push_back(frame.payload);

                if frame.fin {
                    // Assemble complete message
                    frame.opcode = fragment.opcode;
                    frame.is_compressed = fragment.is_compressed;
                    frame.payload = fragment
                        .parts
                        .into_iter()
                        .fold(
                            bytes::BytesMut::with_capacity(fragment.bytes_read),
                            |mut acc, b| {
                                acc.put(b);
                                acc
                            },
                        )
                        .freeze();

                    // Return assembled frame as-is (decompression and validation happen in WebSocket layer)
                    Ok(Some(frame))
                } else if self
                    .fragment_timeout
                    .is_some_and(|timeout| fragment.started.elapsed() > timeout)
                {
                    Err(WebSocketError::FragmentTimeout)
                } else {
                    self.fragment = Some(fragment);
                    Ok(None)
                }
            }
            OpCode::Close => {
                self.is_closed = true;
                Ok(Some(frame))
            }
            _ => Ok(Some(frame)),
        }
    }

    /// Polls the WebSocket stream for the next frame, managing frame processing and protocol compliance.
    ///
    /// This method continuously polls the stream until one of three conditions is met:
    /// 1. A complete message frame is ready
    /// 2. An error occurs during frame processing
    /// 3. The WebSocket connection is closed
    ///
    /// # Message Types
    ///
    /// ## Data Frames
    /// Text and binary frames are processed according to the WebSocket protocol:
    /// - Complete frames are returned immediately
    /// - Fragmented frames are accumulated until the final fragment arrives
    /// - All fragments are validated for proper sequencing
    ///
    /// ## Control Frames
    /// Ping, Pong and Close frames receive special handling:
    /// - Always processed immediately when received
    /// - Never fragmented according to protocol
    /// - Close frames set internal closed state
    ///
    /// # Parameters
    /// - `stream`: The WebSocket stream to poll frames from
    /// - `cx`: Task context for asynchronous polling
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(Frame))`: A complete frame was successfully received
    /// - `Poll::Ready(Err(WebSocketError))`: An error occurred during frame processing
    /// - `Poll::Pending`: More data needed to complete current frame
    ///
    /// On connection closure (receiving Close frame or stream end),
    /// returns `Poll::Ready(Err(WebSocketError::ConnectionClosed))`.
    pub fn poll_frame<S>(&mut self, stream: &mut S, cx: &mut Context<'_>) -> Poll<Result<Frame>>
    where
        S: futures::Stream<Item = Result<Frame>> + Unpin,
    {
        use futures::StreamExt;

        while !self.is_closed {
            let frame = ready!(stream.poll_next_unpin(cx));
            let res = match frame {
                Some(res) => res,
                None => {
                    self.is_closed = true;
                    break;
                }
            };

            match res {
                Ok(frame) => match self.on_frame(frame) {
                    Ok(Some(frame)) => {
                        return Poll::Ready(Ok(frame));
                    }
                    Err(err) => return Poll::Ready(Err(err)),
                    Ok(None) => {}
                },
                Err(err) => return Poll::Ready(Err(err)),
            }
        }

        Poll::Ready(Err(WebSocketError::ConnectionClosed))
    }

    /// Convenience method to read the next frame from the stream.
    ///
    /// This is a higher-level alternative to [`poll_frame`](Self::poll_frame) that handles
    /// the polling loop for you.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use yawc::WebSocket;
    /// # async fn example() -> yawc::Result<()> {
    /// let ws = WebSocket::connect("wss://example.com".parse()?).await?;
    /// let (mut stream, mut read_half, _write_half) = unsafe { ws.split_stream() };
    ///
    /// // Read frames one by one
    /// while let Ok(frame) = read_half.next_frame(&mut stream).await {
    ///     println!("Received: {:?}", frame.opcode());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn next_frame<S>(&mut self, stream: &mut S) -> Result<Frame>
    where
        S: futures::Stream<Item = Result<Frame>> + Unpin,
    {
        poll_fn(|cx| self.poll_frame(stream, cx)).await
    }
}

// ================ WriteHalf ====================

/// Write half of the WebSocket connection.
///
/// [`WriteHalf`] manages sending WebSocket frames and closing the connection gracefully.
/// It handles:
///
/// - Compression of outgoing frames when enabled
/// - Masking of frames when acting as a client
/// - Protocol-compliant connection closure
/// - Frame buffering and flushing
/// - Automatic fragmentation when `max_payload_write_size` is configured
///
/// # Automatic Fragmentation
///
/// When `max_payload_write_size` is set via [`Options::with_max_payload_write_size`](crate::Options::with_max_payload_write_size),
/// outgoing messages that exceed this size will be automatically fragmented into multiple frames.
/// Each fragment will have a payload size at or below the configured limit.
///
/// **Important**: Automatic fragmentation is mutually exclusive with manual fragmentation.
/// If you're manually sending fragmented frames (using `FIN=0`), automatic fragmentation is disabled.
/// Additionally, compressed messages cannot be automatically fragmented - the entire compressed
/// payload is sent as one unit (though it may be split at the compression stage).
///
/// # Warning
///
/// In most cases, you should **not** use [`WriteHalf`] directly. Instead, use
/// [`futures::StreamExt::split`](https://docs.rs/futures/latest/futures/stream/trait.StreamExt.html#method.split)
/// on the [`WebSocket`](super::WebSocket) to obtain stream halves that maintain all WebSocket protocol handling.
/// Direct use of [`WriteHalf`] bypasses important protocol management like automatic control frame handling.
///
/// # Connection Closure
/// When closing the connection, [`WriteHalf`] follows the WebSocket protocol by:
///
/// 1. Sending a [`OpCode::Close`] frame to the peer
/// 2. Flushing any pending frames
/// 3. Closing the underlying network stream
///
/// This ensures a clean shutdown where all data is delivered before disconnection.
///
/// # Example
/// ```no_run
/// use tokio::net::TcpStream;
/// use yawc::{WebSocket, frame::OpCode, Options, Result};
/// use futures::StreamExt;
/// use tokio_rustls::TlsConnector;
///
/// #[tokio::main]
/// async fn main() -> Result<()> {
///     // Connect WebSocket
///     let ws = WebSocket::connect("wss://example.com/ws".parse()?).await?;
///
///     // Split into read/write halves
///     let (stream, read_half, write_half) = unsafe { ws.split_stream() };
///     // do something ...
///     Ok(())
/// }
/// ```
pub struct WriteHalf {
    // it would be ideal to not use a VecDeque...
    fragmented_writes: VecDeque<Frame>,
    pub(super) max_payload_write_size: Option<usize>,
    close_state: Option<CloseState>,
}

/// Represents the various states involved in gracefully closing a WebSocket connection.
///
/// The `CloseState` enum defines the sequential steps taken to close the WebSocket connection:
///
/// - `Sending(Frame)`: The WebSocket is in the process of sending an `OpCode::Close` frame to the peer.
/// - `Flushing`: The `Close` frame has been sent, and the connection is now flushing any remaining data.
/// - `Closing`: The WebSocket is closing the underlying stream, ensuring all resources are properly released.
/// - `Done`: The connection is fully closed, and no further actions are required.
enum CloseState {
    /// The WebSocket is sending the `Close` frame to signal the end of the connection.
    Sending(Frame),
    /// The WebSocket is flushing the remaining data after the `Close` frame has been sent.
    Flushing,
    /// The WebSocket is in the process of closing the underlying network stream.
    Closing,
    /// The connection is fully closed, and all necessary shutdown steps have been completed.
    Done,
}

impl WriteHalf {
    pub(super) fn new(max_payload_write_size: Option<usize>, _opts: &Negotiation) -> Self {
        Self {
            max_payload_write_size,
            fragmented_writes: VecDeque::with_capacity(2),
            close_state: None,
        }
    }

    /// Polls the readiness of the `WriteHalf` to send a new frame.
    ///
    /// If the WebSocket connection is already closed, this will return an error. Otherwise, it
    /// checks if the underlying stream is ready to accept a new frame.
    ///
    /// # Parameters
    /// - `stream`: The WebSocket stream to check readiness on
    /// - `cx`: The polling context
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(()))` if the `WriteHalf` is ready to send.
    /// - `Poll::Ready(Err(WebSocketError::ConnectionClosed))` if the connection is closed.
    pub fn poll_ready<S>(&mut self, stream: &mut S, cx: &mut Context<'_>) -> Poll<Result<()>>
    where
        S: futures::Sink<Frame, Error = WebSocketError> + Unpin,
    {
        if matches!(self.close_state, Some(CloseState::Done)) {
            return Poll::Ready(Err(WebSocketError::ConnectionClosed));
        }

        // ready if the underlying is
        stream.poll_ready_unpin(cx)
    }

    /// Begins sending a frame through the `WriteHalf`.
    ///
    /// This method takes a Frame representing the frame to be sent and prepares it for
    /// transmission according to the WebSocket protocol rules:
    ///
    /// - For non-control frames with compression enabled, the payload is compressed
    /// - For client connections, the frame is automatically masked per protocol requirements
    /// - Close frames trigger a state change to handle connection shutdown
    ///
    /// The method handles frame preparation but does not wait for the actual transmission to complete.
    /// The prepared frame is queued for sending in the underlying stream.
    ///
    /// # Parameters
    /// - `stream`: The WebSocket sink that will transmit the frame
    /// - `view`: A Frame containing the frame payload and metadata
    ///
    /// # Returns
    /// - `Ok(())` if the frame was successfully prepared and queued
    /// - `Err(WebSocketError)` if frame preparation or queueing failed
    ///
    /// # Protocol Details
    /// - Non-control frames are compressed if compression is enabled
    /// - Client frames are masked per WebSocket protocol requirement
    /// - Close frames transition the connection to closing state
    pub fn start_send<S>(&mut self, _stream: &mut S, frame: Frame) -> Result<()>
    where
        S: futures::Sink<Frame, Error = WebSocketError> + Unpin,
    {
        if frame.opcode == OpCode::Close {
            self.close_state = Some(CloseState::Flushing);
        }

        let fragments = frame.into_fragments(self.max_payload_write_size.unwrap_or(usize::MAX));
        self.fragmented_writes.extend(fragments);

        Ok(())
    }

    /// Polls to flush all pending frames in the `WriteHalf`.
    ///
    /// Ensures that any frames waiting to be sent are fully flushed to the network.
    ///
    /// # Parameters
    /// - `stream`: The WebSocket stream to flush frames on
    /// - `cx`: The polling context
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(()))` if all pending frames have been flushed.
    /// - `Poll::Pending` if there are still frames waiting to be flushed.
    pub fn poll_flush<S>(&mut self, stream: &mut S, cx: &mut Context<'_>) -> Poll<Result<()>>
    where
        S: futures::Sink<Frame, Error = WebSocketError> + Unpin,
    {
        while !self.fragmented_writes.is_empty() {
            // poll the underlying to check the byte boundary
            ready!(stream.poll_ready_unpin(cx))?;
            let frame = self.fragmented_writes.pop_front().expect("frame");
            stream.start_send_unpin(frame)?;
        }

        stream.poll_flush_unpin(cx)
    }

    /// Polls the connection to close the [`WriteHalf`] by initiating a graceful shutdown.
    ///
    /// The `poll_close` function guides the closure process through several stages:
    /// 1. **Sending**: Sends a `Close` frame to the peer.
    /// 2. **Flushing**: Ensures the `Close` frame and any pending frames are fully flushed.
    /// 3. **Closing**: Closes the underlying network stream once the frames are flushed.
    /// 4. **Done**: Marks the connection as closed.
    ///
    /// This sequence ensures a clean shutdown, allowing all necessary data to be sent before the connection closes.
    ///
    /// # Parameters
    /// - `stream`: The WebSocket stream to close
    /// - `cx`: The polling context
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(()))` once the connection is fully closed.
    /// - `Poll::Pending` if the closure is still in progress.
    pub fn poll_close<S>(&mut self, stream: &mut S, cx: &mut Context<'_>) -> Poll<Result<()>>
    where
        S: futures::Sink<Frame, Error = WebSocketError> + Unpin,
    {
        loop {
            match self.close_state.take() {
                None => {
                    let frame = Frame::close(CloseCode::Normal, []);
                    self.close_state = Some(CloseState::Sending(frame));
                }
                Some(CloseState::Sending(frame)) => {
                    if stream.poll_ready_unpin(cx).is_pending() {
                        self.close_state = Some(CloseState::Sending(frame));
                        break Poll::Pending;
                    }

                    stream.start_send_unpin(frame)?;

                    self.close_state = Some(CloseState::Flushing);
                }
                Some(CloseState::Flushing) => {
                    if stream.poll_flush_unpin(cx).is_pending() {
                        self.close_state = Some(CloseState::Flushing);
                        break Poll::Pending;
                    }

                    self.close_state = Some(CloseState::Closing);
                }
                // TODO: we should probably wait for the read half close frame response
                Some(CloseState::Closing) => {
                    if stream.poll_close_unpin(cx).is_pending() {
                        self.close_state = Some(CloseState::Closing);
                        break Poll::Pending;
                    }

                    self.close_state = Some(CloseState::Done);
                }
                Some(CloseState::Done) => break Poll::Ready(Ok(())),
            }
        }
    }

    /// Convenience method to send a frame to the stream.
    ///
    /// This is a higher-level alternative to the poll-based methods that handles
    /// the polling loop for you. It ensures the sink is ready, sends the frame,
    /// and flushes it.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use yawc::{WebSocket, Frame};
    /// # async fn example() -> yawc::Result<()> {
    /// let ws = WebSocket::connect("wss://example.com".parse()?).await?;
    /// let (mut stream, _read_half, mut write_half) = unsafe { ws.split_stream() };
    ///
    /// // Send a frame
    /// write_half.send_frame(&mut stream, Frame::text("Hello")).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn send_frame<S>(&mut self, stream: &mut S, frame: Frame) -> Result<()>
    where
        S: futures::Sink<Frame, Error = crate::WebSocketError> + Unpin,
    {
        // Wait until ready
        poll_fn(|cx| self.poll_ready(stream, cx)).await?;

        // Send the frame
        self.start_send(stream, frame)?;

        // Flush to ensure it's sent
        poll_fn(|cx| self.poll_flush(stream, cx)).await?;

        Ok(())
    }

    /// Convenience method to close the connection.
    ///
    /// This sends a close frame and waits for the connection to close gracefully.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use yawc::WebSocket;
    /// # async fn example() -> yawc::Result<()> {
    /// let ws = WebSocket::connect("wss://example.com".parse()?).await?;
    /// let (mut stream, _read_half, mut write_half) = unsafe { ws.split_stream() };
    ///
    /// // Close the connection
    /// write_half.close(&mut stream).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn close<S>(&mut self, stream: &mut S) -> Result<()>
    where
        S: futures::Sink<Frame, Error = crate::WebSocketError> + Unpin,
    {
        poll_fn(|cx| self.poll_close(stream, cx)).await
    }
}
