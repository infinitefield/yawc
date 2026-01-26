use std::{
    collections::VecDeque,
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
};

use crate::{close::CloseCode, codec, Negotiation, OpCode, Result, Role};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, FramedParts};

use crate::{
    codec::Codec,
    compression::{Compressor, Decompressor},
    native::{ContextKind, WakeProxy},
    Frame, ReadHalf, WebSocketError, WriteHalf,
};

pub struct Streaming<S> {
    stream: Framed<S, Codec>,
    // Reading state
    read_half: ReadHalf,
    // Writing state
    write_half: WriteHalf,
    // waker proxy
    wake_proxy: Arc<WakeProxy>,
    // frames we must send (control..)
    obligated_sends: VecDeque<Frame>,
    // flag to indicate the writer to flush sends
    flush_sends: bool,
    // compressor
    deflate: Option<Compressor>,
    // decompressor
    inflate: Option<Decompressor>,
}

impl<S> Streaming<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(role: Role, stream: S, read_buf: Bytes, opts: &Negotiation) -> Self {
        let decoder = codec::Decoder::new(role, opts.max_payload_read);
        let encoder = codec::Encoder::new(role);
        let codec = Codec::from((decoder, encoder));

        let mut parts = FramedParts::new(stream, codec);
        parts.read_buf = read_buf.into();

        let mut framed = Framed::from_parts(parts);
        if let Some(boundary) = opts.max_backpressure_write_boundary {
            framed.set_backpressure_boundary(boundary);
        }

        Self {
            stream: framed,
            read_half: ReadHalf::new(),
            write_half: WriteHalf::new(),
            wake_proxy: Arc::new(WakeProxy::default()),
            obligated_sends: VecDeque::new(),
            flush_sends: false,
            deflate: opts.compressor(role),
            inflate: opts.decompressor(role),
        }
    }

    /// Splits the [`WebSocket`] into its low-level components for advanced usage.
    ///
    /// # Safety
    /// This function is unsafe because it splits ownership of shared state.
    pub unsafe fn split_stream(self) -> (Framed<S, Codec>, ReadHalf, WriteHalf) {
        (self.stream, self.read_half, self.write_half)
    }

    /// Polls for the next frame in the WebSocket stream.
    pub fn poll_next_frame(&mut self, cx: &mut Context<'_>) -> Poll<Result<Frame>> {
        let wake_proxy = Arc::clone(&self.wake_proxy);
        wake_proxy.set_waker(ContextKind::Read, cx.waker());

        loop {
            let res = wake_proxy.with_context(|cx| self.read_half.poll_frame(&mut self.stream, cx));
            match res {
                Poll::Ready(Ok(frame)) => match self.on_frame(frame)? {
                    Some(frame) => return Poll::Ready(Ok(frame)),
                    None => continue,
                },
                Poll::Ready(Err(WebSocketError::ConnectionClosed)) => {
                    ready!(wake_proxy.with_context(|cx| self.try_flush_obligated(cx)))?;
                    return Poll::Ready(Err(WebSocketError::ConnectionClosed));
                }
                Poll::Ready(Err(err)) => {
                    let code = match err {
                        WebSocketError::FrameTooLarge => CloseCode::Size,
                        WebSocketError::InvalidOpCode(_) => CloseCode::Unsupported,
                        WebSocketError::ReservedBitsNotZero
                        | WebSocketError::ControlFrameFragmented
                        | WebSocketError::PingFrameTooLarge
                        | WebSocketError::InvalidFragment
                        | WebSocketError::FragmentTimeout
                        | WebSocketError::InvalidContinuationFrame
                        | WebSocketError::CompressionNotSupported => CloseCode::Protocol,
                        _ => CloseCode::Error,
                    };
                    self.emit_close(Frame::close(code, err.to_string()));
                    return Poll::Ready(Err(err));
                }
                Poll::Pending => {
                    let res = ready!(wake_proxy.with_context(|cx| self.try_flush_obligated(cx)));
                    if let Err(err) = res {
                        return Poll::Ready(Err(err));
                    }
                    return Poll::Pending;
                }
            }
        }
    }

    fn on_frame(&mut self, mut frame: Frame) -> Result<Option<Frame>> {
        #[cfg(test)]
        println!(
            "<<Compression<< OpCode={:?} Fin={} Payload={}",
            frame.opcode,
            frame.fin,
            frame.payload.len()
        );

        // Handle protocol control frames first
        match frame.opcode {
            OpCode::Ping => {
                self.on_ping(&frame);
                return Ok(Some(frame));
            }
            OpCode::Close => {
                self.on_close(&frame)?;
                return Ok(Some(frame));
            }
            OpCode::Pong => return Ok(Some(frame)),
            _ => {}
        }

        if frame.is_compressed {
            if let Some(inflate) = self.inflate.as_mut() {
                // This payload could be empty, which is fine if we are dealing with fragmented frames.
                let payload = inflate.decompress(&frame.payload, frame.is_fin())?;
                // Remove the compression flag
                frame.is_compressed = false;
                frame.payload = payload;
            } else {
                return Err(WebSocketError::CompressionNotSupported);
            }
        }

        Ok(Some(frame))
    }

    fn on_ping(&mut self, frame: &Frame) {
        self.obligated_sends
            .push_back(Frame::pong(frame.payload.clone()));
    }

    fn on_close(&mut self, frame: &Frame) -> Result<()> {
        match frame.payload.len() {
            0 => {}
            1 => return Err(WebSocketError::InvalidCloseFrame),
            _ => {
                let code = frame.close_code().expect("close code");
                let _ = frame.close_reason()?;

                if !code.is_allowed() {
                    self.emit_close(Frame::close(CloseCode::Protocol, &frame.payload[2..]));
                    return Err(WebSocketError::InvalidCloseCode);
                }
            }
        }

        let frame = Frame::close_raw(frame.payload.clone());
        self.emit_close(frame);

        Ok(())
    }

    fn emit_close(&mut self, frame: Frame) {
        self.obligated_sends.push_back(frame);
        self.read_half.is_closed = true;
    }

    fn try_flush_obligated(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        while !self.obligated_sends.is_empty() {
            ready!(self.write_half.poll_ready(&mut self.stream, cx))?;

            let next = self.obligated_sends.pop_front().expect("obligated send");
            self.write_half.start_send(&mut self.stream, next)?;
            self.flush_sends = true;
        }

        if self.flush_sends {
            ready!(self.write_half.poll_flush(&mut self.stream, cx))?;
            self.flush_sends = false;
        }

        Poll::Ready(Ok(()))
    }
}

impl<S> futures::Stream for Streaming<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Item = Frame;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match ready!(this.poll_next_frame(cx)) {
            Ok(ok) => Poll::Ready(Some(ok)),
            Err(_) => Poll::Ready(None),
        }
    }
}

impl<S> futures::Sink<Frame> for Streaming<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Error = WebSocketError;

    fn poll_ready(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        let this = self.get_mut();
        let wake_proxy = Arc::clone(&this.wake_proxy);
        wake_proxy.set_waker(ContextKind::Write, cx.waker());
        wake_proxy.with_context(|cx| {
            ready!(this.try_flush_obligated(cx))?;
            this.write_half.poll_ready(&mut this.stream, cx)
        })
    }

    fn start_send(self: Pin<&mut Self>, mut item: Frame) -> std::result::Result<(), Self::Error> {
        let this = self.get_mut();

        #[cfg(test)]
        println!(
            ">>Compression>> OpCode={:?} Fin={} Payload={}",
            item.opcode,
            item.fin,
            item.payload.len()
        );

        let should_compress = !item.opcode.is_control();
        if should_compress {
            if let Some(deflate) = this.deflate.as_mut() {
                let output = deflate.compress(&item.payload, item.is_fin())?;
                // Set the RSV1 bit only when we are not streaming
                item.is_compressed = !this.write_half.streaming;
                item.payload = output;
            }
        }

        this.write_half.start_send(&mut this.stream, item)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        let this = self.get_mut();
        let wake_proxy = Arc::clone(&this.wake_proxy);
        wake_proxy.set_waker(ContextKind::Write, cx.waker());
        wake_proxy.with_context(|cx| this.write_half.poll_flush(&mut this.stream, cx))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        let this = self.get_mut();
        let wake_proxy = Arc::clone(&this.wake_proxy);
        this.wake_proxy.set_waker(ContextKind::Write, cx.waker());
        wake_proxy.with_context(|cx| this.write_half.poll_close(&mut this.stream, cx))
    }
}
