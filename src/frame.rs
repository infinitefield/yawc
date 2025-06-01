//! # Frame
//!
//! The `frame` module implements WebSocket frames as defined in [RFC 6455 Section 5.2](https://datatracker.ietf.org/doc/html/rfc6455#section-5.2),
//! providing the core building blocks for WebSocket communication. Each frame represents an atomic unit of data transmission,
//! containing both the payload and protocol-level metadata.
//!
//! ## Overview of WebSocket Frames
//!
//! WebSocket messages are transmitted as a sequence of frames. The module provides two main frame implementations:
//!
//! - [`Frame`]: Full mutable frame with all protocol metadata and masking capabilities
//! - [`FrameView`]: Lightweight immutable view optimized for efficient frame handling
//!
//! ### Frame Binary Format
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-------+-+-------------+-------------------------------+
//! |F|R|R|R| opcode|M| Payload len |    Extended payload length    |
//! |I|S|S|S|  (4)  |A|     (7)     |         (16 or 64 bits)       |
//! |N|V|V|V|       |S|             |                               |
//! | |1|2|3|       |K|             |                               |
//! +-+-+-+-+-------+-+-------------+-------------------------------+
//! |        Extended payload length continued, if payload len == 127|
//! +---------------------------------------------------------------+
//! |                               |   Masking-key, if MASK set to 1|
//! +-------------------------------+-------------------------------+
//! |     Masking-key (continued)       |          Payload Data      |
//! +-----------------------------------+ - - - - - - - - - - - - - -+
//! :                     Payload Data continued ...                :
//! +---------------------------------------------------------------+
//! ```
//!
//! Frames come in two categories:
//!
//! - **Data Frames**: Carry application payload with:
//!   - `OpCode::Text`: UTF-8 text data
//!   - `OpCode::Binary`: Raw binary data
//!   - `OpCode::Continuation`: Continuation of a fragmented message
//! - **Control Frames**: Manage the connection with:
//!   - `OpCode::Close`: Initiates connection closure with optional status code and reason
//!   - `OpCode::Ping`: Checks connection liveness, requires a Pong response
//!   - `OpCode::Pong`: Responds to Ping frames
//!
//! ## Frame Structure
//!
//! The [`Frame`] struct implements the full WebSocket frame format with:
//!
//! - `fin`: Final fragment flag (1 bit)
//! - `opcode`: Frame type identifier (4 bits)
//! - `mask`: Optional 32-bit XOR masking key
//! - `payload`: Frame data as `BytesMut`
//! - `is_compressed`: Per-message compression flag (1 bit, RSV1)
//!
//! While [`FrameView`] provides an optimized immutable view with:
//!
//! - `opcode`: Frame type identifier
//! - `payload`: Immutable frame data as `Bytes`
//!
//! ### Frame Construction
//!
//! The module provides ergonomic constructors via [`FrameView`] for common frame types:
//!
//! ```rust
//! use yawc::frame::FrameView;
//! use bytes::Bytes;
//! use yawc::close::CloseCode;
//!
//! // Text frame with UTF-8 payload
//! let text_frame = FrameView::text("Hello, WebSocket!");
//!
//! // Control frames
//! let ping = FrameView::ping("Ping payload"); // Ping with optional payload
//! let pong = FrameView::pong("Pong response"); // Pong response to ping
//! let close = FrameView::close(CloseCode::Normal, b"Normal closure"); // Status code + reason
//! ```
//!
//! ## Frame Processing
//!
//! Frames support automatic masking (required for client-to-server messages) and optional
//! per-message compression via the WebSocket permessage-deflate extension. The module handles:
//!
//! - Frame header parsing and serialization
//! - Payload masking and unmasking
//! - Message fragmentation and reassembly
//! - UTF-8 validation for text frames
//! - Compression and decompression of payloads (if permessage-deflate is enabled)
//!
//! For more details on the WebSocket protocol and frame handling, see [RFC 6455 Section 5](https://datatracker.ietf.org/doc/html/rfc6455#section-5).
use bytes::{Bytes, BytesMut};

use crate::{close::CloseCode, WebSocketError};

/// WebSocket operation code (OpCode) that determines the semantic meaning and handling of a frame.
///
/// Each variant represents a distinct frame type in the WebSocket protocol:
///
/// # Data Frame OpCodes
/// - `Continuation`: Continues a fragmented message started by another data frame
/// - `Text`: Contains UTF-8 encoded text data
/// - `Binary`: Contains raw binary data
///
/// # Control Frame OpCodes
/// - `Close`: Initiates or confirms connection closure
/// - `Ping`: Tests connection liveness, requiring a `Pong` response
/// - `Pong`: Responds to a `Ping` frame
///
/// # Reserved OpCodes
/// The ranges 0x3-0x7 and 0xB-0xF are reserved for future protocol extensions.
/// Frames with these opcodes will be rejected as invalid per RFC 6455.
///
/// The numeric values for each OpCode are defined in [RFC 6455, Section 11.8](https://datatracker.ietf.org/doc/html/rfc6455#section-11.8):
/// - Continuation = 0x0
/// - Text = 0x1
/// - Binary = 0x2
/// - Close = 0x8
/// - Ping = 0x9
/// - Pong = 0xA
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum OpCode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl OpCode {
    /// Returns `true` if the `OpCode` represents a control frame (`Close`, `Ping`, or `Pong`).
    ///
    /// Control frames are used to manage the connection state and have special constraints:
    /// - Cannot be fragmented (the FIN bit must be set)
    /// - Payload must not exceed 125 bytes
    /// - Must be processed immediately rather than queued with data frames
    pub fn is_control(&self) -> bool {
        matches!(*self, OpCode::Close | OpCode::Ping | OpCode::Pong)
    }
}

impl TryFrom<u8> for OpCode {
    type Error = WebSocketError;

    /// Attempts to convert a byte value into an `OpCode`, returning an error if the byte does not match any valid `OpCode`.
    ///
    /// This conversion is typically used during frame parsing to interpret the opcode field from the frame header.
    /// Invalid opcodes (0x3-0x7 and 0xB-0xF) will result in a `WebSocketError::InvalidOpCode` error.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x0 => Ok(Self::Continuation),
            0x1 => Ok(Self::Text),
            0x2 => Ok(Self::Binary),
            0x8 => Ok(Self::Close),
            0x9 => Ok(Self::Ping),
            0xA => Ok(Self::Pong),
            _ => Err(WebSocketError::InvalidOpCode(value)),
        }
    }
}

impl From<OpCode> for u8 {
    /// Converts an `OpCode` into its corresponding byte representation.
    fn from(val: OpCode) -> Self {
        match val {
            OpCode::Continuation => 0x0,
            OpCode::Text => 0x1,
            OpCode::Binary => 0x2,
            OpCode::Close => 0x8,
            OpCode::Ping => 0x9,
            OpCode::Pong => 0xA,
        }
    }
}

/// A lightweight view of a WebSocket frame, containing just the opcode and payload.
/// This struct provides a more efficient, immutable representation of frame data
/// compared to the full `Frame` struct.
#[derive(Clone)]
pub struct FrameView {
    /// The operation code indicating the type of frame (Text, Binary, Close, etc.)
    pub opcode: OpCode,
    /// The frame's payload data as immutable bytes, already unmasked if it was originally masked
    pub payload: Bytes,
}

impl FrameView {
    /// Extracts the close code from a Close frame's payload.
    ///
    /// For a valid Close frame, the first two bytes of the payload contain
    /// a status code indicating why the connection was closed.
    ///
    /// # Returns
    /// - `Some(CloseCode)` if the payload contains a valid close code
    /// - `None` if the payload is empty or too short to contain a close code
    pub fn close_code(&self) -> Option<CloseCode> {
        let code = CloseCode::from(u16::from_be_bytes(self.payload[0..2].try_into().ok()?));
        Some(code)
    }

    /// Extracts the close reason from a Close frame's payload.
    ///
    /// For a valid Close frame, bytes after the first two bytes may contain
    /// a UTF-8 encoded reason string explaining why the connection was closed.
    ///
    /// # Returns
    /// - `Some(&str)` containing the reason string if present and valid UTF-8
    /// - `None` if there is no reason string or it's not valid UTF-8
    pub fn close_reason(&self) -> Option<&str> {
        std::str::from_utf8(&self.payload[2..]).ok()
    }

    /// Converts the frame payload to a string slice, expecting valid UTF-8.
    ///
    /// # Returns
    /// A string slice (`&str`) of the frame's payload.
    ///
    /// # Panics
    /// Panics if the payload is not valid UTF-8. Use this method only when
    /// you are certain the payload contains valid UTF-8 text, such as with
    /// frames that have `OpCode::Text`.
    #[inline]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.payload).expect("utf8")
    }

    /// Creates a new immutable text frame view with the given payload.
    /// The payload is converted to immutable `Bytes`.
    pub fn text(payload: impl Into<Bytes>) -> Self {
        Self {
            opcode: OpCode::Text,
            payload: payload.into(),
        }
    }

    /// Creates a new immutable binary frame view with the given payload.
    /// The payload is converted to immutable `Bytes`.
    pub fn binary(payload: impl Into<Bytes>) -> Self {
        Self {
            opcode: OpCode::Binary,
            payload: payload.into(),
        }
    }

    /// Creates a new immutable close frame view with a close code and reason.
    /// Constructs the close frame payload by combining the code and reason bytes.
    pub fn close(code: CloseCode, reason: impl AsRef<[u8]>) -> Self {
        let code16 = u16::from(code);
        let reason: &[u8] = reason.as_ref();
        let mut payload = Vec::with_capacity(2 + reason.len());
        payload.extend_from_slice(&code16.to_be_bytes());
        payload.extend_from_slice(reason);

        Self {
            opcode: OpCode::Close,
            payload: payload.into(),
        }
    }

    /// Creates a new immutable ping frame view with the given payload.
    /// Used to create ping messages to check connection liveness.
    pub fn ping(payload: impl Into<Bytes>) -> Self {
        Self {
            opcode: OpCode::Ping,
            payload: payload.into(),
        }
    }

    /// Creates a new immutable pong frame view with the given payload.
    /// Used to respond to ping messages.
    pub fn pong(payload: impl Into<Bytes>) -> Self {
        Self {
            opcode: OpCode::Pong,
            payload: payload.into(),
        }
    }

    /// Creates a new immutable close frame view with a raw payload.
    /// Allows creating close frames without enforcing code/reason structure.
    pub fn close_raw(payload: impl Into<Bytes>) -> Self {
        Self {
            opcode: OpCode::Close,
            payload: payload.into(),
        }
    }
}

/// Converts a `FrameView` into a tuple of `(OpCode, Bytes)`.
///
/// This allows destructuring a frame view into its component parts while
/// maintaining ownership of both the opcode and payload.
impl From<FrameView> for (OpCode, Bytes) {
    fn from(val: FrameView) -> Self {
        (val.opcode, val.payload)
    }
}

/// Converts a tuple of `(OpCode, Bytes)` to a `FrameView`.
///
/// This allows constructing a `FrameView` from an opcode and immutable bytes payload.
impl From<(OpCode, Bytes)> for FrameView {
    fn from((opcode, payload): (OpCode, Bytes)) -> Self {
        Self { opcode, payload }
    }
}

/// Converts a tuple of `(OpCode, BytesMut)` to a `FrameView`.
///
/// This allows constructing a `FrameView` from an opcode and mutable bytes payload,
/// automatically freezing the bytes into an immutable form.
impl From<(OpCode, BytesMut)> for FrameView {
    fn from((opcode, payload): (OpCode, BytesMut)) -> Self {
        Self {
            opcode,
            payload: payload.freeze(),
        }
    }
}

/// Converts a full `Frame` into a `FrameView` by extracting just the opcode and
/// freezing the payload into immutable bytes.
impl From<Frame> for FrameView {
    fn from(value: Frame) -> Self {
        Self::from((value.opcode, value.payload))
    }
}

/// Represents a WebSocket frame, encapsulating the data and metadata for message transmission.
///
/// **Note: This low-level struct should rarely be used directly.** Most users should interact with the
/// higher-level WebSocket message APIs instead. Direct frame manipulation should only be needed in
/// specialized cases where fine-grained control over the WebSocket protocol is required.
///
/// A WebSocket frame is the fundamental unit of communication in the WebSocket protocol, carrying both
/// the payload data and essential metadata. Frames can be categorized into two types:
///
/// 1. **Data Frames**
///    - Text frames containing UTF-8 encoded text
///    - Binary frames containing raw data
///    - Continuation frames for message fragmentation
///
/// 2. **Control Frames**
///    - Close frames for connection termination
///    - Ping frames for connection liveness checks
///    - Pong frames for responding to pings
///
/// # Creating Frames
///
/// While frames can be constructed directly, it's recommended to use the provided factory methods:
/// ```rust
/// use yawc::frame::FrameView;
/// use yawc::close::CloseCode;
///
/// let text_frame = FrameView::text("Hello");
/// let binary_frame = FrameView::binary(vec![1, 2, 3]);
/// let ping_frame = FrameView::ping(vec![]);
/// let close_frame = FrameView::close(CloseCode::Normal, b"Goodbye");
/// ```
///
/// # Fields
/// - `fin`: Final fragment flag. When `true`, indicates this frame completes a message.
/// - `opcode`: Defines the frame type and interpretation (text, binary, control, etc).
/// - `mask`: Optional 32-bit XOR masking key required for client-to-server messages.
/// - `payload`: Frame payload data stored as dynamically sized bytes.
pub struct Frame {
    /// Indicates if this is the final frame in a message.
    pub fin: bool,
    /// The opcode of the frame, defining its type.
    pub opcode: OpCode,
    /// Flag indicating whether the payload is compressed.
    pub(super) is_compressed: bool,
    /// The masking key for the frame, if any, used for security in client-to-server frames.
    mask: Option<[u8; 4]>,
    /// The payload of the frame, containing the actual data.
    pub payload: BytesMut,
}

/// Converts a `FrameView` into a `Frame`.
///
/// The resulting `Frame` will have:
/// - `fin` set to `true` (final frame)
/// - The same opcode as the `FrameView`
/// - No masking key
/// - The same payload as the `FrameView`
impl From<FrameView> for Frame {
    fn from(value: FrameView) -> Self {
        Frame::new(true, value.opcode, None, value.payload)
    }
}

pub(crate) const MAX_HEAD_SIZE: usize = 16;

impl Frame {
    /// Creates a new WebSocket `Frame`.
    ///
    /// # Parameters
    /// - `fin`: Indicates if this frame is the final fragment in a message.
    /// - `opcode`: The operation code of the frame, defining its type (e.g., Text, Binary, Close).
    /// - `mask`: Optional 4-byte masking key, typically used in client-to-server frames.
    /// - `payload`: The frame payload data.
    ///
    /// # Returns
    /// A new instance of `Frame` with the specified parameters.
    pub fn new(
        fin: bool,
        opcode: OpCode,
        mask: Option<[u8; 4]>,
        payload: impl Into<BytesMut>,
    ) -> Self {
        Self {
            fin,
            opcode,
            mask,
            payload: payload.into(),
            is_compressed: false,
        }
    }

    /// Creates a new frame with compression enabled.
    ///
    /// Similar to `new`, but sets the compression flag to indicate the payload
    /// has been compressed using the permessage-deflate extension.
    ///
    /// # Parameters
    /// - `fin`: Indicates if this frame is the final fragment in a message.
    /// - `opcode`: The operation code of the frame, defining its type.
    /// - `mask`: Optional 4-byte masking key for client-to-server frames.
    /// - `payload`: The compressed frame payload data.
    pub fn compress(
        fin: bool,
        opcode: OpCode,
        mask: Option<[u8; 4]>,
        payload: impl Into<BytesMut>,
    ) -> Self {
        Self {
            fin,
            opcode,
            mask,
            payload: payload.into(),
            is_compressed: true,
        }
    }

    /// Creates a new WebSocket close frame with a raw payload.
    ///
    /// This method does not validate if `payload` is a valid close frame payload.
    pub fn close_raw<T: AsRef<[u8]>>(payload: T) -> Self {
        Self {
            fin: true,
            opcode: OpCode::Close,
            mask: None,
            is_compressed: false,
            payload: BytesMut::from(payload.as_ref()),
        }
    }

    /// Checks if the frame payload is valid UTF-8.
    ///
    /// # Returns
    /// - `true` if the payload is valid UTF-8.
    /// - `false` otherwise.
    #[inline(always)]
    pub fn is_utf8(&self) -> bool {
        std::str::from_utf8(&self.payload).is_ok()
    }

    /// Returns whether the frame is masked.
    ///
    /// # Returns
    /// - `true` if the frame has a masking key.
    /// - `false` otherwise.
    #[inline(always)]
    pub(super) fn is_masked(&self) -> bool {
        self.mask.is_some()
    }

    /// Masks the payload using a masking key.
    ///
    /// If no masking key is set, a random key is generated and applied.
    pub(super) fn mask(&mut self) {
        let payload = &mut self.payload;
        if let Some(mask) = self.mask {
            crate::mask::apply_mask(payload, mask);
        } else {
            let mask: [u8; 4] = rand::random();
            crate::mask::apply_mask(payload, mask);
            self.mask = Some(mask);
        }
    }

    /// Unmasks the payload.
    ///
    /// This reverses any masking applied to the payload using the existing masking key.
    pub(super) fn unmask(&mut self) {
        if let Some(mask) = self.mask.take() {
            let payload = &mut self.payload;
            crate::mask::apply_mask(payload, mask);
        }
    }

    /// Formats the frame header into the provided `head` buffer and returns the size of the length field.
    ///
    /// # Parameters
    /// - `head`: The buffer to hold the formatted frame header.
    ///
    /// # Returns
    /// - The size of the length field (0, 2, 4, or 10 bytes).
    ///
    /// # Panics
    /// Panics if `head` is not large enough to hold the formatted header.
    pub(super) fn fmt_head(&self, head: &mut [u8]) -> usize {
        let compression = u8::from(self.is_compressed);
        head[0] = (self.fin as u8) << 7 | compression << 6 | u8::from(self.opcode);

        let len = self.payload.len();
        let size = if len < 126 {
            head[1] = len as u8;
            2
        } else if len < 65536 {
            head[1] = 126;
            head[2..4].copy_from_slice(&(len as u16).to_be_bytes());
            4
        } else {
            head[1] = 127;
            head[2..10].copy_from_slice(&(len as u64).to_be_bytes());
            10
        };

        if let Some(mask) = self.mask {
            head[1] |= 0x80;
            head[size..size + 4].copy_from_slice(&mask);
            size + 4
        } else {
            size
        }
    }
}

/// Unit tests for the `yawc::frame` module.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::close::CloseCode;
    use bytes::{Bytes, BytesMut};

    /// Tests for the `OpCode` enum.
    mod opcode_tests {
        use super::*;

        #[test]
        fn test_is_control() {
            // Control frames
            assert!(OpCode::Close.is_control());
            assert!(OpCode::Ping.is_control());
            assert!(OpCode::Pong.is_control());

            // Data frames
            assert!(!OpCode::Continuation.is_control());
            assert!(!OpCode::Text.is_control());
            assert!(!OpCode::Binary.is_control());
        }

        #[test]
        fn test_try_from_u8_valid() {
            assert_eq!(OpCode::try_from(0x0).unwrap(), OpCode::Continuation);
            assert_eq!(OpCode::try_from(0x1).unwrap(), OpCode::Text);
            assert_eq!(OpCode::try_from(0x2).unwrap(), OpCode::Binary);
            assert_eq!(OpCode::try_from(0x8).unwrap(), OpCode::Close);
            assert_eq!(OpCode::try_from(0x9).unwrap(), OpCode::Ping);
            assert_eq!(OpCode::try_from(0xA).unwrap(), OpCode::Pong);
        }

        #[test]
        fn test_try_from_u8_invalid() {
            // Invalid opcodes should return an error
            for &code in &[0x3, 0x4, 0x5, 0x6, 0x7, 0xB, 0xC, 0xD, 0xE, 0xF] {
                assert!(OpCode::try_from(code).is_err());
            }
        }

        #[test]
        fn test_from_opcode_to_u8() {
            assert_eq!(u8::from(OpCode::Continuation), 0x0);
            assert_eq!(u8::from(OpCode::Text), 0x1);
            assert_eq!(u8::from(OpCode::Binary), 0x2);
            assert_eq!(u8::from(OpCode::Close), 0x8);
            assert_eq!(u8::from(OpCode::Ping), 0x9);
            assert_eq!(u8::from(OpCode::Pong), 0xA);
        }
    }

    /// Tests for the `FrameView` struct.
    mod frameview_tests {
        use super::*;

        #[test]
        fn test_text_frameview() {
            let text = "Hello, WebSocket!";
            let frame = FrameView::text(text);

            assert_eq!(frame.opcode, OpCode::Text);
            assert_eq!(frame.payload, Bytes::from(text));
        }

        #[test]
        fn test_binary_frameview() {
            let data = vec![0x01, 0x02, 0x03];
            let frame = FrameView::binary(data.clone());

            assert_eq!(frame.opcode, OpCode::Binary);
            assert_eq!(frame.payload, Bytes::from(data));
        }

        #[test]
        fn test_close_frameview() {
            let reason = "Normal closure";
            let frame = FrameView::close(CloseCode::Normal, reason);

            assert_eq!(frame.opcode, OpCode::Close);

            // The payload should contain the close code (1000) and the reason
            let mut expected_payload = Vec::new();
            expected_payload.extend_from_slice(&1000u16.to_be_bytes());
            expected_payload.extend_from_slice(reason.as_bytes());

            assert_eq!(frame.payload, Bytes::from(expected_payload));
        }

        #[test]
        fn test_close_raw_frameview() {
            let payload = vec![0x03, 0xE8]; // Close code 1000 without reason
            let frame = FrameView::close_raw(payload.clone());

            assert_eq!(frame.opcode, OpCode::Close);
            assert_eq!(frame.payload, Bytes::from(payload));
        }

        #[test]
        fn test_ping_frameview() {
            let payload = b"Ping payload";
            let frame = FrameView::ping(&payload[..]);

            assert_eq!(frame.opcode, OpCode::Ping);
            assert_eq!(frame.payload, Bytes::from(&payload[..]));
        }

        #[test]
        fn test_pong_frameview() {
            let payload = b"Pong payload";
            let frame = FrameView::pong(&payload[..]);

            assert_eq!(frame.opcode, OpCode::Pong);
            assert_eq!(frame.payload, Bytes::from(&payload[..]));
        }

        #[test]
        fn test_from_frameview_to_tuple() {
            let frame = FrameView::text("Test");
            let (opcode, payload): (OpCode, Bytes) = frame.into();

            assert_eq!(opcode, OpCode::Text);
            assert_eq!(payload, Bytes::from("Test"));
        }

        #[test]
        fn test_from_tuple_to_frameview() {
            let opcode = OpCode::Binary;
            let payload = Bytes::from_static(b"\xDE\xAD\xBE\xEF");

            let frame = FrameView::from((opcode, payload.clone()));

            assert_eq!(frame.opcode, OpCode::Binary);
            assert_eq!(frame.payload, payload);
        }

        #[test]
        fn test_frameview_from_frame() {
            let frame = Frame::new(true, OpCode::Text, None, BytesMut::from("Hello"));
            let frame_view = FrameView::from(frame);

            assert_eq!(frame_view.opcode, OpCode::Text);
            assert_eq!(frame_view.payload, Bytes::from("Hello"));
        }
    }

    /// Tests for the `Frame` struct.
    mod frame_tests {
        use super::*;

        #[test]
        fn test_frame_new() {
            let payload = BytesMut::from("Test payload");
            let frame = Frame::new(true, OpCode::Text, None, payload.clone());

            assert!(frame.fin);
            assert_eq!(frame.opcode, OpCode::Text);
            assert_eq!(frame.mask, None);
            assert_eq!(frame.payload, payload);
            assert!(!frame.is_compressed);
        }

        /// Tests the creation and configuration of a compressed WebSocket frame.
        ///
        /// This test case demonstrates:
        /// - Creating a compressed frame with a payload
        /// - Setting frame to be non-final (fragmented)
        /// - Using binary opcode format
        /// - Applying a masking key
        /// - Verifying the compression flag is set
        ///
        /// # The test verifies:
        /// - fin flag is false (fragmented frame)
        /// - opcode is Binary
        /// - masking key is correctly set to [0xAA, 0xBB, 0xCC, 0xDD]
        /// - payload matches input data
        /// - compression flag is enabled
        #[test]
        fn test_frame_compress() {
            let payload = BytesMut::from("Compressed payload");
            let frame = Frame::compress(
                false,
                OpCode::Binary,
                Some([0xAA, 0xBB, 0xCC, 0xDD]),
                payload.clone(),
            );

            assert!(!frame.fin);
            assert_eq!(frame.opcode, OpCode::Binary);
            assert_eq!(frame.mask, Some([0xAA, 0xBB, 0xCC, 0xDD]));
            assert_eq!(frame.payload, payload);
            assert!(frame.is_compressed);
        }

        #[test]
        fn test_frame_is_utf8() {
            let valid_utf8 = BytesMut::from("Hello, 世界");
            let frame = Frame::new(true, OpCode::Text, None, valid_utf8);
            assert!(frame.is_utf8());

            let invalid_utf8 = BytesMut::from(&[0xFF, 0xFE, 0xFD][..]);
            let frame = Frame::new(true, OpCode::Text, None, invalid_utf8);
            assert!(!frame.is_utf8());
        }

        #[test]
        fn test_frame_mask_unmask() {
            let payload = BytesMut::from("Mask me");
            let mut frame = Frame::new(
                true,
                OpCode::Binary,
                Some([0x01, 0x02, 0x03, 0x04]),
                payload.clone(),
            );

            // Mask the payload
            frame.mask();
            assert_ne!(frame.payload, payload);

            // Unmask the payload
            frame.unmask();
            assert_eq!(frame.payload, payload);
            assert_eq!(frame.mask, None);
        }

        #[test]
        fn test_frame_fmt_head() {
            let payload = BytesMut::from("Header test");
            let mask_key = [0xAA, 0xBB, 0xCC, 0xDD];
            let frame = Frame::new(true, OpCode::Text, Some(mask_key), payload);

            let mut head = [0u8; MAX_HEAD_SIZE];
            let head_size = frame.fmt_head(&mut head);

            // Verify the head size is correct
            assert_eq!(head_size, 2 + 4); // Small payload (<126), so 2 bytes header + 4 bytes mask

            // Verify the FIN bit, RSV bits, and opcode
            assert_eq!(head[0], 0x81); // FIN=1, RSV1-3=0, OpCode=0x1 (Text)

            // Verify the MASK bit and payload length
            assert_eq!(head[1], 0x80 | 11); // MASK=1, Payload Len=11

            // Verify the masking key
            assert_eq!(&head[2..6], &mask_key);
        }

        #[test]
        fn test_frame_close_raw() {
            let payload = b"\x03\xE8Goodbye"; // Close code 1000 with reason "Goodbye"
            let frame = Frame::close_raw(payload);

            assert!(frame.fin);
            assert_eq!(frame.opcode, OpCode::Close);
            assert_eq!(frame.payload, BytesMut::from(&payload[..]));
        }

        #[test]
        fn test_frame_from_frameview() {
            let frame_view = FrameView::binary("Data");
            let frame = Frame::from(frame_view.clone());

            assert!(frame.fin);
            assert_eq!(frame.opcode, frame_view.opcode);
            assert_eq!(frame.payload.freeze(), frame_view.payload);
        }

        #[test]
        fn test_frame_is_masked() {
            let frame = Frame::new(true, OpCode::Text, Some([0x00; 4]), BytesMut::from("Test"));
            assert!(frame.is_masked());

            let frame = Frame::new(true, OpCode::Text, None, BytesMut::from("Test"));
            assert!(!frame.is_masked());
        }
    }
}
