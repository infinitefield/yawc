//! # codec
//!
//! WebSocket codec implementation for frame encoding and decoding.
//!
//! This module provides the **lowest layer** of the WebSocket processing stack, handling
//! the raw byte-level encoding and decoding of WebSocket frames according to RFC 6455.
//!
//! ## Architecture Layer: Tokio Codec
//!
//! The codec layer is responsible for:
//! - **Frame decoding**: Parsing raw bytes from the network into structured [`Frame`] objects
//! - **Frame encoding**: Serializing [`Frame`] objects into raw bytes for network transmission
//! - **Header parsing**: Extracting FIN, RSV1-3, OpCode, and mask bits
//! - **Masking/unmasking**: Applying and removing XOR masks as per RFC 6455
//! - **Payload length handling**: Supporting 7-bit, 16-bit, and 64-bit payload lengths
//!
//! ## What the Codec Does NOT Handle
//!
//! The codec operates at the frame level only. It does **not** handle:
//! - Fragment assembly (handled by [`ReadHalf`](crate::native::split::ReadHalf))
//! - Decompression (handled by [`WebSocket`](crate::WebSocket))
//! - UTF-8 validation (handled by [`WebSocket`](crate::WebSocket))
//! - Protocol control (Ping/Pong/Close handling)
//!
//! ## Components
//!
//! - [`Codec`]: Combined encoder/decoder for bidirectional WebSocket communication
//! - [`Decoder`]: Parses raw bytes into individual WebSocket frames
//! - [`Encoder`]: Serializes WebSocket frames into raw bytes for transmission
//!
//! ## Example Data Flow
//!
//! **Receiving a fragmented compressed message:**
//! ```text
//! Network bytes → Decoder → Frame(OpCode::Text, RSV1=1, FIN=0)
//! Network bytes → Decoder → Frame(OpCode::Continuation, RSV1=0, FIN=0)
//! Network bytes → Decoder → Frame(OpCode::Continuation, RSV1=0, FIN=1)
//! ```
//!
//! The decoder simply returns individual frames. Fragment assembly happens at the
//! [`ReadHalf`](crate::native::split::ReadHalf) layer, and decompression happens at the
//! [`WebSocket`](crate::WebSocket) layer.

use bytes::{Buf, Bytes, BytesMut};
use tokio_util::codec;

use crate::{
    frame::{self, Frame, OpCode, MAX_HEAD_SIZE},
    Role, WebSocketError,
};

/// Compact reading state using bit packing (like uWebSockets).
/// Total size: 16 bytes on 64-bit (vs 24 bytes before)
#[repr(C)]
struct ReadState {
    /// Packed flags: fin (1 bit) | rsv1 (1 bit) | opcode (4 bits) | has_mask (1 bit)
    flags: u8,
    /// Masking key bytes (4 bytes)
    mask: [u8; 4],
    /// Reserved for alignment (3 bytes)
    _reserved: [u8; 3],
    /// Length of the payload (8 bytes)
    payload_len: usize,
}

impl ReadState {
    #[inline(always)]
    fn new(
        fin: bool,
        rsv1: bool,
        opcode: OpCode,
        mask: Option<[u8; 4]>,
        payload_len: usize,
    ) -> Self {
        let flags = ((fin as u8) << 7)
            | ((rsv1 as u8) << 6)
            | ((opcode as u8) & 0x0F)
            | if mask.is_some() { 0x10 } else { 0 };

        Self {
            flags,
            mask: mask.unwrap_or([0; 4]),
            _reserved: [0; 3],
            payload_len,
        }
    }

    #[inline(always)]
    fn fin(&self) -> bool {
        self.flags & 0x80 != 0
    }

    #[inline(always)]
    fn rsv1(&self) -> bool {
        self.flags & 0x40 != 0
    }

    #[inline(always)]
    fn opcode(&self) -> OpCode {
        // SAFETY: We only store valid opcodes
        unsafe { std::mem::transmute(self.flags & 0x0F) }
    }

    #[inline(always)]
    fn mask(&self) -> Option<[u8; 4]> {
        if self.flags & 0x10 != 0 {
            Some(self.mask)
        } else {
            None
        }
    }
}

/// A combined codec that provides both encoding and decoding functionality for WebSocket frames.
///
/// The `Codec` struct combines a `Decoder` for parsing incoming WebSocket frames and an
/// `Encoder` for serializing outgoing frames. This provides a complete interface for
/// bidirectional WebSocket frame processing.
///
/// This codec can be used with Tokio's framed streams to handle WebSocket protocol
/// frame encoding and decoding.
pub struct Codec {
    decoder: Decoder,
    encoder: Encoder,
}

impl From<(Decoder, Encoder)> for Codec {
    fn from((decoder, encoder): (Decoder, Encoder)) -> Self {
        Self { decoder, encoder }
    }
}

impl codec::Decoder for Codec {
    type Item = <Decoder as codec::Decoder>::Item;
    type Error = <Decoder as codec::Decoder>::Error;

    #[inline]
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        self.decoder.decode(src)
    }
}

impl codec::Encoder<Frame> for Codec {
    type Error = <Encoder as codec::Encoder<Frame>>::Error;

    #[inline]
    fn encode(&mut self, item: Frame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        self.encoder.encode(item, dst)
    }
}

/// A decoder for WebSocket frames, handling state transitions.
///
/// `Decoder` manages WebSocket frame parsing, including tracking the maximum allowed payload size
/// and current state. The decoder state changes as each part of the frame (header and payload) is processed.
pub struct Decoder {
    role: Role,
    /// Current reading state (header or payload).
    state: Option<ReadState>,
    /// Maximum allowed size for the frame payload.
    max_payload_size: usize,
}

impl Decoder {
    /// Creates a new `Decoder` with a specified maximum payload size.
    ///
    /// # Parameters
    /// - `max_payload_size`: The maximum allowed payload size, in bytes.
    ///
    /// # Returns
    /// A `Decoder` instance configured to limit payloads to `max_payload_size`.
    pub fn new(role: Role, max_payload_size: usize) -> Self {
        Self {
            role,
            state: None,
            max_payload_size,
        }
    }
}

impl codec::Decoder for Decoder {
    type Item = Frame;
    type Error = WebSocketError;

    /// Decodes WebSocket frames from a `BytesMut` buffer, managing header and payload parsing.
    ///
    /// The `decode` function parses the header and payload in stages, maintaining state across calls.
    /// It handles control frame validation, masking, payload length constraints, and checks for
    /// reserved bits. This function transitions between states based on the completeness of the data
    /// in the buffer, returning a decoded `Frame` once all parts are processed.
    ///
    /// # Parameters
    /// - `src`: A mutable reference to a `BytesMut` buffer containing the raw frame data.
    ///
    /// # Returns
    /// - `Ok(Some(Frame))`: Returns a fully decoded `Frame` when successful.
    /// - `Ok(None)`: Indicates more data is needed to complete the frame.
    /// - `Err(WebSocketError)`: If a protocol violation or invalid frame structure is detected.
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // If we have a partial frame state, try to complete it
        if let Some(state) = self.state.take() {
            if src.remaining() < state.payload_len {
                self.state = Some(state);
                return Ok(None);
            }

            // We have enough data for the payload
            if self.role == Role::Server {
                let Some(mask) = state.mask() else {
                    return Err(WebSocketError::InvalidFragment);
                };
                crate::mask::apply_mask(&mut src[..state.payload_len], mask);
            }

            let payload = src.split_to(state.payload_len).freeze();
            let mut frame = Frame::new(state.fin(), state.opcode(), state.mask(), payload);
            frame.is_compressed = state.rsv1();
            return Ok(Some(frame));
        }

        // Parse new frame header
        if src.remaining() < 2 {
            return Ok(None);
        }

        let fin = src[0] & 0b10000000 != 0;
        let rsv1 = src[0] & 0b01000000 != 0;

        // Check reserved bits
        if src[0] & 0b00110000 != 0 {
            return Err(WebSocketError::ReservedBitsNotZero);
        }

        let opcode = frame::OpCode::try_from(src[0] & 0b00001111)?;
        let masked = src[1] & 0b10000000 != 0;
        let length_code = src[1] & 0x7F;

        // Determine additional header length
        let extra = match length_code {
            126 => 2,
            127 => 8,
            _ => 0,
        };
        let header_size = 2 + extra + (masked as usize * 4);

        // Check if we have the full header
        if src.remaining() < header_size {
            return Ok(None);
        }

        src.advance(2);

        // Parse payload length
        let payload_len: usize = match extra {
            0 => usize::from(length_code),
            2 => src.get_u16() as usize,
            #[cfg(target_pointer_width = "64")]
            8 => src.get_u64() as usize,
            #[cfg(any(target_pointer_width = "16", target_pointer_width = "32"))]
            8 => match usize::try_from(src.get_u64()) {
                Ok(length) => length,
                Err(_) => return Err(WebSocketError::FrameTooLarge),
            },
            _ => unreachable!(),
        };

        // Parse optional mask
        let mask = if masked {
            Some(src.get_u32().to_be_bytes())
        } else {
            None
        };

        // Validate control frame requirements
        if opcode.is_control() && !fin {
            return Err(WebSocketError::ControlFrameFragmented);
        }
        if opcode == OpCode::Ping && payload_len > 125 {
            return Err(WebSocketError::PingFrameTooLarge);
        }
        if payload_len >= self.max_payload_size {
            return Err(WebSocketError::FrameTooLarge);
        }

        // Check if we have the full payload
        if src.remaining() < payload_len {
            // Save state and wait for more data
            self.state = Some(ReadState::new(fin, rsv1, opcode, mask, payload_len));
            return Ok(None);
        }

        // We have everything, decode immediately
        if self.role == Role::Server {
            let Some(mask) = mask else {
                return Err(WebSocketError::InvalidFragment);
            };
            crate::mask::apply_mask(&mut src[..payload_len], mask);
        }

        let payload = src.split_to(payload_len).freeze();
        let mut frame = Frame::new(fin, opcode, mask, payload);
        frame.is_compressed = rsv1;
        Ok(Some(frame))
    }
}

/// WebSocket frame encoder for serializing `Frame` instances into a buffer.
///
/// `Encoder` formats a `Frame` header and payload into a `BytesMut` buffer, preparing
/// it for transmission over the network. The encoder is responsible for serializing
/// headers and appending payloads in the correct format.
///
/// If a fragmentation threshold is set, it will automatically fragment frames into chunks of
/// the appropiate size
///
/// # Errors
/// Returns `WebSocketError` if any issues arise during encoding.
pub struct Encoder {
    role: Role,
    fragmentation_threshold: Option<usize>,
}

impl Encoder {
    pub fn new(role: Role, fragmentation_threshold: Option<usize>) -> Self {
        Self {
            role,
            fragmentation_threshold,
        }
    }

    fn serialize_frame(&self, mut frame: Frame, dst: &mut BytesMut) {
        if self.role == Role::Client {
            // ensure the mask is set
            frame.set_random_mask_if_not_set();
        }
        frame.write_head(dst);

        let index = dst.len();
        dst.extend_from_slice(&frame.payload);

        if let Some(mask) = frame.mask {
            crate::mask::apply_mask(&mut dst[index..], mask);
        }
    }

    fn should_fragment_frame(&self, frame: &Frame) -> bool {
        !(self.fragmentation_threshold.is_none()
            || frame.opcode.is_control()
            || frame.payload.is_empty()
            || !frame.is_fin()
            || frame.opcode == OpCode::Continuation)
    }
}

impl codec::Encoder<Frame> for Encoder {
    type Error = WebSocketError;

    /// Encodes a `Frame` into the provided buffer.
    ///
    /// This method formats the frame's header and appends the payload to the destination buffer.
    ///
    /// If a fragmentation threshold is set, it will automatically fragment frames into chunks of
    /// the appropiate size
    ///
    /// # Parameters
    /// - `frame`: The `Frame` to be encoded.
    /// - `dst`: A mutable reference to a `BytesMut` buffer where the encoded frame will be written.
    ///
    /// # Returns
    /// - `Ok(())` if encoding is successful.
    /// - `Err(WebSocketError)` if encoding fails.
    fn encode(&mut self, frame: Frame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let payload_len = frame.payload.len();
        if self.should_fragment_frame(&frame) {
            let fragments = Fragments::new(
                frame.payload,
                // we check before if it's `None`, so we can't panic
                self.fragmentation_threshold.unwrap(),
                frame.opcode,
                frame.is_compressed,
            );
            dst.reserve(MAX_HEAD_SIZE * fragments.len() + payload_len);

            for frame in fragments {
                self.serialize_frame(frame, dst);
            }
        } else {
            dst.reserve(MAX_HEAD_SIZE + payload_len);
            self.serialize_frame(frame, dst);
        }
        Ok(())
    }
}

struct Fragments {
    payload: Bytes,
    max_payload: usize,
    opcode: OpCode,
    compressed: bool,
}

impl Fragments {
    fn new(payload: Bytes, max_payload: usize, opcode: OpCode, compressed: bool) -> Self {
        debug_assert!(!payload.is_empty());
        Self {
            payload,
            max_payload,
            opcode,
            compressed,
        }
    }

    fn len(&self) -> usize {
        self.payload.len().div_ceil(self.max_payload)
    }
}

impl Iterator for Fragments {
    type Item = Frame;

    fn next(&mut self) -> Option<Self::Item> {
        if self.payload.is_empty() {
            return None;
        }

        let chunk = if self.payload.len() > self.max_payload {
            self.payload.split_to(self.max_payload)
        } else {
            self.payload.split_off(0)
        };

        let fin = self.payload.is_empty();

        let mut frame = Frame::new(fin, self.opcode, None, chunk);
        frame.is_compressed = self.compressed;

        // Modify state for subsequent calls to next. The rest of the frames need to be
        // continuation frames, and the compressed flag needs to be false (only the first frame in
        // a compressed fragmented message needs to have the compressed flag set). This way we
        // also avoid branches and extra state to see if this is the first frame being produced
        self.opcode = OpCode::Continuation;
        self.compressed = false;

        Some(frame)
    }
}
