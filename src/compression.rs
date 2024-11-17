use std::io;

use bytes::{BufMut, BytesMut};
use flate2::{CompressError, DecompressError, Status};

use nom::{
    bytes::complete::tag,
    bytes::complete::take_while1,
    character::complete::digit1,
    character::complete::space0,
    combinator::opt,
    sequence::{pair, preceded},
    IResult,
};

use crate::{CompressionLevel, DeflateOptions};

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
    pub(super) server_max_window_bits: Option<u8>,
    pub(super) client_max_window_bits: Option<u8>,
    pub(super) server_no_context_takeover: bool,
    pub(super) client_no_context_takeover: bool,
}

impl<'a> From<&'a DeflateOptions> for WebSocketExtensions {
    /// Converts [`DeflateOptions`] into `WebSocketExtensions`, configuring the extensions
    /// for negotiation based on the specified compression settings.
    fn from(value: &'a DeflateOptions) -> Self {
        Self {
            #[cfg(feature = "zlib")]
            server_max_window_bits: value.server_max_window_bits,
            #[cfg(not(feature = "zlib"))]
            server_max_window_bits: None,
            #[cfg(feature = "zlib")]
            client_max_window_bits: value.client_max_window_bits,
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
        write!(f, "{}", PERMESSAGE_DEFLATE)?;

        if let Some(server_max_window_bits) = self.server_max_window_bits {
            if (9..16).contains(&server_max_window_bits) {
                write!(f, "; server_max_window_bits={}", server_max_window_bits)?;
            } else {
                write!(f, "; server_max_window_bits")?;
            }
        }
        if let Some(client_max_window_bits) = self.client_max_window_bits {
            if (9..16).contains(&client_max_window_bits) {
                write!(f, "; client_max_window_bits={}", client_max_window_bits)?;
            } else {
                write!(f, "; client_max_window_bits")?;
            }
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
                    if let Some(value) = value {
                        self.server_max_window_bits = Some(value.parse().unwrap_or_default())
                    } else {
                        self.server_max_window_bits = Some(0);
                    }
                }
                "client_max_window_bits" => {
                    if let Some(value) = value {
                        self.client_max_window_bits = Some(value.parse().unwrap_or_default())
                    } else {
                        self.client_max_window_bits = Some(0);
                    }
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
    /// the key and an optional value if it exists.
    ///
    /// # Parameters
    /// - `input`: A string containing a single extension parameter, prefixed with a semicolon (`;`).
    ///
    /// # Returns
    /// - `IResult<&str, (&str, Option<&str>)>`: The remaining input after the parsed key-value pair,
    ///   along with a tuple of the key and optional value.
    fn parse_extension(input: &str) -> IResult<&str, (&str, Option<&str>)> {
        // ; server_no_context_takeover
        preceded(
            tag(";"),
            preceded(
                space0,
                pair(
                    take_while1(|c: char| c.is_alphanumeric() || c == '_'),
                    opt(preceded(tag("="), digit1)),
                ),
            ),
        )(input)
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
/// A compressor for handling WebSocket payload compression, supporting both contextual and no-context-takeover modes.
///
/// `Compressor` is used to compress WebSocket message payloads, optimizing data transmission.
/// It provides flexibility with different configurations, such as specifying compression level,
/// window size (when using `zlib`), and no-context-takeover mode.
pub struct Compressor {
    compressor_type: CompressorType,
}

/// Enum representing different types of compression strategies:
/// - `Contextual`: Maintains compression context across frames.
/// - `NoContextTakeover`: Resets the compression dictionary after each frame, reducing memory usage at the cost of compression efficiency.
enum CompressorType {
    Contextual(Deflate),
    NoContextTakeover(Deflate),
}

impl Compressor {
    /// Creates a new compressor with the specified compression level.
    ///
    /// The compressor will maintain the compression context across frames, improving efficiency.
    ///
    /// # Parameters
    /// - `level`: The level of compression to be applied.
    ///
    /// # Returns
    /// A `Compressor` instance in contextual mode.
    pub fn new(level: CompressionLevel) -> Self {
        Self {
            compressor_type: CompressorType::Contextual(Deflate::new(level)),
        }
    }

    /// Creates a new compressor with a specific window size for LZ77, available when `zlib` is enabled.
    ///
    /// # Parameters
    /// - `level`: The level of compression.
    /// - `window_bits`: The number of bits for the LZ77 compression window.
    ///
    /// # Returns
    /// A `Compressor` instance configured with the specified window size.
    #[cfg(feature = "zlib")]
    pub fn new_with_window_bits(level: CompressionLevel, window_bits: u8) -> Self {
        Self {
            compressor_type: CompressorType::Contextual(Deflate::new_with_window_bits(
                level,
                window_bits,
            )),
        }
    }

    /// Creates a new compressor in no-context-takeover mode.
    ///
    /// In no-context-takeover mode, the compressor resets its dictionary after each frame,
    /// lowering memory usage at the cost of compression efficiency.
    ///
    /// # Parameters
    /// - `level`: The level of compression.
    ///
    /// # Returns
    /// A `Compressor` instance in no-context-takeover mode.
    pub fn no_context_takeover(level: CompressionLevel) -> Self {
        Self {
            compressor_type: CompressorType::NoContextTakeover(Deflate::new(level)),
        }
    }

    /// Compresses the given input data and returns the compressed output.
    ///
    /// # Parameters
    /// - `input`: The data slice to compress.
    ///
    /// # Returns
    /// A `BytesMut` containing the compressed data, or an `io::Error` if compression fails.
    pub fn compress(&mut self, input: &[u8]) -> io::Result<BytesMut> {
        match &mut self.compressor_type {
            CompressorType::Contextual(compressor) => compressor.compress(input),
            CompressorType::NoContextTakeover(compressor) => compressor.compress_no_context(input),
        }
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
    ///
    /// # Parameters
    /// - `level`: The level of compression to apply, using the [`CompressionLevel`] type.
    ///
    /// # Returns
    /// A new `Deflate` instance with the specified compression level.
    fn new(level: CompressionLevel) -> Self {
        Self {
            output: BytesMut::with_capacity(1024),
            compress: flate2::Compress::new(level, false),
        }
    }

    /// Creates a new `Deflate` compressor with a specific compression level and window size for LZ77.
    ///
    /// Available only when compiled with the `zlib` feature, this allows finer control over compression by specifying the
    /// `window_bits` for the LZ77 sliding window.
    ///
    /// # Parameters
    /// - `level`: The level of compression to apply.
    /// - `window_bits`: The window size for LZ77, in bits.
    ///
    /// # Returns
    /// A `Deflate` instance configured with the specified window size.
    #[cfg(feature = "zlib")]
    fn new_with_window_bits(level: CompressionLevel, window_bits: u8) -> Self {
        Self {
            output: BytesMut::with_capacity(1024),
            compress: flate2::Compress::new_with_window_bits(level, false, window_bits),
        }
    }

    /// Compresses input data with no context takeover, resetting the compression dictionary before each compression.
    ///
    /// # Parameters
    /// - `input`: The data slice to compress.
    ///
    /// # Returns
    /// A `BytesMut` containing the compressed data, or an `io::Error` if compression fails.
    fn compress_no_context(&mut self, input: &[u8]) -> io::Result<BytesMut> {
        self.compress.reset(); // Reset dictionary for no-context takeover
        self.compress(input)
    }

    /// Compresses input data while maintaining compression context across frames.
    ///
    /// # Parameters
    /// - `input`: The data slice to compress.
    ///
    /// # Returns
    /// A `BytesMut` containing the compressed data, or an `io::Error` if compression fails.
    fn compress(&mut self, mut input: &[u8]) -> io::Result<BytesMut> {
        while !input.is_empty() {
            let consumed = self.write(input)?;
            input = &input[consumed..];
        }
        self.flush()
    }

    /// Writes a chunk of data to the output buffer during compression.
    ///
    /// # Parameters
    /// - `input`: The data slice to write to the compressor.
    ///
    /// # Returns
    /// The number of bytes consumed from `input`, or an `io::Error` if the compression fails.
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let output = &mut self.output;
        let compressor = &mut self.compress;

        let dst = chunk(output);

        let before_out = compressor.total_out();
        let before_in = compressor.total_in();

        let status = compressor.compress(input, dst, flate2::FlushCompress::None);

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

    /// Flushes the current compression state and returns the compressed output buffer.
    ///
    /// This method performs several steps:
    /// 1. Flushes any remaining compressed data using `FlushCompress::Sync`
    /// 2. Continues flushing until no more output is produced
    /// 3. Removes unnecessary trailing suffix bytes if present
    /// 4. Returns the final compressed output
    ///
    /// # Returns
    /// - `Ok(BytesMut)`: The compressed output buffer containing all flushed data
    /// - `Err(io::Error)`: If an error occurs during compression
    fn flush(&mut self) -> io::Result<BytesMut> {
        let output = &mut self.output;
        let compressor = &mut self.compress;

        let dst = chunk(output);
        let before_out = compressor.total_out();

        compressor
            .compress(&[], dst, flate2::FlushCompress::Sync)
            .map_err(deflate_error)?;

        let written = (compressor.total_out() - before_out) as usize;
        unsafe { output.advance_mut(written) };

        loop {
            let dst = chunk(output);

            let before_out = compressor.total_out();
            compressor
                .compress(&[], dst, flate2::FlushCompress::None)
                .map_err(deflate_error)?;

            if before_out == compressor.total_out() {
                // Strip unnecessary suffix if present
                if output.ends_with(&[0x0, 0x0, 0xff, 0xff]) {
                    output.truncate(output.len() - 4);
                }

                break Ok(output.split());
            }

            let written = (compressor.total_out() - before_out) as usize;
            unsafe { output.advance_mut(written) };
        }
    }
}

/// ignore the mapping input and print out a specific error.
fn deflate_error(err: CompressError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("Compression error: {}", err),
    )
}

fn inflate_error(err: DecompressError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("Decompression error: {}", err),
    )
}

/// Returns a mutable slice to the next available chunk of memory in the BytesMut buffer.
///
/// This function manages the buffer capacity and provides safe access to uninitialized memory:
///
/// * If the buffer is full (len == capacity), reserves an additional 1024 bytes
/// * Gets a reference to the uninitialized spare capacity
/// * Performs an unsafe conversion of MaybeUninit<u8> to u8 for the raw bytes
///
/// # Safety
/// The unsafe conversion is necessary to treat uninitialized memory as initialized bytes,
/// which is valid in this context since the bytes will be written before being read.
///
/// # Arguments
/// * `output` - Mutable reference to the BytesMut buffer
///
/// # Returns
/// A mutable slice of u8 representing the next available chunk of memory
fn chunk(output: &mut BytesMut) -> &mut [u8] {
    if output.len() == output.capacity() {
        // allocate capacity ourselves since chunk_mut reserves only 64 bytes
        output.reserve(1024);
    }

    let uninitbuf = output.spare_capacity_mut();
    unsafe { &mut *(uninitbuf as *mut [std::mem::MaybeUninit<u8>] as *mut [u8]) }
}

/// A decompressor for handling WebSocket payload decompression.
///
/// The `Decompressor` type provides functionality for decompressing WebSocket messages that were
/// compressed using the permessage-deflate extension. It supports two modes of operation:
///
/// - Contextual mode: The decompression context (dictionary) is maintained across multiple frames,
///   providing better compression ratios for related data.
///
/// - No-context-takeover mode: The decompression context is reset after each frame, trading
///   compression efficiency for reduced memory usage.
///
pub struct Decompressor {
    decompressor_type: DecompressorType,
}

impl Default for Decompressor {
    /// Creates a new decompressor in contextual mode by default.
    ///
    /// This is equivalent to calling [`Decompressor::new()`].
    ///
    /// # Returns
    ///
    /// Returns a new `Decompressor` instance that maintains compression context across frames.
    fn default() -> Self {
        Self {
            decompressor_type: DecompressorType::Contextual(Default::default()),
        }
    }
}

/// The type of decompression strategy to use.
///
/// This enum determines how the decompressor handles the compression dictionary between frames:
/// - `Contextual`: Maintains the dictionary across multiple frames for better compression
/// - `NoContextTakeover`: Resets the dictionary after each frame to reduce memory usage
enum DecompressorType {
    /// Retains decompression context across frames for better compression ratios.
    Contextual(Inflate),
    /// Resets decompression context after each frame to reduce memory usage.
    NoContextTakeover(Inflate),
}

impl Decompressor {
    /// Creates a new `Decompressor` in contextual mode.
    ///
    /// The created decompressor will maintain its internal dictionary state between frames,
    /// potentially providing better compression ratios for related data across frames.
    ///
    /// # Returns
    ///
    /// Returns a new `Decompressor` instance that uses contextual decompression.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new `Decompressor` with specific LZ77 window bits.
    ///
    /// This constructor allows fine-tuning of the decompression window size when using
    /// the `zlib` feature. A larger window size generally provides better compression
    /// but requires more memory.
    ///
    /// # Parameters
    ///
    /// * `window_bits` - Number of bits to use for the LZ77 sliding window (9-15)
    ///
    /// # Returns
    ///
    /// Returns a new `Decompressor` configured with the specified window size.
    ///
    /// # Features
    ///
    /// This function is only available when compiled with the `zlib` feature enabled.
    #[cfg(feature = "zlib")]
    pub fn new_with_window_bits(window_bits: u8) -> Self {
        Self {
            decompressor_type: DecompressorType::Contextual(Inflate::new_with_window_bits(
                window_bits,
            )),
        }
    }

    /// Creates a new `Decompressor` in no-context-takeover mode.
    ///
    /// In no-context-takeover mode, the decompression dictionary is reset after processing
    /// each frame. This reduces memory usage at the cost of potentially lower compression
    /// ratios, since each frame is decompressed independently.
    ///
    /// # Returns
    ///
    /// Returns a new `Decompressor` instance that resets its context after each frame.
    pub fn no_context_takeover() -> Self {
        Self {
            decompressor_type: DecompressorType::NoContextTakeover(Default::default()),
        }
    }

    /// Decompresses a compressed data frame.
    ///
    /// This method decompresses the provided input data according to the configured mode
    /// (contextual or no-context-takeover). When `stream_end` is true, this indicates the
    /// final frame in a message, which triggers special handling required by the WebSocket
    /// permessage-deflate extension.
    ///
    /// # Parameters
    ///
    /// * `input` - The compressed data bytes to decompress
    /// * `stream_end` - Boolean flag indicating if this is the final frame in a message
    ///
    /// # Returns
    ///
    /// * `Ok(Some(BytesMut))` - Successfully decompressed data
    /// * `Ok(None)` - More input needed to complete decompression
    /// * `Err(io::Error)` - Decompression failed due to invalid/corrupt data
    ///
    pub fn decompress(&mut self, input: &[u8], stream_end: bool) -> io::Result<Option<BytesMut>> {
        match &mut self.decompressor_type {
            DecompressorType::Contextual(decompressor) => {
                decompressor.decompress(input, stream_end)
            }
            DecompressorType::NoContextTakeover(decompressor) => {
                decompressor.decompress_no_context(input, stream_end)
            }
        }
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
}

impl Default for Inflate {
    /// Creates a new `Inflate` instance with a default buffer size and decompressor.
    fn default() -> Self {
        Self {
            output: BytesMut::with_capacity(1024),
            decompress: flate2::Decompress::new(false),
        }
    }
}

impl Inflate {
    /// Creates a new `Inflate` instance with a specific LZ77 window size for decompression.
    ///
    /// Available only when compiled with the `zlib` feature, this allows finer control over decompression by specifying the
    /// `window_bits` for the LZ77 sliding window.
    ///
    /// # Parameters
    /// - `window_bits`: The window size for LZ77, in bits.
    ///
    /// # Returns
    /// A `Inflate` instance configured with the specified window size.
    #[cfg(feature = "zlib")]
    fn new_with_window_bits(window_bits: u8) -> Self {
        Self {
            output: BytesMut::with_capacity(1024),
            decompress: flate2::Decompress::new_with_window_bits(false, window_bits),
        }
    }

    /// Decompresses input data in no-context-takeover mode, resetting the decompression context before each call.
    ///
    /// # Parameters
    /// - `input`: The compressed data to decompress.
    /// - `stream_end`: Indicates whether this is the final frame, signaling the end of the decompression stream.
    ///
    /// # Returns
    /// - `Ok(Some(BytesMut))`: The decompressed data, or `None` if more input is needed.
    /// - `Err(io::Error)`: If decompression fails.
    fn decompress_no_context(
        &mut self,
        input: &[u8],
        stream_end: bool,
    ) -> io::Result<Option<BytesMut>> {
        self.decompress.reset(false); // Reset the context for no-context takeover
        self.decompress(input, stream_end)
    }

    /// Decompresses input data while maintaining decompression context across frames.
    ///
    /// # Parameters
    /// - `input`: The compressed data to decompress.
    /// - `stream_end`: Indicates whether this is the final frame in a message, signaling the end of the decompression stream.
    ///
    /// # Returns
    /// - `Ok(Some(BytesMut))`: The decompressed data if available, or `None` if more input is needed.
    /// - `Err(io::Error)`: If decompression fails.
    fn decompress(&mut self, input: &[u8], stream_end: bool) -> io::Result<Option<BytesMut>> {
        self.write(input)?;

        if stream_end {
            // Add the required 4-byte suffix as per RFC 7692, Section 7.2.2
            self.write(&[0x0, 0x0, 0xff, 0xff])?;
            self.flush().map(Some)
        } else {
            Ok(None)
        }
    }

    /// Writes compressed input data to the output buffer during decompression.
    ///
    /// # Parameters
    /// - `input`: The compressed data slice to write to the decompressor.
    ///
    /// # Returns
    /// - `Ok(())` if successful.
    /// - `Err(io::Error)` if an error occurs during decompression.
    fn write(&mut self, mut input: &[u8]) -> io::Result<()> {
        let output = &mut self.output;
        let decompressor = &mut self.decompress;

        while !input.is_empty() {
            let dst = chunk(output);

            let before_out = decompressor.total_out();
            let before_in = decompressor.total_in();

            let status = decompressor.decompress(input, dst, flate2::FlushDecompress::None);

            let read = (decompressor.total_out() - before_out) as usize;
            let consumed = (decompressor.total_in() - before_in) as usize;

            unsafe { output.advance_mut(read) };

            input = &input[consumed..];

            match status {
                Ok(Status::Ok | Status::BufError | Status::StreamEnd) => {}
                Err(..) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "corrupt deflate stream",
                    ))
                }
            }
        }

        Ok(())
    }

    /// Flushes the decompressed data to the output buffer.
    ///
    /// This method ensures all data is written to the output buffer and strips any unnecessary suffix bytes.
    ///
    /// # Returns
    /// - `Ok(BytesMut)`: The flushed output data.
    /// - `Err(io::Error)`: If an error occurs during flushing.
    fn flush(&mut self) -> io::Result<BytesMut> {
        let output = &mut self.output;
        let decompressor = &mut self.decompress;

        let dst = chunk(output);
        let before_out = decompressor.total_out();

        decompressor
            .decompress(&[], dst, flate2::FlushDecompress::Sync)
            .map_err(inflate_error)?;

        let written = (decompressor.total_out() - before_out) as usize;
        unsafe { output.advance_mut(written) };

        loop {
            let dst = chunk(output);

            let before_out = decompressor.total_out();
            decompressor
                .decompress(&[], dst, flate2::FlushDecompress::None)
                .map_err(inflate_error)?;

            if before_out == decompressor.total_out() {
                break Ok(output.split());
            }

            let written = (decompressor.total_out() - before_out) as usize;
            unsafe {
                output.advance_mut(written);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use flate2::Compression;

    use crate::compression::{Compressor, Decompressor, Deflate, Inflate};

    use super::WebSocketExtensions;

    #[test]
    fn test_parse_extensions() {
        use std::str::FromStr;
        let compression = WebSocketExtensions::from_str("permessage-deflate; client_no_context_takeover; server_max_window_bits=7; client_max_window_bits=2; server_no_context_takeover").unwrap();
        assert!(compression.client_no_context_takeover);
        assert!(compression.server_no_context_takeover);
        assert_eq!(compression.server_max_window_bits, Some(7));
        assert_eq!(compression.client_max_window_bits, Some(2));
    }

    #[test]
    fn test_parse_extensions_fail() {
        use std::str::FromStr;
        let res = WebSocketExtensions::from_str("permessage-deflate; client_no_context_takeover; server_max_window_bits=7; client_max_window_bits=");
        assert!(res.is_err());
        let res = WebSocketExtensions::from_str("foo, bar; baz=1");
        assert!(res.is_err());
        let res = WebSocketExtensions::from_str(
            "permessage-deflate; client_no_context_takeover server_max_window_bits=7",
        );
        assert!(res.is_err());
        let res = WebSocketExtensions::from_str(
            "permessage-deflate; server_max_window_bits=; client_no_context_takeover",
        );
        assert!(res.is_err());
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
        let compressed = deflate
            .compress_no_context(data)
            .expect("Compression failed");
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_compress_with_context() {
        let mut deflate = Deflate::new(Compression::default());
        let data = b"test data";
        let compressed = deflate.compress(data).expect("Compression failed");
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
        let inflate = Inflate::new_with_window_bits(15);
        assert_eq!(inflate.output.capacity(), 1024);
    }

    #[test]
    fn test_decompress_with_context() {
        let mut deflate = Deflate::new(Compression::default());
        let data = b"test data";
        let compressed = deflate.compress(data).expect("Compression failed");

        let mut inflate = Inflate::default();
        let decompressed = inflate
            .decompress(&compressed, true)
            .expect("Decompression failed");
        assert_eq!(decompressed.as_ref().unwrap(), &data[..]);
    }

    #[test]
    fn test_decompress_no_context() {
        let mut deflate = Deflate::new(Compression::default());
        let data = b"test data";
        let compressed = deflate
            .compress_no_context(data)
            .expect("Compression failed");

        let mut inflate = Inflate::default();
        let decompressed = inflate
            .decompress_no_context(&compressed, true)
            .expect("Decompression failed");
        assert_eq!(decompressed.as_ref().unwrap(), &data[..]);
    }

    #[test]
    fn test_compressor_no_context_takeover() {
        let mut compressor = Compressor::no_context_takeover(Compression::default());
        let data = b"sample data";
        let compressed = compressor.compress(data).expect("Compression failed");
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_decompressor_no_context_takeover() {
        let mut compressor = Compressor::no_context_takeover(Compression::default());
        let data = b"sample data";
        let compressed = compressor.compress(data).expect("Compression failed");

        let mut decompressor = Decompressor::no_context_takeover();
        let decompressed = decompressor
            .decompress(&compressed, true)
            .expect("Decompression failed");
        assert_eq!(decompressed.as_ref().unwrap(), &data[..]);
    }
}
