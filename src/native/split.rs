//! Split read/write halves for WebSocket connections.

use std::{
    task::{ready, Context, Poll},
    time::{Duration, Instant},
};

use bytes::BytesMut;
use futures::SinkExt;

use crate::{
    close::CloseCode,
    compression::{Compressor, Decompressor},
    frame::{Frame, OpCode},
    Result, WebSocketError,
};

use super::{Negotiation, Role};

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
    /// Optional decompressor used to decompress incoming payloads.
    inflate: Option<Decompressor>,
    /// Structure for handling fragmented message frames.
    fragment: Option<Fragment>,
    /// Accumulated data from fragmented frames.
    accumulated: BytesMut,
    /// Maximum size of the read buffer
    ///
    /// When accumulating fragmented messages, once the read buffer exceeds this size
    /// it will be reset back to its initial capacity of 1 KiB. This helps prevent
    /// unbounded memory growth from receiving very large fragmented messages.
    max_read_buffer: usize,
    /// Maximum time allowed to receive all fragments of a fragmented message.
    fragment_timeout: Option<Duration>,
    /// Indicates if the connection has been closed.
    pub(super) is_closed: bool,
}

/// Fragmented message header.
struct Fragment {
    started: Instant,
    opcode: OpCode,
    is_compressed: bool,
}

impl ReadHalf {
    pub(super) fn new(role: Role, opts: &Negotiation) -> Self {
        let inflate = opts.decompressor(role);
        Self {
            inflate,
            max_read_buffer: opts.max_read_buffer,
            fragment_timeout: opts.fragment_timeout,
            fragment: None,
            accumulated: BytesMut::with_capacity(1024),
            is_closed: false,
        }
    }

    /// Processes an incoming WebSocket frame, handling message fragmentation and decompression if required.
    ///
    /// This function manages both data frames (text and binary) and control frames (ping, pong, and close).
    /// For fragmented messages, it accumulates the fragments until the message is complete, handling
    /// decompression if needed. If an uncompressed frame is received but compression is required, an error
    /// is returned.
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
        if frame.is_compressed && self.inflate.is_none() {
            return Err(WebSocketError::CompressionNotSupported);
        }

        match frame.opcode {
            OpCode::Text | OpCode::Binary => {
                if self.fragment.is_some() {
                    return Err(WebSocketError::InvalidFragment);
                }

                if !frame.fin {
                    self.fragment = Some(Fragment {
                        started: Instant::now(),
                        opcode: frame.opcode,
                        is_compressed: frame.is_compressed,
                    });

                    self.accumulated.extend_from_slice(&frame.payload);

                    Ok(None)
                } else if frame.is_compressed {
                    let inflate = self.inflate.as_mut().unwrap();
                    let payload = inflate.decompress(&frame.payload, frame.fin)?;
                    if let Some(payload) = payload {
                        frame.is_compressed = false;
                        frame.payload = payload.freeze();
                        Ok(Some(frame))
                    } else {
                        Err(WebSocketError::InvalidFragment)
                    }
                } else {
                    Ok(Some(frame))
                }
            }
            OpCode::Continuation => {
                if self.accumulated.len() + frame.payload.len() >= self.max_read_buffer {
                    return Err(WebSocketError::FrameTooLarge);
                }

                self.accumulated.extend_from_slice(&frame.payload);

                let fragment = self
                    .fragment
                    .as_ref()
                    .ok_or(WebSocketError::InvalidFragment)?;

                if frame.fin {
                    // gather all accumulated buffer and replace it with a new one to avoid
                    // too many allocations or a potential DoS.
                    let payload =
                        std::mem::replace(&mut self.accumulated, BytesMut::with_capacity(1024));
                    if fragment.is_compressed {
                        let inflate = self.inflate.as_mut().unwrap();
                        let output = inflate
                            .decompress(&payload, frame.fin)?
                            .expect("decompress output");

                        frame.opcode = fragment.opcode;
                        frame.is_compressed = false;
                        frame.payload = output.freeze();

                        self.fragment = None;

                        Ok(Some(frame))
                    } else {
                        frame.opcode = fragment.opcode;
                        frame.payload = payload.freeze();

                        self.fragment = None;

                        Ok(Some(frame))
                    }
                } else if self
                    .fragment_timeout
                    .is_some_and(|timeout| fragment.started.elapsed() > timeout)
                {
                    Err(WebSocketError::FragmentTimeout)
                } else {
                    Ok(None)
                }
            }
            _ => {
                // Control frames cannot be fragmented
                if !frame.fin {
                    return Err(WebSocketError::InvalidFragment);
                }

                self.is_closed = frame.opcode == OpCode::Close;

                Ok(Some(frame))
            }
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
    deflate: Option<Compressor>,
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
    pub(super) fn new(role: Role, opts: &Negotiation) -> Self {
        let deflate = opts.compressor(role);
        Self {
            deflate,
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
    pub fn start_send<S>(&mut self, stream: &mut S, view: Frame) -> Result<()>
    where
        S: futures::Sink<Frame, Error = WebSocketError> + Unpin,
    {
        if view.opcode == OpCode::Close {
            self.close_state = Some(CloseState::Flushing);
        }

        let maybe_frame = if !view.opcode.is_control() {
            if let Some(deflate) = self.deflate.as_mut() {
                // Compress the payload if compression is enabled
                let output = deflate.compress(&view.payload)?;
                Some(Frame::compress(true, view.opcode, None, output))
            } else {
                None
            }
        } else {
            None
        };

        let frame =
            maybe_frame.unwrap_or_else(|| Frame::new(true, view.opcode, None, view.payload));

        stream.start_send_unpin(frame)
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
}
