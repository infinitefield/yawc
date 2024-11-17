use bytes::{Buf, BytesMut};
use tokio_util::codec;

use crate::{
    frame::{self, Frame, MAX_HEAD_SIZE},
    OpCode, WebSocketError,
};

/// Represents the reading state of a WebSocket frame.
enum ReadState {
    /// Currently reading the header of the frame.
    Header(Header),
    /// Currently reading the payload of the frame.
    Payload(HeaderAndMask),
}

/// Represents the initial header fields of a WebSocket frame.
struct Header {
    /// Indicates if this is the final fragment in a message.
    fin: bool,
    /// Compression flag indicating if payload is compressed.
    rsv1: bool,
    /// Indicates if the frame is masked.
    masked: bool,
    /// The operation code of the frame.
    opcode: OpCode,
    /// Additional length of the frame, if applicable.
    extra: usize,
    /// Encoded length of the payload.
    length_code: u8,
    /// Total size of the header in bytes.
    header_size: usize,
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
    pub fn new(max_payload_size: usize) -> Self {
        Self {
            state: None,
            max_payload_size,
        }
    }
}

/// Contains header and mask data after decoding the bytes before the payload.
struct HeaderAndMask {
    /// Decoded header fields.
    header: Header,
    /// Optional masking key for decoding the payload.
    mask: Option<[u8; 4]>,
    /// Length of the payload, in bytes.
    payload_len: usize,
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
        loop {
            match self.state.take() {
                None => {
                    // Check if enough data is available for basic header
                    if src.remaining() < 2 {
                        return Ok(None);
                    }

                    // Parse initial header bytes
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
                    let header_size = extra + masked as usize * 4;
                    src.advance(2);

                    self.state = Some(ReadState::Header(Header {
                        fin,
                        rsv1,
                        masked,
                        opcode,
                        length_code,
                        extra,
                        header_size,
                    }));
                }
                Some(ReadState::Header(header)) => {
                    // Check if enough data is available for the full header
                    if src.remaining() < header.header_size {
                        self.state = Some(ReadState::Header(header));
                        return Ok(None);
                    }

                    // Parse payload length based on `extra` field size
                    let payload_len: usize = match header.extra {
                        0 => usize::from(header.length_code),
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

                    // Parse the optional mask key if `masked` is true
                    let mask = if header.masked {
                        Some(src.get_u32().to_be_bytes())
                    } else {
                        None
                    };

                    // Validate control frame requirements
                    if header.opcode.is_control() && !header.fin {
                        return Err(WebSocketError::ControlFrameFragmented);
                    }
                    if header.opcode == OpCode::Ping && payload_len > 125 {
                        return Err(WebSocketError::PingFrameTooLarge);
                    }
                    if payload_len >= self.max_payload_size {
                        return Err(WebSocketError::FrameTooLarge);
                    }

                    self.state = Some(ReadState::Payload(HeaderAndMask {
                        header,
                        mask,
                        payload_len,
                    }));
                }
                Some(ReadState::Payload(header_and_mask)) => {
                    // Check if enough data is available for the full payload
                    if src.remaining() < header_and_mask.payload_len {
                        self.state = Some(ReadState::Payload(header_and_mask));
                        return Ok(None);
                    }

                    let header = header_and_mask.header;
                    let mask = header_and_mask.mask;
                    let payload_len = header_and_mask.payload_len;

                    let payload = src.split_to(payload_len).freeze();
                    let mut frame = Frame::new(header.fin, header.opcode, mask, payload);
                    frame.is_compressed = header.rsv1;

                    break Ok(Some(frame));
                }
            }
        }
    }
}

/// WebSocket frame encoder for serializing `Frame` instances into a buffer.
///
/// `Encoder` formats a `Frame` header and payload into a `BytesMut` buffer, preparing
/// it for transmission over the network. The encoder is responsible for serializing
/// headers and appending payloads in the correct format.
///
/// # Errors
/// Returns `WebSocketError` if any issues arise during encoding.
pub struct Encoder;

impl codec::Encoder<Frame> for Encoder {
    type Error = WebSocketError;

    /// Encodes a `Frame` into the provided buffer.
    ///
    /// This method formats the frame's header and appends the payload to the destination buffer.
    ///
    /// # Parameters
    /// - `frame`: The `Frame` to be encoded.
    /// - `dst`: A mutable reference to a `BytesMut` buffer where the encoded frame will be written.
    ///
    /// # Returns
    /// - `Ok(())` if encoding is successful.
    /// - `Err(WebSocketError)` if encoding fails.
    fn encode(&mut self, frame: Frame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let mut header = [0; MAX_HEAD_SIZE];
        let size = frame.fmt_head(&mut header[..]);

        dst.extend_from_slice(&header[..size]);
        dst.extend_from_slice(&frame.payload);

        Ok(())
    }
}
