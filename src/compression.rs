use std::io;

use bytes::{BufMut, Bytes, BytesMut};
use flate2::{CompressError, DecompressError, FlushCompress, Status};

use nom::{
    bytes::complete::{tag, take_while1},
    character::complete::{digit1, space0},
    combinator::opt,
    sequence::{pair, preceded},
    IResult, Parser,
};

use crate::{CompressionLevel, DeflateOptions, Role, WebSocketError};

static PERMESSAGE_DEFLATE: &str = "permessage-deflate";

/// Handler for permessage-deflate negotiation in WebSocket connections.
///
/// `WebSocketExtensions` facilitates the negotiation of compression parameters between
/// the client and server during a WebSocket handshake. Compression parameters are negotiated
/// based on compatibility with the other party's settings, where:
/// - A server will typically accept the client’s parameters if compatible with its own settings.
/// - A client will accept the server's parameters as specified.
///
/// The permessage-deflate extension provides options such as window size and context takeover
/// for both server and client. By default, these values are unset or set to conservative defaults,
/// and can be modified through [`DeflateOptions`].
#[derive(Debug, Clone, Default)]
pub struct WebSocketExtensions {
    pub(super) server_max_window_bits: Option<Option<u8>>,
    pub(super) client_max_window_bits: Option<Option<u8>>,
    pub(super) server_no_context_takeover: bool,
    pub(super) client_no_context_takeover: bool,
}

impl WebSocketExtensions {
    /// Reads the `Sec-WebSocket-Extensions` header out of a handshake message.
    ///
    /// A missing header and one that fails to parse are both treated as "nothing
    /// negotiated": the peer offered nothing this side can act on, and the connection
    /// proceeds without extensions rather than failing. Every handshake path, client and
    /// server, HTTP/1.1 and HTTP/2, reads the header this way.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn from_headers(headers: &hyper::HeaderMap) -> Option<Self> {
        use std::str::FromStr;

        headers
            .get(hyper::header::SEC_WEBSOCKET_EXTENSIONS)
            .and_then(|value| value.to_str().ok())
            .map(Self::from_str)
            .and_then(std::result::Result::ok)
    }

    /// Resolves what a server should answer with, given what the client offered.
    ///
    /// Compression is only negotiated when both sides want it: a client that offered
    /// nothing gets nothing, and a server with compression disabled ignores whatever was
    /// offered. Otherwise the two are merged into the terms both can honor.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn agree(server: Option<&DeflateOptions>, client: Option<Self>) -> Option<Self> {
        match (server, client) {
            (Some(server), Some(client)) => Some(server.merge(&client)),
            _ => None,
        }
    }
}

impl<'a> From<&'a DeflateOptions> for WebSocketExtensions {
    /// Converts [`DeflateOptions`] into `WebSocketExtensions`, configuring the extensions
    /// for negotiation based on the specified compression settings.
    fn from(value: &'a DeflateOptions) -> Self {
        Self {
            #[cfg(feature = "zlib")]
            server_max_window_bits: value.server_max_window_bits.map(Some),
            #[cfg(not(feature = "zlib"))]
            server_max_window_bits: None,
            #[cfg(feature = "zlib")]
            client_max_window_bits: value.client_max_window_bits.map(Some),
            #[cfg(not(feature = "zlib"))]
            client_max_window_bits: None,
            server_no_context_takeover: value.server_no_context_takeover,
            client_no_context_takeover: value.client_no_context_takeover,
        }
    }
}

impl std::fmt::Display for WebSocketExtensions {
    /// Formats the `WebSocketExtensions` parameters as a permessage-deflate string
    /// for use in the WebSocket handshake headers.
    ///
    /// The output string includes any applicable `server_max_window_bits`, `client_max_window_bits`,
    /// `server_no_context_takeover`, and `client_no_context_takeover` options.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{PERMESSAGE_DEFLATE}")?;

        match self.server_max_window_bits {
            Some(Some(bits)) if (9..16).contains(&bits) => {
                write!(f, "; server_max_window_bits={bits}")?;
            }
            Some(_) => {
                write!(f, "; server_max_window_bits")?;
            }
            None => {}
        }

        match self.client_max_window_bits {
            Some(Some(bits)) if (9..16).contains(&bits) => {
                write!(f, "; client_max_window_bits={bits}")?;
            }
            Some(_) => {
                write!(f, "; client_max_window_bits")?;
            }
            None => {}
        }

        if self.server_no_context_takeover {
            write!(f, "; server_no_context_takeover")?;
        }
        if self.client_no_context_takeover {
            write!(f, "; client_no_context_takeover")?;
        }

        Ok(())
    }
}

impl WebSocketExtensions {
    /// Parses a permessage-deflate extension string to configure `WebSocketExtensions`.
    ///
    /// This method takes an input string from a WebSocket handshake header and parses it
    /// to set parameters for `client_no_context_takeover`, `server_no_context_takeover`,
    /// `server_max_window_bits`, and `client_max_window_bits`. It will ignore unrecognized
    /// keys.
    ///
    /// # Parameters
    /// - `input`: The extension string to parse.
    ///
    /// # Returns
    /// - `Ok(Self)`: A configured `WebSocketExtensions` instance if parsing is successful.
    /// - `Err(nom::Err)`: An error if parsing fails due to an unexpected format.
    fn parse(input: &str) -> Result<Self, nom::Err<nom::error::Error<&str>>> {
        let mut this = Self::default();
        let (remaining, _) = tag(PERMESSAGE_DEFLATE)(input)?;
        this.parse_extensions(remaining)?;
        Ok(this)
    }

    /// Parses individual permessage-deflate extension parameters from the input string.
    ///
    /// This method iterates through extension parameters in the format of
    /// `key=value` pairs (e.g., `server_max_window_bits=15`). Keys are mapped to
    /// corresponding settings within `WebSocketExtensions`.
    ///
    /// # Parameters
    /// - `input`: The remaining portion of the extension string after the initial `PERMESSAGE_DEFLATE` tag.
    ///
    /// # Returns
    /// - `Ok(())`: If parsing is successful and parameters are set accordingly.
    /// - `Err(nom::Err)`: If parsing fails due to an invalid format.
    fn parse_extensions<'a>(
        &mut self,
        mut input: &'a str,
    ) -> Result<(), nom::Err<nom::error::Error<&'a str>>> {
        while !input.is_empty() {
            let (remaining, (key, value)) = Self::parse_extension(input)?;
            match key {
                "client_no_context_takeover" => {
                    self.client_no_context_takeover = true;
                }
                "server_no_context_takeover" => {
                    self.server_no_context_takeover = true;
                }
                "server_max_window_bits" => {
                    self.server_max_window_bits = Some(value.and_then(|v| v.parse().ok()));
                }
                "client_max_window_bits" => {
                    self.client_max_window_bits = Some(value.and_then(|v| v.parse().ok()));
                }
                _ => {}
            }

            input = remaining;
        }

        Ok(())
    }

    /// Parses a single extension parameter from the input string.
    ///
    /// This method identifies key-value pairs in the form `key=value` and returns both
    /// the key and an optional value if it exists. The method handles spaces around
    /// both the semicolon separator and equals sign.
    ///
    /// # Parameters
    /// - `input`: A string containing a single extension parameter, prefixed with a semicolon (`;`).
    ///
    /// # Returns
    /// - `IResult<&str, (&str, Option<&str>)>`: The remaining input after the parsed key-value pair,
    ///   along with a tuple of the key and optional value.
    fn parse_extension(input: &str) -> IResult<&str, (&str, Option<&str>)> {
        // ; server_no_context_takeover
        let mut parser = preceded(
            // allow strings preceded by spaces
            preceded(space0, tag(";")),
            preceded(
                space0,
                pair(
                    take_while1(|c: char| c.is_alphanumeric() || c == '_'),
                    opt(preceded(
                        // allow space precedence before the `=`
                        preceded(space0, tag("=")),
                        preceded(space0, opt(digit1)),
                    )),
                ),
            ),
        );

        parser
            .parse(input)
            .map(|(key, (key2, value))| (key, (key2, value.flatten())))
    }
}

/// Parses the permessage-deflate extension from the `Sec-WebSocket-Extensions` header.
///
/// This implementation of `FromStr` for `WebSocketExtensions` enables parsing directly from
/// a header string to configure compression settings for WebSocket connections.
///
/// # Parameters
/// - `input`: The string from the `Sec-WebSocket-Extensions` header containing the extension options.
///
/// # Returns
/// - `Ok(WebSocketExtensions)`: A configured `WebSocketExtensions` instance if parsing succeeds.
/// - `Err(String)`: An error message if parsing fails.
///
impl std::str::FromStr for WebSocketExtensions {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input).map_err(|err| err.to_string())
    }
}
/// Deflate settings for one direction of a connection.
///
/// Which of the negotiated `client_*`/`server_*` values end up here depends on the role,
/// and that mapping is applied once in [`CompressionConfig::resolve`], so nothing
/// downstream has to reason about it again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HalfConfig {
    /// Reset the dictionary after every message.
    pub(crate) no_context_takeover: bool,
    /// LZ77 window size, already clamped to the range deflate accepts.
    pub(crate) window_bits: Option<u8>,
}

/// The window sizes deflate accepts. Peers may negotiate values outside this range, and
/// the RFC 7692 lower bound of 8 is not usable with every backend.
const WINDOW_BITS: std::ops::RangeInclusive<u8> = 9..=15;

/// Resolved permessage-deflate configuration for a connection.
///
/// Built once at handshake time from the negotiated extension parameters, with the role
/// already applied, so the read and write paths just use `incoming`/`outgoing`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CompressionConfig {
    level: CompressionLevel,
    /// Settings for the direction we compress.
    outgoing: HalfConfig,
    /// Settings for the direction we decompress.
    incoming: HalfConfig,
}

impl CompressionConfig {
    /// Resolves the negotiated extension parameters into per-direction settings.
    ///
    /// Returns `Ok(None)` when permessage-deflate was not negotiated.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::CompressionNotSupported`] when the peer negotiated the
    /// extension but this side never offered compression. For a client that is the
    /// RFC 6455, Section 4.1 case of a server returning an extension the client did not
    /// ask for, which the client must fail on.
    pub(crate) fn resolve(
        extensions: Option<&WebSocketExtensions>,
        level: Option<CompressionLevel>,
        role: Role,
    ) -> crate::Result<Option<Self>> {
        let Some(extensions) = extensions else {
            return Ok(None);
        };

        let Some(level) = level else {
            return Err(WebSocketError::CompressionNotSupported);
        };

        let clamp = |bits: Option<Option<u8>>| {
            bits.flatten()
                .map(|bits| bits.clamp(*WINDOW_BITS.start(), *WINDOW_BITS.end()))
        };

        let client = HalfConfig {
            no_context_takeover: extensions.client_no_context_takeover,
            window_bits: clamp(extensions.client_max_window_bits),
        };
        let server = HalfConfig {
            no_context_takeover: extensions.server_no_context_takeover,
            window_bits: clamp(extensions.server_max_window_bits),
        };

        // A peer compresses with its own settings and decompresses with the other side's.
        let (outgoing, incoming) = match role {
            Role::Client => (client, server),
            Role::Server => (server, client),
        };

        Ok(Some(Self {
            level,
            outgoing,
            incoming,
        }))
    }

    /// Builds the compressor for the direction this side writes.
    pub(crate) fn compressor(&self) -> Compressor {
        Compressor::new(self.level, self.outgoing)
    }

    /// Builds the decompressor for the direction this side reads.
    pub(crate) fn decompressor(&self, max_decoded_message_size: usize) -> Decompressor {
        Decompressor::new_with_limit(self.incoming, max_decoded_message_size)
    }
}

/// Compresses WebSocket payloads with permessage-deflate.
///
/// In no-context-takeover mode the dictionary is reset after each message, lowering
/// memory use at the cost of compression ratio.
pub struct Compressor {
    deflate: Deflate,
    no_context_takeover: bool,
}

impl Compressor {
    /// Creates a compressor for one direction of a connection.
    pub(crate) fn new(level: CompressionLevel, config: HalfConfig) -> Self {
        Self {
            deflate: match config.window_bits {
                #[cfg(feature = "zlib")]
                Some(window_bits) => Deflate::new_with_window_bits(level, window_bits),
                #[cfg(not(feature = "zlib"))]
                Some(_) => Deflate::new(level),
                None => Deflate::new(level),
            },
            no_context_takeover: config.no_context_takeover,
        }
    }

    /// Compresses the given input data and returns the compressed output.
    ///
    /// # Parameters
    /// - `input`: The data slice to compress.
    /// - `flush`: Whether to flush the compressor (typically true for final frames).
    pub fn compress(&mut self, input: &[u8], flush: bool) -> io::Result<Bytes> {
        let res = self.deflate.compress(input, flush);
        if flush && self.no_context_takeover {
            self.deflate.reset();
        }
        res
    }
}

/// A Deflate compressor for WebSocket payloads, supporting both contextual and no-context-takeover compression.
///
/// `Deflate` wraps around the `flate2` library, providing efficient compression with configurable compression levels
/// and optional window bits (when `zlib` feature is enabled). It maintains an internal output buffer and handles
/// streaming compression, allowing for both contextual compression (where the compression dictionary is retained across frames)
/// and no-context-takeover mode (where the dictionary is reset after each frame).
struct Deflate {
    output: BytesMut,
    compress: flate2::Compress,
}

impl Deflate {
    /// Creates a new `Deflate` compressor with the specified compression level.
    fn new(level: CompressionLevel) -> Self {
        Self {
            output: BytesMut::with_capacity(1024),
            compress: flate2::Compress::new(level, false),
        }
    }

    /// Creates a new `Deflate` compressor with a specific compression level and window size for LZ77.
    #[cfg(feature = "zlib")]
    fn new_with_window_bits(level: CompressionLevel, window_bits: u8) -> Self {
        Self {
            output: BytesMut::with_capacity(1024),
            compress: flate2::Compress::new_with_window_bits(level, false, window_bits),
        }
    }

    /// Resets the compression dictionary, for no-context-takeover mode.
    fn reset(&mut self) {
        self.compress.reset();
    }

    /// Compresses input data while maintaining compression context across frames.
    fn compress(&mut self, mut input: &[u8], flush: bool) -> io::Result<Bytes> {
        while !input.is_empty() {
            let consumed = self.write(input)?;
            input = &input[consumed..];
        }

        if flush {
            self.flush()
        } else {
            // Return buffered data without flushing
            Ok(self.output.split().freeze())
        }
    }

    /// Writes a chunk of data to the output buffer during compression.
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let output = &mut self.output;
        let compressor = &mut self.compress;

        let dst = chunk(output);

        let before_out = compressor.total_out();
        let before_in = compressor.total_in();

        // partially flush the buffer
        let status = compressor.compress(input, dst, flate2::FlushCompress::Partial);

        let written = (compressor.total_out() - before_out) as usize;
        let consumed = (compressor.total_in() - before_in) as usize;

        unsafe { output.advance_mut(written) };

        match status {
            Ok(Status::Ok) => Ok(consumed),
            Ok(Status::StreamEnd | Status::BufError) | Err(..) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "corrupt deflate stream",
            )),
        }
    }

    /// Flushes the compressor, syncing any pending data and returning the accumulated output buffer.
    fn flush(&mut self) -> io::Result<Bytes> {
        let output = &mut self.output;
        let compressor = &mut self.compress;

        loop {
            let dst = chunk(output);
            let before_out = compressor.total_out();

            compressor
                .compress(&[], dst, FlushCompress::Sync)
                .map_err(deflate_error)?;

            let written = (compressor.total_out() - before_out) as usize;
            unsafe { output.advance_mut(written) };

            // FlushCompress::Sync writes the end of the stream, indicating the stream is finished
            if output.ends_with(&[0x0, 0x0, 0xff, 0xff]) {
                output.truncate(output.len() - 4);
                break;
            }
        }

        Ok(output.split().freeze())
    }
}

/// ignore the mapping input and print out a specific error.
fn deflate_error(err: CompressError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("Compression error: {err}"),
    )
}

fn inflate_error(err: DecompressError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("Decompression error: {err}"),
    )
}

/// Returns a mutable slice to the next available chunk of memory in the BytesMut buffer.
///
/// This function ensures that there's always at least 1024 bytes available in the returning byte slice.
fn chunk(output: &mut BytesMut) -> &mut [u8] {
    // always ensure there's 1024 bytes available
    if output.capacity() - output.len() < 1024 {
        output.reserve(1024);
    }

    let uninitbuf = output.spare_capacity_mut();
    unsafe { &mut *(uninitbuf as *mut [std::mem::MaybeUninit<u8>] as *mut [u8]) }
}

/// Decompresses WebSocket payloads compressed with permessage-deflate.
///
/// In no-context-takeover mode the dictionary is reset after each message, matching a
/// peer that compresses the same way.
pub struct Decompressor {
    inflate: Inflate,
    no_context_takeover: bool,
}

impl Decompressor {
    /// Creates a decompressor for one direction of a connection.
    #[cfg(test)]
    pub(crate) fn new(config: HalfConfig) -> Self {
        Self::new_with_limit(config, usize::MAX)
    }

    /// Creates a decompressor with a maximum decompressed message size.
    pub(crate) fn new_with_limit(config: HalfConfig, max_decoded_message_size: usize) -> Self {
        Self {
            inflate: match config.window_bits {
                #[cfg(feature = "zlib")]
                Some(window_bits) => {
                    Inflate::new(max_decoded_message_size).with_window_bits(window_bits)
                }
                #[cfg(not(feature = "zlib"))]
                Some(_) => Inflate::new(max_decoded_message_size),
                None => Inflate::new(max_decoded_message_size),
            },
            no_context_takeover: config.no_context_takeover,
        }
    }

    /// Decompresses a compressed data frame.
    ///
    /// `stream_end` marks the final frame of a message, which triggers the handling
    /// permessage-deflate requires (the 4-byte suffix, and the dictionary reset in
    /// no-context-takeover mode).
    pub fn decompress(&mut self, input: &[u8], stream_end: bool) -> io::Result<Bytes> {
        let res = self.inflate.decompress(input, stream_end);
        if stream_end && self.no_context_takeover {
            self.inflate.reset();
        }
        res
    }
}

/// An inflater for decompressing WebSocket payloads using the Deflate algorithm.
///
/// `Inflate` is designed for WebSocket permessage-deflate decompression, supporting both contextual
/// decompression and no-context-takeover mode. It utilizes the `flate2` crate to handle the decompression
/// process and provides internal buffering for efficient streaming decompression.
struct Inflate {
    output: BytesMut,
    decompress: flate2::Decompress,
    max_decoded_message_size: usize,
    decoded_message_size: usize,
    /// Set when the peer terminated the deflate stream with a final block (BFINAL=1).
    ///
    /// Once that happens the inflater cannot consume any more input, so the remaining
    /// bytes are dropped and the context is reset before the next message.
    stream_ended: bool,
}

impl Default for Inflate {
    /// Creates a new `Inflate` instance with a default buffer size and decompressor.
    fn default() -> Self {
        Self::new(usize::MAX)
    }
}

impl Inflate {
    fn new(max_decoded_message_size: usize) -> Self {
        Self {
            output: BytesMut::with_capacity(1024.min(max_decoded_message_size)),
            decompress: flate2::Decompress::new(false),
            max_decoded_message_size,
            decoded_message_size: 0,
            stream_ended: false,
        }
    }

    /// Configures a specific LZ77 window size for decompression.
    ///
    /// Available only when compiled with the `zlib` feature, this allows finer control over decompression by specifying the
    /// `window_bits` for the LZ77 sliding window.
    ///
    /// # Parameters
    /// - `window_bits`: The window size for LZ77, in bits.
    ///
    #[cfg(feature = "zlib")]
    fn with_window_bits(mut self, window_bits: u8) -> Self {
        self.decompress = flate2::Decompress::new_with_window_bits(false, window_bits);
        self
    }

    /// Resets the decompression dictionary, for no-context-takeover mode.
    fn reset(&mut self) {
        self.decompress.reset(false);
        self.decoded_message_size = 0;
        self.stream_ended = false;
    }

    /// Decompresses input data while maintaining decompression context across frames.
    fn decompress(&mut self, input: &[u8], stream_end: bool) -> io::Result<Bytes> {
        self.write(input)?;

        if stream_end {
            // Add the required 4-byte suffix as per RFC 7692, Section 7.2.2.
            // A peer that already closed the stream with a final block does not need it,
            // and feeding it would be rejected by the inflater anyway.
            if !self.stream_ended {
                self.write(&[0x0, 0x0, 0xff, 0xff])?;
            }

            let out = self.flush()?;

            // The stream is unusable once the peer sent a final block, so start fresh.
            // Such a peer is effectively running with no context takeover.
            if self.stream_ended {
                self.decompress.reset(false);
                self.stream_ended = false;
            }

            self.decoded_message_size = 0;

            Ok(out)
        } else {
            Ok(self.output.split().freeze())
        }
    }

    /// Writes compressed input data to the output buffer during decompression.
    fn write(&mut self, mut input: &[u8]) -> io::Result<()> {
        let output = &mut self.output;
        let decompressor = &mut self.decompress;
        let mut stream_ended = false;

        while !input.is_empty() {
            let dst = inflate_chunk(output);

            let before_out = decompressor.total_out();
            let before_in = decompressor.total_in();

            let status = decompressor.decompress(input, dst, flate2::FlushDecompress::None);

            let read = (decompressor.total_out() - before_out) as usize;
            let consumed = (decompressor.total_in() - before_in) as usize;

            unsafe { output.advance_mut(read) };
            self.decoded_message_size = self
                .decoded_message_size
                .checked_add(read)
                .ok_or_else(decompressed_message_too_large)?;
            if self.decoded_message_size > self.max_decoded_message_size {
                return Err(decompressed_message_too_large());
            }

            input = &input[consumed..];

            match status {
                // The peer closed the deflate stream with a final block. Nothing after it can
                // be consumed, so stop instead of looping forever over the leftover bytes.
                Ok(Status::StreamEnd) => {
                    stream_ended = true;
                    break;
                }
                Ok(Status::Ok | Status::BufError) => {}
                Err(..) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "corrupt deflate stream",
                    ))
                }
            }

            // Guard against any other state where the inflater makes no progress:
            // without this the loop would spin on the CPU until the process is killed.
            if consumed == 0 && read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "deflate stream made no progress",
                ));
            }
        }

        self.stream_ended |= stream_ended;

        Ok(())
    }

    /// Flushes the decompressed data to the output buffer.
    fn flush(&mut self) -> io::Result<Bytes> {
        let mut flush = flate2::FlushDecompress::Sync;

        loop {
            let dst = inflate_chunk(&mut self.output);

            let before_out = self.decompress.total_out();
            self.decompress
                .decompress(&[], dst, flush)
                .map_err(inflate_error)?;
            flush = flate2::FlushDecompress::None;

            let written = (self.decompress.total_out() - before_out) as usize;
            if written == 0 {
                return Ok(self.output.split().freeze());
            }

            unsafe { self.output.advance_mut(written) };
            self.decoded_message_size = self
                .decoded_message_size
                .checked_add(written)
                .ok_or_else(decompressed_message_too_large)?;
            if self.decoded_message_size > self.max_decoded_message_size {
                return Err(decompressed_message_too_large());
            }
        }
    }
}

#[derive(Debug)]
struct DecompressedMessageTooLarge;

impl std::fmt::Display for DecompressedMessageTooLarge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("decompressed message exceeds configured maximum")
    }
}

impl std::error::Error for DecompressedMessageTooLarge {}

fn decompressed_message_too_large() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, DecompressedMessageTooLarge)
}

pub(crate) fn is_decompressed_message_too_large(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<DecompressedMessageTooLarge>())
        .is_some()
}

/// Returns at most 1 KiB of writable spare capacity for decompression.
fn inflate_chunk(output: &mut BytesMut) -> &mut [u8] {
    if output.capacity() - output.len() < 1024 {
        output.reserve(1024);
    }

    let len = output.spare_capacity_mut().len().min(1024);
    let uninitbuf = &mut output.spare_capacity_mut()[..len];
    unsafe { &mut *(uninitbuf as *mut [std::mem::MaybeUninit<u8>] as *mut [u8]) }
}

#[cfg(test)]
mod tests {
    use flate2::Compression;

    use crate::compression::{
        CompressionConfig, Compressor, Decompressor, Deflate, HalfConfig, Inflate, WINDOW_BITS,
    };
    use crate::{CompressionLevel, Role, WebSocketError};

    use super::{is_decompressed_message_too_large, WebSocketExtensions};

    #[test]
    fn test_parse_extensions() {
        use std::str::FromStr;
        let compression = WebSocketExtensions::from_str("permessage-deflate; client_no_context_takeover; server_max_window_bits=7; client_max_window_bits=2; server_no_context_takeover").unwrap();
        assert!(compression.client_no_context_takeover);
        assert!(compression.server_no_context_takeover);
        assert_eq!(compression.server_max_window_bits, Some(Some(7)));
        assert_eq!(compression.client_max_window_bits, Some(Some(2)));
    }

    #[test]
    fn test_parse_extensions_client_max_window_bits_no_value() {
        use std::str::FromStr;
        let compression =
            WebSocketExtensions::from_str("permessage-deflate; client_max_window_bits").unwrap();
        assert_eq!(compression.client_max_window_bits, Some(None));
        assert!(!compression.client_no_context_takeover);
        assert!(!compression.server_no_context_takeover);
        assert_eq!(compression.server_max_window_bits, None);
    }

    #[test]
    fn test_parse_extensions_fail() {
        use std::str::FromStr;
        let res = WebSocketExtensions::from_str("foo, bar; baz=1");
        assert!(res.is_err());
        let res = WebSocketExtensions::from_str(
            "permessage-deflate; client_no_context_takeover server_max_window_bits=7",
        );
        assert!(res.is_err());
        let res = WebSocketExtensions::from_str(
            "permessage-deflate; server_max_window_bits=; client_no_context_takeover",
        );
        assert!(res.is_ok());
    }

    #[test]
    fn test_websocket_extensions_to_string() {
        let mut extensions = WebSocketExtensions {
            client_no_context_takeover: true,
            ..Default::default()
        };
        extensions.server_max_window_bits = Some(Some(15));
        let formatted = extensions.to_string();
        assert_eq!(
            formatted,
            "permessage-deflate; server_max_window_bits=15; client_no_context_takeover"
        );
    }

    #[cfg(feature = "zlib")]
    #[test]
    fn test_deflate_with_window_bits() {
        let deflate = Deflate::new_with_window_bits(Compression::default(), 15);
        assert_eq!(deflate.output.capacity(), 1024);
    }

    #[test]
    fn test_compress_no_context() {
        let mut deflate = Deflate::new(Compression::default());
        let data = b"test data";
        let compressed = deflate.compress(data, true).expect("Compression failed");
        deflate.reset();
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_compress_with_context() {
        let mut deflate = Deflate::new(Compression::default());
        let data = b"test data";
        let compressed = deflate.compress(data, true).expect("Compression failed");
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_inflate_default() {
        let inflate = Inflate::default();
        assert_eq!(inflate.output.capacity(), 1024);
    }

    #[cfg(feature = "zlib")]
    #[test]
    fn test_inflate_with_window_bits() {
        let inflate = Inflate::default().with_window_bits(15);
        assert_eq!(inflate.output.capacity(), 1024);
    }

    #[test]
    fn test_parse_sec_websocket_extensions_with_spaces() {
        use std::str::FromStr;
        let extensions =
            WebSocketExtensions::from_str("permessage-deflate ; server_no_context_takeover")
                .unwrap();
        assert!(extensions.server_no_context_takeover);
        assert!(!extensions.client_no_context_takeover);
        assert_eq!(extensions.server_max_window_bits, None);
        assert_eq!(extensions.client_max_window_bits, None);
    }

    #[test]
    fn test_parse_extensions_with_extra_spaces() {
        use std::str::FromStr;
        let extensions = WebSocketExtensions::from_str(
            "permessage-deflate  ; server_no_context_takeover  ;    server_max_window_bits  =    12",
        )
        .unwrap();
        assert!(extensions.server_no_context_takeover);
        assert!(!extensions.client_no_context_takeover);
        assert_eq!(extensions.server_max_window_bits, Some(Some(12)));
        assert_eq!(extensions.client_max_window_bits, None);
    }

    #[test]
    fn test_parser_robustness_with_unusual_spacing() {
        use std::str::FromStr;
        // Test with excessive spaces around semicolons and equals signs
        let extensions = WebSocketExtensions::from_str(
            "permessage-deflate    ;     client_no_context_takeover    ;    server_max_window_bits    =    10",
        )
        .unwrap();
        assert!(extensions.client_no_context_takeover);
        assert_eq!(extensions.server_max_window_bits, Some(Some(10)));
    }

    #[test]
    fn test_parser_with_mixed_spacing() {
        use std::str::FromStr;
        // Test with inconsistent spacing
        let extensions = WebSocketExtensions::from_str(
            "permessage-deflate;client_no_context_takeover ;server_max_window_bits=10; client_max_window_bits = 15",
        )
        .unwrap();
        assert!(extensions.client_no_context_takeover);
        assert_eq!(extensions.server_max_window_bits, Some(Some(10)));
        assert_eq!(extensions.client_max_window_bits, Some(Some(15)));
    }

    #[test]
    fn test_decompress_with_context() {
        let mut deflate = Deflate::new(Compression::default());
        let data = b"test data";
        let compressed = deflate.compress(data, true).expect("Compression failed");

        let mut inflate = Inflate::default();
        let decompressed = inflate
            .decompress(&compressed, true)
            .expect("Decompression failed");
        assert_eq!(decompressed.as_ref(), &data[..]);
    }

    #[test]
    fn test_decompress_no_context() {
        let mut deflate = Deflate::new(Compression::default());
        let data = b"test data";
        let compressed = deflate.compress(data, true).expect("Compression failed");
        deflate.reset();

        let mut inflate = Inflate::default();
        let decompressed = inflate
            .decompress(&compressed, true)
            .expect("Decompression failed");
        assert_eq!(decompressed.as_ref(), &data[..]);
    }

    #[test]
    fn decompression_accepts_the_exact_message_limit() {
        let data = vec![b'a'; 4096];
        let mut deflate = Deflate::new(Compression::default());
        let compressed = deflate.compress(&data, true).expect("compression failed");

        let mut inflate = Inflate::new(data.len());
        let decompressed = inflate
            .decompress(&compressed, true)
            .expect("an exact-size message should be accepted");

        assert_eq!(decompressed.as_ref(), data);
        assert_eq!(inflate.decoded_message_size, 0);
    }

    #[test]
    fn decompression_rejects_a_message_over_the_limit() {
        let data = vec![b'a'; 4097];
        let mut deflate = Deflate::new(Compression::default());
        let compressed = deflate.compress(&data, true).expect("compression failed");

        let mut inflate = Inflate::new(4096);
        let error = inflate
            .decompress(&compressed, true)
            .expect_err("an oversized decompressed message should be rejected");

        assert!(is_decompressed_message_too_large(&error));
        assert!(inflate.output.len() <= 4096 + 1024);
        assert!(inflate.decoded_message_size > 4096);
        assert!(inflate.decoded_message_size <= 4096 + 1024);
    }

    #[test]
    fn decompression_limit_is_cumulative_across_fragments() {
        let first = vec![b'a'; 2048];
        let second = vec![b'b'; 2048];
        let mut deflate = Deflate::new(Compression::default());
        let first_compressed = deflate.compress(&first, false).expect("compression failed");
        let second_compressed = deflate.compress(&second, true).expect("compression failed");

        let mut inflate = Inflate::new(3072);
        let first_decompressed = inflate
            .decompress(&first_compressed, false)
            .expect("the first fragment should fit");
        assert!(!first_decompressed.is_empty());
        assert_eq!(inflate.decoded_message_size, first_decompressed.len());
        assert!(inflate.decoded_message_size < 3072);

        let error = inflate
            .decompress(&second_compressed, true)
            .expect_err("the complete fragmented message should exceed the limit");
        assert!(is_decompressed_message_too_large(&error));
        assert!(inflate.decoded_message_size > 3072);
        assert!(inflate.decoded_message_size <= 3072 + 1024);
    }

    #[test]
    fn test_compressor_no_context_takeover() {
        let mut compressor = Compressor::new(
            Compression::default(),
            HalfConfig {
                no_context_takeover: true,
                window_bits: None,
            },
        );
        let data = b"sample data";
        let compressed = compressor.compress(data, true).expect("Compression failed");
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_decompressor_no_context_takeover() {
        let mut compressor = Compressor::new(
            Compression::default(),
            HalfConfig {
                no_context_takeover: true,
                window_bits: None,
            },
        );
        let data = b"sample data";
        let compressed = compressor.compress(data, true).expect("Compression failed");

        let mut decompressor = Decompressor::new(HalfConfig {
            no_context_takeover: true,
            window_bits: None,
        });
        let decompressed = decompressor
            .decompress(&compressed, true)
            .expect("Decompression failed");
        assert_eq!(decompressed.as_ref(), &data[..]);
    }

    #[test]
    fn test_large_data_compression_and_decompression() {
        let large_data = vec![1u8; 1024 * 1024]; // 1 MB of data
        let mut compressor = Compressor::new(Compression::default(), HalfConfig::default());
        let compressed = compressor
            .compress(&large_data, true)
            .expect("Compression failed");

        let mut decompressor = Decompressor::new(HalfConfig::default());
        let decompressed = decompressor
            .decompress(&compressed, true)
            .expect("Decompression failed");

        assert_eq!(&decompressed[..], &large_data[..]);
    }

    #[test]
    fn test_extensions_parsing_with_missing_values() {
        use std::str::FromStr;
        let extensions =
            WebSocketExtensions::from_str("permessage-deflate; server_max_window_bits=").unwrap();
        assert_eq!(extensions.server_max_window_bits, Some(None));
    }

    #[test]
    fn test_multiple_large_messages_compression_issue_reproduction() {
        // This test reproduces the issue from GitHub issue #7
        // where compression fails after 2-5 messages with long repeated data

        let csv_like_data = "timestamp,user_id,action,data,more_data,even_more_data,field1,field2,field3,field4,field5,field6,field7,field8,field9,field10"
            .repeat(100); // Create a long repeated string similar to CSV data

        let mut compressor = Compressor::new(Compression::default(), HalfConfig::default());
        let mut decompressor = Decompressor::new(HalfConfig::default());

        // Test multiple sequential compressions and decompressions
        for i in 1..=10 {
            println!("Processing message {i}");

            // Compress the data
            let compressed = compressor
                .compress(csv_like_data.as_bytes(), true)
                .unwrap_or_else(|_| panic!("Compression failed on message {i}"));

            println!(
                "Message {}: Original size: {}, Compressed size: {}",
                i,
                csv_like_data.len(),
                compressed.len()
            );

            // Decompress the data
            let decompressed = decompressor
                .decompress(&compressed, true)
                .unwrap_or_else(|_| panic!("Decompression failed on message {i}"));

            let decompressed_data = decompressed;
            assert_eq!(
                &decompressed_data[..],
                csv_like_data.as_bytes(),
                "Decompressed data doesn't match original on message {i}"
            );

            // If the issue reproduces, we should see errors after a few messages
            if i >= 2 {
                println!("Successfully processed {i} messages without compression errors");
            }
        }
    }

    fn compress_repetitive_csv_msg(n: usize) {
        // Test the same scenario but with no context takeover to compare
        let csv_like_data = "timestamp,user_id,action,data,more_data,even_more_data,field1,field2,field3,field4,field5,field6,field7,field8,field9,field10"
        .repeat(n);

        let mut compressor = Compressor::new(
            Compression::default(),
            HalfConfig {
                no_context_takeover: true,
                window_bits: None,
            },
        );
        let mut decompressor = Decompressor::new(HalfConfig {
            no_context_takeover: true,
            window_bits: None,
        });

        for i in 1..=10 {
            println!("Processing no-context message {i}");

            let compressed = compressor
                .compress(csv_like_data.as_bytes(), true)
                .unwrap_or_else(|_| panic!("No-context compression failed on message {i}"));

            let decompressed = decompressor
                .decompress(&compressed, true)
                .unwrap_or_else(|_| panic!("No-context decompression failed on message {i}"));

            let decompressed_data = decompressed;
            assert_eq!(
                std::str::from_utf8(&decompressed_data[..]).unwrap(),
                csv_like_data,
                "No-context decompressed data doesn't match original on message {i}"
            );
        }
    }

    #[test]
    fn test_no_context_takeover_multiple_messages() {
        compress_repetitive_csv_msg(100);
    }

    #[test]
    fn test_no_context_takeover_multiple_messages_large() {
        compress_repetitive_csv_msg(100_000);
    }

    #[test]
    fn test_detailed_compression_with_suffix_inspection() {
        // Test compression with detailed inspection of the compressed data
        // to understand how the suffix is being handled

        let csv_like_data = "timestamp,user_id,action,data,more_data,even_more_data,field1,field2,field3,field4,field5,field6,field7,field8,field9,field10"
            .repeat(50);

        let mut compressor = Compressor::new(Compression::default(), HalfConfig::default());
        let mut decompressor = Decompressor::new(HalfConfig::default());

        for i in 1..=5 {
            println!("=== Processing detailed message {i} ===");

            let compressed = compressor
                .compress(csv_like_data.as_bytes(), true)
                .unwrap_or_else(|_| panic!("Compression failed on message {i}"));

            println!("Message {}: Compressed size: {}", i, compressed.len());

            // Inspect the end of the compressed data
            let end_bytes = if compressed.len() >= 8 {
                &compressed[compressed.len() - 8..]
            } else {
                &compressed[..]
            };
            println!("Message {i}: End bytes: {end_bytes:02x?}");

            // Check if it ends with the deflate suffix
            let ends_with_suffix = compressed.ends_with(&[0x0, 0x0, 0xff, 0xff]);
            println!("Message {i}: Ends with suffix: {ends_with_suffix}");

            // Decompress the data
            let decompressed = decompressor
                .decompress(&compressed, true)
                .unwrap_or_else(|_| panic!("Decompression failed on message {i}"));

            let decompressed_data = decompressed;
            assert_eq!(
                &decompressed_data[..],
                csv_like_data.as_bytes(),
                "Decompressed data doesn't match original on message {i}"
            );

            println!("Message {i}: Successfully decompressed");
        }
    }

    #[test]
    fn test_random_data_compression_and_decompression() {
        // Generate pseudo-random data deterministically for repeatable tests
        let data_len = 10_000i32;
        let data: Vec<u8> = (0..data_len)
            .map(|i| ((i.wrapping_mul(1234567).wrapping_add(987654321)) % 256) as u8)
            .collect();

        // Compress the data
        let mut compressor = Compressor::new(Compression::default(), HalfConfig::default());
        let compressed = compressor
            .compress(&data, true)
            .expect("Compression failed");

        // Decompress the data
        let mut decompressor = Decompressor::new(HalfConfig::default());
        let decompressed = decompressor
            .decompress(&compressed, true)
            .expect("Decompression failed");

        // The decompression result should be Some(BytesMut) for a final frame
        assert_eq!(
            decompressed,
            &data[..],
            "Decompressed data does not match original"
        );
    }

    #[test]
    fn test_raw_deflate_compression_sequence() {
        // Test the raw deflate compression/decompression to see if we can reproduce the issue
        // This bypasses the WebSocket-specific compression wrapper

        let csv_like_data = "timestamp,user_id,action,data,more_data,even_more_data,field1,field2,field3,field4,field5,field6,field7,field8,field9,field10"
            .repeat(50);

        let mut deflate = Deflate::new(Compression::default());
        let mut inflate = Inflate::default();

        for i in 1..=5 {
            println!("=== Raw deflate message {i} ===");

            let compressed = deflate
                .compress(csv_like_data.as_bytes(), true)
                .unwrap_or_else(|_| panic!("Raw compression failed on message {i}"));

            println!("Raw message {}: Compressed size: {}", i, compressed.len());

            let decompressed = inflate
                .decompress(&compressed, true)
                .unwrap_or_else(|_| panic!("Raw decompression failed on message {i}"));

            let decompressed_data = decompressed;
            assert_eq!(
                &decompressed_data[..],
                csv_like_data.as_bytes(),
                "Raw decompressed data doesn't match original on message {i}"
            );

            println!("Raw message {i}: Successfully processed");
        }
    }

    #[test]
    fn test_github_issue_7_exact_reproduction() {
        // Test that exactly matches the pattern described in GitHub issue #7
        // Using the exact data pattern from the issue

        let data = "long repeated string of CSV-like data".repeat(500); // Make it very long

        let mut compressor = Compressor::new(Compression::default(), HalfConfig::default());
        let mut decompressor = Decompressor::new(HalfConfig::default());

        // The issue mentions it happens after 2-5 messages, so let's test exactly that range
        for i in 1..=7 {
            println!("GitHub issue reproduction - message {i}");

            let data_to_send = data.clone();

            let compressed = compressor
                .compress(data_to_send.as_bytes(), true)
                .unwrap_or_else(|_| panic!("GitHub issue: Compression failed on message {i}"));

            println!(
                "GitHub issue message {}: Original: {}, Compressed: {}",
                i,
                data_to_send.len(),
                compressed.len()
            );

            // Try to decompress - this is where the issue should manifest
            let decompressed = decompressor.decompress(&compressed, true);

            match decompressed {
                Ok(decompressed_data) => {
                    assert_eq!(
                        &decompressed_data[..],
                        data_to_send.as_bytes(),
                        "GitHub issue: Decompressed data doesn't match original on message {i}"
                    );
                    println!("GitHub issue message {i}: Successfully processed");
                }
                Err(e) => {
                    println!("GitHub issue: REPRODUCED! Decompression error on message {i}: {e}");
                    // This is what we expect to see if the issue reproduces
                    if (2..=5).contains(&i) {
                        println!("ERROR REPRODUCED: This matches the GitHub issue description!");
                        panic!("Successfully reproduced GitHub issue #7 on message {i}: {e}");
                    } else {
                        panic!("Unexpected error on message {i}: {e}");
                    }
                }
            }
        }

        // If we get here, the issue was not reproduced
        println!("GitHub issue #7 was NOT reproduced - all messages processed successfully");
    }

    #[test]
    fn test_extremely_repetitive_data() {
        // Test with extremely repetitive data that should compress very well
        // This might trigger edge cases in the compression algorithm

        let repetitive_data = "A".repeat(10000); // Very repetitive data

        let mut compressor = Compressor::new(Compression::default(), HalfConfig::default());
        let mut decompressor = Decompressor::new(HalfConfig::default());

        for i in 1..=8 {
            println!("Repetitive data test - message {i}");

            let compressed = compressor
                .compress(repetitive_data.as_bytes(), true)
                .map_err(|e| {
                    println!("Repetitive data: Compression error on message {i}: {e}");
                    e
                })
                .unwrap_or_else(|_| panic!("Repetitive data: Compression failed on message {i}"));

            println!(
                "Repetitive message {}: Original: {}, Compressed: {} (ratio: {:.2}%)",
                i,
                repetitive_data.len(),
                compressed.len(),
                (compressed.len() as f64 / repetitive_data.len() as f64) * 100.0
            );

            let decompressed = decompressor
                .decompress(&compressed, true)
                .map_err(|e| {
                    println!("Repetitive data: POTENTIAL ISSUE REPRODUCED! Decompression error on message {i}: {e}");
                    e
                })
                .unwrap_or_else(|_| panic!("Repetitive data: Decompression failed on message {i}"));

            let decompressed_data = decompressed;
            assert_eq!(
                &decompressed_data[..],
                repetitive_data.as_bytes(),
                "Repetitive data: Decompressed data doesn't match original on message {i}"
            );

            println!("Repetitive message {i}: Successfully processed");
        }
    }

    #[test]
    fn test_stress_compression_with_mixed_data() {
        // Stress test with mixed data patterns that might trigger edge cases
        let patterns = [
            "A".repeat(1000),
            "AB".repeat(500),
            "ABC".repeat(333),
            "Hello, World! ".repeat(100),
            (0u8..=255)
                .cycle()
                .take(1000)
                .map(|b| b as char)
                .collect::<String>(),
        ];

        let mut compressor = Compressor::new(Compression::default(), HalfConfig::default());
        let mut decompressor = Decompressor::new(HalfConfig::default());

        for (pattern_idx, pattern) in patterns.iter().enumerate() {
            for msg_idx in 1..=5 {
                println!(
                    "Stress test pattern {}, message {}",
                    pattern_idx + 1,
                    msg_idx
                );

                let compressed = compressor
                    .compress(pattern.as_bytes(), true)
                    .map_err(|e| {
                        println!(
                            "Stress test: Compression error on pattern {} message {}: {}",
                            pattern_idx + 1,
                            msg_idx,
                            e
                        );
                        e
                    })
                    .unwrap_or_else(|_| {
                        panic!(
                            "Stress test: Compression failed on pattern {} message {}",
                            pattern_idx + 1,
                            msg_idx
                        )
                    });

                let decompressed = decompressor
                    .decompress(&compressed, true)
                    .map_err(|e| {
                        println!("Stress test: POTENTIAL ISSUE! Decompression error on pattern {} message {}: {}",
                                pattern_idx + 1, msg_idx, e);
                        e
                    })
                    .unwrap_or_else(|_| panic!("Stress test: Decompression failed on pattern {} message {}",
                                   pattern_idx + 1, msg_idx));

                let decompressed_data = decompressed;
                assert_eq!(
                    &decompressed_data[..],
                    pattern.as_bytes(),
                    "Stress test: Data mismatch on pattern {} message {}",
                    pattern_idx + 1,
                    msg_idx
                );
            }
        }

        println!("Stress test completed successfully - no compression issues detected");
    }

    #[test]
    fn test_fragmented_compressed_frames() {
        // Test that fragmented frames (continuation frames) work correctly with compression
        // In WebSocket, a message can be split into multiple frames:
        // - First frame: FIN=0, contains partial data
        // - Continuation frames: FIN=0, contain more partial data
        // - Final frame: FIN=1, contains last part of data

        let test_data =
            "This is a test message that will be fragmented across multiple frames. ".repeat(100);
        let chunk_size = 500; // Split into chunks of 500 bytes

        let mut compressor = Compressor::new(Compression::default(), HalfConfig::default());
        let mut decompressor = Decompressor::new(HalfConfig::default());

        let mut compressed_fragments = Vec::new();
        let mut offset = 0;

        // Compress each fragment
        while offset < test_data.len() {
            let end = std::cmp::min(offset + chunk_size, test_data.len());
            let chunk = &test_data.as_bytes()[offset..end];
            let is_final = end == test_data.len();

            // Only flush on the final frame
            let compressed = compressor
                .compress(chunk, is_final)
                .expect("Fragmented compression failed");

            compressed_fragments.push((compressed, is_final));
            offset = end;
        }

        println!(
            "Created {} compressed fragments",
            compressed_fragments.len()
        );

        // Decompress each fragment
        let mut decompressed_data = Vec::new();
        for (idx, (compressed_chunk, is_final)) in compressed_fragments.iter().enumerate() {
            println!(
                "Decompressing fragment {} (final: {}, size: {})",
                idx,
                is_final,
                compressed_chunk.len()
            );

            let result = decompressor
                .decompress(compressed_chunk, *is_final)
                .expect("Fragmented decompression failed");

            decompressed_data.extend_from_slice(&result);

            // Only the final frame should return data
            if *is_final {
                assert!(
                    !result.is_empty(),
                    "Final frame should return decompressed data"
                );
            } else {
                assert!(!result.is_empty(), "Non-final frame should return None");
            }
        }

        // Verify the decompressed data matches the original
        assert_eq!(
            &decompressed_data[..],
            test_data.as_bytes(),
            "Fragmented decompressed data doesn't match original"
        );

        println!("Fragmented frame test passed - data integrity maintained");
    }

    #[test]
    fn test_fragmented_frames_with_context() {
        // Test fragmentation with context preservation across multiple messages
        let messages = [
            "First message with repetitive data: ".repeat(50),
            "Second message also repetitive: ".repeat(50),
            "Third message continues the pattern: ".repeat(50),
        ];

        let mut compressor = Compressor::new(Compression::default(), HalfConfig::default());
        let mut decompressor = Decompressor::new(HalfConfig::default());

        for (msg_idx, message) in messages.iter().enumerate() {
            println!("Processing fragmented message {}", msg_idx + 1);

            // Split each message into 3 fragments
            let chunk_size = message.len() / 3;
            let mut fragments = Vec::new();

            for i in 0..3 {
                let start = i * chunk_size;
                let end = if i == 2 {
                    message.len()
                } else {
                    (i + 1) * chunk_size
                };
                let chunk = &message.as_bytes()[start..end];
                let is_final = i == 2;

                let compressed = compressor.compress(chunk, is_final).unwrap_or_else(|_| {
                    panic!(
                        "Compression failed on message {} fragment {}",
                        msg_idx + 1,
                        i + 1
                    )
                });

                fragments.push((compressed, is_final));
            }

            // Decompress fragments
            let mut decompressed_data = Vec::new();
            for (frag_idx, (compressed, is_final)) in fragments.iter().enumerate() {
                let result = decompressor
                    .decompress(compressed, *is_final)
                    .unwrap_or_else(|_| {
                        panic!(
                            "Decompression failed on message {} fragment {}",
                            msg_idx + 1,
                            frag_idx + 1
                        )
                    });

                decompressed_data.extend_from_slice(&result);
            }

            assert_eq!(
                &decompressed_data[..],
                message.as_bytes(),
                "Message {} fragmented data doesn't match",
                msg_idx + 1
            );
        }

        println!("Fragmented frames with context test passed");
    }

    #[test]
    fn test_no_context_takeover_behavior() {
        // Test that verifies no_context_takeover properly resets compression state
        // between messages, ensuring consistent compression ratios unlike contextual compression

        let repetitive_message = "This is a repeated message. ".repeat(100);

        // Test with contextual compression (maintains state)
        let mut contextual_compressor =
            Compressor::new(Compression::default(), HalfConfig::default());
        let mut compressed_sizes_contextual = Vec::new();

        for i in 0..5 {
            let compressed = contextual_compressor
                .compress(repetitive_message.as_bytes(), true)
                .expect("Contextual compression failed");
            compressed_sizes_contextual.push(compressed.len());
            println!(
                "Contextual compression round {}: {} bytes",
                i + 1,
                compressed.len()
            );
        }

        // Test with no_context_takeover (resets state)
        let mut no_context_compressor = Compressor::new(
            Compression::default(),
            HalfConfig {
                no_context_takeover: true,
                window_bits: None,
            },
        );
        let mut compressed_sizes_no_context = Vec::new();

        for i in 0..5 {
            let compressed = no_context_compressor
                .compress(repetitive_message.as_bytes(), true)
                .expect("No-context compression failed");
            compressed_sizes_no_context.push(compressed.len());
            println!(
                "No-context compression round {}: {} bytes",
                i + 1,
                compressed.len()
            );
        }

        // With no_context_takeover, all compressed sizes should be identical
        // because the compression state is reset each time
        for i in 1..compressed_sizes_no_context.len() {
            assert_eq!(
                compressed_sizes_no_context[0],
                compressed_sizes_no_context[i],
                "No-context takeover should produce identical compression sizes, \
                 but round 1 had {} bytes while round {} had {} bytes",
                compressed_sizes_no_context[0],
                i + 1,
                compressed_sizes_no_context[i]
            );
        }

        // Test decompression works correctly with no_context_takeover
        let mut no_context_decompressor = Decompressor::new(HalfConfig {
            no_context_takeover: true,
            window_bits: None,
        });

        for i in 0..5 {
            let compressed = no_context_compressor
                .compress(repetitive_message.as_bytes(), true)
                .expect("Compression failed");

            let decompressed = no_context_decompressor
                .decompress(&compressed, true)
                .expect("Decompression failed");

            assert_eq!(
                &decompressed[..],
                repetitive_message.as_bytes(),
                "Decompressed data doesn't match original on round {}",
                i + 1
            );
        }

        println!("No-context takeover behavior test passed");
        println!(
            "No-context compression sizes: {:?}",
            compressed_sizes_no_context
        );
        println!(
            "Contextual compression sizes: {:?}",
            compressed_sizes_contextual
        );
    }

    /// A peer may finish its deflate stream with a final block (BFINAL=1) and append
    /// trailing bytes the inflater can never consume. The inflate loop used to spin on
    /// those bytes forever, pinning a core.
    ///
    /// See https://github.com/infinitefield/yawc/issues/40
    #[test]
    fn test_decompress_final_block_does_not_spin() {
        let mut encoder = flate2::Compress::new(Compression::default(), false);
        let data = b"the quick brown fox jumps over the lazy dog";

        let mut compressed = vec![0u8; 256];
        encoder
            .compress(data, &mut compressed, flate2::FlushCompress::Finish)
            .expect("compression failed");
        compressed.truncate(encoder.total_out() as usize);
        // Trailing bytes after the final block, as seen in the wild.
        compressed.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);

        let mut inflate = Inflate::default();
        let decompressed = inflate
            .decompress(&compressed, true)
            .expect("decompression failed");
        assert_eq!(decompressed.as_ref(), &data[..]);

        // The context is reset, so the next message decompresses too.
        let mut encoder = flate2::Compress::new(Compression::default(), false);
        let mut compressed = vec![0u8; 256];
        encoder
            .compress(data, &mut compressed, flate2::FlushCompress::Finish)
            .expect("compression failed");
        compressed.truncate(encoder.total_out() as usize);

        let decompressed = inflate
            .decompress(&compressed, true)
            .expect("decompression failed");
        assert_eq!(decompressed.as_ref(), &data[..]);
    }

    /// The compressed payload from the issue report: an OCPP client whose deflate
    /// stream ends with a final block plus four trailing bytes.
    const ISSUE_40_PAYLOAD: &[u8] = &[
        0x9d, 0x51, 0x3d, 0x6f, 0xc2, 0x30, 0x10, 0xfd, 0x2f, 0xb7, 0x36, 0x89, 0x9c, 0xef, 0x90,
        0x0d, 0x2a, 0x86, 0x0c, 0x2d, 0x12, 0x44, 0xed, 0x80, 0x3a, 0xb8, 0xc9, 0x25, 0x44, 0x10,
        0x9b, 0xda, 0x4e, 0x54, 0x8a, 0xf2, 0xdf, 0x7b, 0x86, 0x4a, 0x30, 0x74, 0x69, 0xe5, 0xe5,
        0xfc, 0xfc, 0xee, 0xbd, 0x77, 0xe7, 0x6d, 0xe0, 0x40, 0x18, 0x87, 0x49, 0xd4, 0x04, 0x89,
        0x5b, 0x21, 0xf7, 0xdd, 0x28, 0x41, 0xee, 0x72, 0xe4, 0xa9, 0x1b, 0xb1, 0xf7, 0x30, 0xce,
        0x66, 0x59, 0xd2, 0x64, 0x08, 0x0e, 0x94, 0x8a, 0x0b, 0xcd, 0x2b, 0xd3, 0x49, 0xb1, 0x1c,
        0x51, 0x18, 0x70, 0xce, 0x80, 0xb6, 0x28, 0x4f, 0x47, 0x84, 0x1c, 0x36, 0x86, 0x2b, 0x83,
        0x35, 0x31, 0x71, 0xd4, 0x04, 0x9c, 0xa1, 0xab, 0x21, 0xf7, 0x1d, 0xa8, 0xa4, 0x10, 0x58,
        0x19, 0xa9, 0x0a, 0x7b, 0x9f, 0x1c, 0x30, 0x5d, 0x8f, 0xda, 0xf0, 0xfe, 0x48, 0x5d, 0x01,
        0x23, 0x63, 0x96, 0xba, 0x41, 0x56, 0xfa, 0x71, 0x1e, 0xcf, 0x72, 0x96, 0x78, 0x8c, 0xb1,
        0x07, 0x96, 0xe5, 0x8c, 0x91, 0x96, 0x51, 0x5d, 0xdb, 0xa2, 0x5a, 0x23, 0xd7, 0x52, 0x10,
        0x7f, 0x3e, 0x98, 0x9d, 0x54, 0xdd, 0xd7, 0xc5, 0x48, 0xe3, 0xc7, 0xb3, 0x24, 0xcd, 0x24,
        0x9d, 0x39, 0xe4, 0x56, 0xca, 0x3d, 0x8a, 0xab, 0xf1, 0x4f, 0x09, 0x61, 0x18, 0x2e, 0x96,
        0x91, 0x3f, 0xb7, 0x4a, 0xd7, 0x98, 0xc5, 0x66, 0xe5, 0x47, 0x51, 0x14, 0x82, 0x0d, 0x72,
        0x1b, 0xa9, 0x10, 0x8d, 0xb4, 0xad, 0xf7, 0x10, 0xc5, 0x05, 0x3f, 0xb5, 0xc4, 0x1e, 0x0d,
        0xaa, 0x17, 0x7e, 0x18, 0x48, 0x61, 0x7b, 0xfe, 0xd3, 0x00, 0x9a, 0x68, 0x07, 0xac, 0x6f,
        0xcd, 0xe3, 0xb5, 0x62, 0x97, 0xc5, 0x18, 0xfc, 0x34, 0x24, 0x72, 0xb7, 0x5b, 0x6f, 0x81,
        0x6d, 0x27, 0xc0, 0x7a, 0x72, 0x3d, 0x10, 0x6e, 0x53, 0x2c, 0x05, 0xaa, 0xf6, 0xe4, 0xcd,
        0x89, 0x31, 0xa2, 0x57, 0xf4, 0x47, 0xa9, 0x8c, 0xb7, 0x26, 0xa2, 0xa6, 0x5c, 0xc4, 0x3d,
        0xc8, 0x8a, 0xdb, 0x66, 0xa2, 0x2e, 0x64, 0x7d, 0x22, 0x64, 0x10, 0x9d, 0x59, 0x35, 0x4f,
        0x17, 0x8d, 0xcb, 0x67, 0x58, 0x80, 0x9e, 0xf7, 0xaf, 0x3b, 0x98, 0x26, 0xe7, 0x7f, 0x31,
        0x36, 0xf2, 0xf1, 0x17, 0xb3, 0xe9, 0xcd, 0x9e, 0x6f, 0x0b, 0x5b, 0xb9, 0x68,
    ];

    /// Exact payload from the issue report: an OCPP client whose deflate stream ends with
    /// a final block plus four trailing bytes.
    #[test]
    fn test_decompress_issue_40_payload() {
        let mut inflate = Inflate::default();
        let decompressed = inflate
            .decompress(ISSUE_40_PAYLOAD, true)
            .expect("decompression failed");
        assert!(decompressed
            .starts_with(b"[2,\"35364f26-cea1-46ea-aea7-40b358986f8e\",\"TransactionEvent\""));
        assert_eq!(decompressed.len(), 586);
    }

    /// The no-context-takeover path shares `Inflate::write`, so it must survive a final
    /// block too. Its own reset is redundant here but harmless.
    #[test]
    fn test_decompress_no_context_final_block() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let finished = |data: &[u8]| {
            let mut encoder = flate2::Compress::new(Compression::default(), false);
            let mut out = vec![0u8; 256];
            encoder
                .compress(data, &mut out, flate2::FlushCompress::Finish)
                .expect("compression failed");
            out.truncate(encoder.total_out() as usize);
            out
        };

        let mut inflate = Inflate::default();
        for _ in 0..3 {
            let mut compressed = finished(data);
            compressed.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);

            let decompressed = inflate
                .decompress(&compressed, true)
                .expect("decompression failed");
            assert_eq!(decompressed.as_ref(), &data[..]);
            assert!(!inflate.stream_ended);
        }
    }

    /// A final block arriving on a non-final fragment leaves the flag set until the
    /// message completes, and must not spin on the remaining fragments either.
    #[test]
    fn test_decompress_final_block_mid_fragment() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let mut encoder = flate2::Compress::new(Compression::default(), false);
        let mut compressed = vec![0u8; 256];
        encoder
            .compress(data, &mut compressed, flate2::FlushCompress::Finish)
            .expect("compression failed");
        compressed.truncate(encoder.total_out() as usize);

        let mut inflate = Inflate::default();
        let first = inflate
            .decompress(&compressed, false)
            .expect("decompression failed");
        assert!(inflate.stream_ended);

        let last = inflate
            .decompress(&[0xde, 0xad], true)
            .expect("decompression failed");
        assert_eq!([first, last].concat(), &data[..]);
        assert!(!inflate.stream_ended);
    }

    /// Round trip at every negotiable window size. The existing window-bits tests only
    /// checked buffer capacity, so no data ever went through a non-default window.
    #[cfg(feature = "zlib")]
    #[test]
    fn test_window_bits_round_trip() {
        let messages: [&[u8]; 4] = [
            b"short",
            b"the quick brown fox jumps over the lazy dog",
            &[b'a'; 4096],
            b"{\"eventType\":\"Started\",\"evse\":{\"id\":1,\"connectorId\":1}}",
        ];

        for bits in 9..=15u8 {
            let mut deflate = Deflate::new_with_window_bits(Compression::default(), bits);
            let mut inflate = Inflate::default().with_window_bits(bits);

            for message in messages {
                let compressed = deflate.compress(message, true).expect("compression failed");
                let decompressed = inflate
                    .decompress(&compressed, true)
                    .expect("decompression failed");
                assert_eq!(
                    decompressed.as_ref(),
                    message,
                    "round trip failed at {bits} window bits"
                );
            }
        }
    }

    /// Same, with the context reset between messages.
    #[cfg(feature = "zlib")]
    #[test]
    fn test_window_bits_round_trip_no_context() {
        for bits in 9..=15u8 {
            let mut deflate = Deflate::new_with_window_bits(Compression::default(), bits);
            let mut inflate = Inflate::default().with_window_bits(bits);

            for i in 0..4 {
                let message = format!("message number {i} with some repeated repeated text");
                let compressed = deflate
                    .compress(message.as_bytes(), true)
                    .expect("compression failed");
                deflate.reset();
                let decompressed = inflate
                    .decompress(&compressed, true)
                    .expect("decompression failed");
                inflate.reset();
                assert_eq!(
                    decompressed.as_ref(),
                    message.as_bytes(),
                    "round trip failed at {bits} window bits"
                );
            }
        }
    }

    /// The reporter negotiated `client_max_window_bits=10`, so the inbound inflater used a
    /// 10-bit window. Decode their payload through that exact configuration.
    #[cfg(feature = "zlib")]
    #[test]
    fn test_decompress_issue_40_payload_window_bits_10() {
        let mut inflate = Inflate::default().with_window_bits(10);
        let decompressed = inflate
            .decompress(ISSUE_40_PAYLOAD, true)
            .expect("decompression failed");
        assert_eq!(decompressed.len(), 586);
    }

    /// A final block must not spin the inflate loop at a non-default window size either.
    #[cfg(feature = "zlib")]
    #[test]
    fn test_window_bits_final_block_does_not_spin() {
        for bits in 9..=15u8 {
            let mut encoder =
                flate2::Compress::new_with_window_bits(Compression::default(), false, bits);
            let data = b"the quick brown fox jumps over the lazy dog";

            let mut compressed = vec![0u8; 256];
            encoder
                .compress(data, &mut compressed, flate2::FlushCompress::Finish)
                .expect("compression failed");
            compressed.truncate(encoder.total_out() as usize);
            compressed.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);

            let mut inflate = Inflate::default().with_window_bits(bits);
            let decompressed = inflate
                .decompress(&compressed, true)
                .expect("decompression failed");
            assert_eq!(decompressed.as_ref(), &data[..]);
            assert!(!inflate.stream_ended);
        }
    }

    // ============================ Fuzzing ============================
    //
    // These run in the normal test suite with a fixed seed so failures reproduce.
    // The coverage-guided fuzz targets live in `fuzz/` and are run separately with
    // `cargo make fuzz` (see fuzz/README.md).

    /// Small xorshift PRNG. A seeded generator keeps these tests deterministic and
    /// avoids a dev-dependency just to produce bytes.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// Uniform enough for generating test inputs.
        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }

        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| self.next_u64() as u8).collect()
        }

        /// Payloads that stress the inflater differently: incompressible noise,
        /// highly repetitive runs, and text-like data.
        fn payload(&mut self, len: usize) -> Vec<u8> {
            match self.below(3) {
                0 => self.bytes(len),
                1 => {
                    let byte = self.next_u64() as u8;
                    vec![byte; len]
                }
                _ => {
                    let words = ["ok", "event", "id", "timestamp", "value", "{}", "\"a\""];
                    let mut out = Vec::with_capacity(len);
                    while out.len() < len {
                        out.extend_from_slice(words[self.below(words.len())].as_bytes());
                    }
                    out.truncate(len);
                    out
                }
            }
        }
    }

    /// Runs `case` for each iteration on a detached thread, failing if any single case
    /// stops making progress. A spinning inflate loop never yields, so it cannot be
    /// caught by an assertion inside the case itself.
    ///
    /// The last-started iteration is published so a failure names the exact case, which
    /// together with the fixed seed makes it reproducible.
    fn run_with_watchdog(
        name: &str,
        seed: u64,
        iterations: usize,
        case: impl Fn(&mut Rng, usize) + Send + 'static,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{mpsc, Arc};
        use std::time::Duration;

        let progress = Arc::new(AtomicUsize::new(0));
        let reported = Arc::clone(&progress);
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            let mut rng = Rng(seed);
            for i in 0..iterations {
                reported.store(i, Ordering::Relaxed);
                case(&mut rng, i);
            }
            let _ = tx.send(());
        });

        // The whole batch runs in a few seconds, so this only trips on a real hang.
        if rx.recv_timeout(Duration::from_secs(30)).is_err() {
            panic!(
                "{name}: iteration {} did not terminate (seed {seed}). \
                 Rerun with this seed to reproduce.",
                progress.load(Ordering::Relaxed)
            );
        }
    }

    /// Arbitrary bytes must never hang or panic the inflater: it either decompresses
    /// them or returns an error. This is the shape of the issue #40 bug, where a
    /// well-formed stream with unconsumable trailing bytes spun forever.
    #[test]
    fn test_fuzz_decompress_arbitrary_input() {
        run_with_watchdog(
            "decompress_arbitrary_input",
            0x5eed_1234,
            20_000,
            |rng, _| {
                let len = rng.below(512);
                let input = rng.bytes(len);

                let mut inflate = if rng.below(2) == 0 {
                    Inflate::default()
                } else {
                    #[cfg(feature = "zlib")]
                    {
                        Inflate::default().with_window_bits(9 + rng.below(7) as u8)
                    }
                    #[cfg(not(feature = "zlib"))]
                    {
                        Inflate::default()
                    }
                };

                let stream_end = rng.below(2) == 0;
                // Result is irrelevant, termination without panicking is the property.
                let _ = inflate.decompress(&input, stream_end);
            },
        );
    }

    /// Same, but starting from a well-formed deflate stream that is then corrupted,
    /// truncated, or extended. Random bytes rarely form a valid stream header, so this
    /// reaches decoder states the fully random test does not.
    #[test]
    fn test_fuzz_decompress_mutated_stream() {
        run_with_watchdog(
            "decompress_mutated_stream",
            0x5eed_abcd,
            20_000,
            |rng, _| {
                let len = 1 + rng.below(400);
                let data = rng.payload(len);

                let mut encoder = flate2::Compress::new(Compression::default(), false);
                let mut stream = vec![0u8; 2048];
                let flush = match rng.below(4) {
                    0 => flate2::FlushCompress::Sync,
                    1 => flate2::FlushCompress::Partial,
                    2 => flate2::FlushCompress::Full,
                    _ => flate2::FlushCompress::Finish,
                };
                if encoder.compress(&data, &mut stream, flush).is_err() {
                    return;
                }
                stream.truncate(encoder.total_out() as usize);

                match rng.below(4) {
                    // Flip a bit.
                    0 if !stream.is_empty() => {
                        let at = rng.below(stream.len());
                        stream[at] ^= 1 << rng.below(8);
                    }
                    // Truncate.
                    1 if !stream.is_empty() => {
                        let keep = rng.below(stream.len());
                        stream.truncate(keep);
                    }
                    // Append trailing bytes, the issue #40 shape.
                    2 => {
                        let extra_len = rng.below(8);
                        let extra = rng.bytes(extra_len);
                        stream.extend_from_slice(&extra);
                    }
                    _ => {}
                }

                let mut inflate = Inflate::default();

                // Deliver as one or more fragments, ending the message on the last.
                let fragments = 1 + rng.below(3);
                let mut offset = 0;
                for fragment in 0..fragments {
                    let last = fragment + 1 == fragments;
                    let end = if last {
                        stream.len()
                    } else {
                        offset + rng.below(stream.len() - offset + 1)
                    };
                    if inflate.decompress(&stream[offset..end], last).is_err() {
                        break;
                    }
                    offset = end;
                }
            },
        );
    }

    /// Round trip: whatever the compressor produces, the decompressor must reproduce
    /// exactly. Exercises both loops, including the compressor's flush loop.
    #[test]
    fn test_fuzz_round_trip() {
        run_with_watchdog("round_trip", 0x5eed_beef, 5_000, |rng, i| {
            let level = Compression::new(rng.below(10) as u32);
            let no_context = rng.below(2) == 0;

            let (mut deflate, mut inflate) = match rng.below(2) {
                #[cfg(feature = "zlib")]
                0 => {
                    let bits = 9 + rng.below(7) as u8;
                    (
                        Deflate::new_with_window_bits(level, bits),
                        Inflate::default().with_window_bits(bits),
                    )
                }
                _ => (Deflate::new(level), Inflate::default()),
            };

            // Several messages over one connection, so context takeover is exercised.
            for _ in 0..1 + rng.below(4) {
                let len = rng.below(2048);
                let data = rng.payload(len);

                let (compressed, decompressed) = if no_context {
                    let compressed = deflate.compress(&data, true).expect("compression failed");
                    deflate.reset();
                    let decompressed = inflate
                        .decompress(&compressed, true)
                        .expect("decompression failed");
                    inflate.reset();
                    (compressed, decompressed)
                } else {
                    let compressed = deflate.compress(&data, true).expect("compression failed");
                    let decompressed = inflate
                        .decompress(&compressed, true)
                        .expect("decompression failed");
                    (compressed, decompressed)
                };

                assert_eq!(
                    decompressed.as_ref(),
                    &data[..],
                    "round trip mismatch on iteration {i} ({} bytes in, {} compressed)",
                    data.len(),
                    compressed.len()
                );
            }
        });
    }

    /// Minimised by the `websocket_read` fuzz target from the issue #40 hang: a two-byte
    /// deflate payload holding nothing but an empty final block. Reaching the end of the
    /// stream with a single byte still pending was enough to spin the inflate loop.
    #[test]
    fn test_decompress_minimal_final_block() {
        let mut inflate = Inflate::default();
        let decompressed = inflate
            .decompress(&[0x03, 0x80], true)
            .expect("decompression failed");
        assert!(decompressed.is_empty());
        assert!(!inflate.stream_ended);
    }

    /// The role decides which negotiated half applies to which direction. Getting this
    /// backwards would still round trip against yawc itself, so it is pinned here.
    #[test]
    fn test_compression_config_maps_role_to_direction() {
        let extensions = WebSocketExtensions {
            server_max_window_bits: Some(Some(11)),
            client_max_window_bits: Some(Some(13)),
            server_no_context_takeover: true,
            client_no_context_takeover: false,
        };
        let level = Some(CompressionLevel::default());

        let client = CompressionConfig::resolve(Some(&extensions), level, Role::Client)
            .expect("resolve")
            .expect("negotiated");
        let server = CompressionConfig::resolve(Some(&extensions), level, Role::Server)
            .expect("resolve")
            .expect("negotiated");

        // A client writes with the client_* half and reads the server_* half.
        assert!(!client.outgoing.no_context_takeover);
        assert!(client.incoming.no_context_takeover);
        assert_eq!(client.outgoing.window_bits, Some(13));
        assert_eq!(client.incoming.window_bits, Some(11));

        // A server is the mirror image.
        assert_eq!(server.outgoing, client.incoming);
        assert_eq!(server.incoming, client.outgoing);
    }

    /// Window bits are clamped for every direction and role. Only one of the four
    /// original code paths did this, so a peer could push an unsupported size through
    /// the other three.
    #[test]
    fn test_compression_config_clamps_window_bits() {
        for bits in [0u8, 1, 7, 8, 16, 255] {
            let extensions = WebSocketExtensions {
                server_max_window_bits: Some(Some(bits)),
                client_max_window_bits: Some(Some(bits)),
                ..Default::default()
            };

            for role in [Role::Client, Role::Server] {
                let config = CompressionConfig::resolve(
                    Some(&extensions),
                    Some(CompressionLevel::default()),
                    role,
                )
                .expect("resolve")
                .expect("negotiated");

                for half in [config.outgoing, config.incoming] {
                    let window_bits = half.window_bits.expect("window bits");
                    assert!(
                        WINDOW_BITS.contains(&window_bits),
                        "{bits} was not clamped for {role}, got {window_bits}"
                    );
                }
            }
        }
    }

    /// A peer must not be able to turn compression on unilaterally. RFC 6455,
    /// Section 4.1 requires a client to fail when the response names an extension it
    /// never offered; this used to panic on an unwrap instead.
    #[test]
    fn test_compression_config_rejects_unoffered_extension() {
        let err =
            CompressionConfig::resolve(Some(&WebSocketExtensions::default()), None, Role::Client)
                .expect_err("an unoffered extension must be rejected");

        assert!(matches!(err, WebSocketError::CompressionNotSupported));
    }

    /// No extension negotiated means no compression, not an error.
    #[test]
    fn test_compression_config_absent_without_extension() {
        let config = CompressionConfig::resolve(None, None, Role::Client).expect("resolve");
        assert!(config.is_none());
    }
}
