use futures::{
    channel::mpsc::{unbounded, UnboundedReceiver},
    stream::StreamExt,
};
use std::{
    pin::Pin,
    str::FromStr,
    task::{ready, Context, Poll},
};
use url::Url;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, Event, MessageEvent};

use crate::{
    frame::{Frame, OpCode},
    Result, WebSocketError,
};

/// Every handler is registered as an `Event` listener so they can all be stored together.
/// `MessageEvent` and `CloseEvent` derive from `Event`, so the handlers that need the
/// richer type cast back to it; the registration they are attached to guarantees the cast.
type EventClosure = Closure<dyn FnMut(Event)>;

/// The socket's event handlers, held only so they are freed along with it.
///
/// `Closure` is never `Send`, which would make the whole socket `!Send` on targets where
/// it used to be `Send`. Without `atomics` there is no shared memory and so only one
/// thread, the same reasoning wasm-bindgen uses for its own `Send` impl on `JsValue`. On
/// an `atomics` build these impls drop out, and the socket stays `!Send` regardless
/// because `stream` inherits that from `JsValue`.
struct Handlers(#[allow(dead_code)] [EventClosure; 4]);

#[cfg(not(target_feature = "atomics"))]
unsafe impl Send for Handlers {}
#[cfg(not(target_feature = "atomics"))]
unsafe impl Sync for Handlers {}

/// A WebSocket wrapper for WASM applications that provides an async interface
/// for WebSocket communication. This implementation wraps the browser's native
/// WebSocket API and provides Rust-friendly methods for sending and receiving messages.
///
/// Dropping the `WebSocket` unregisters the event handlers and closes the underlying
/// browser socket.
pub struct WebSocket {
    /// The underlying browser WebSocket instance
    stream: web_sys::WebSocket,
    /// Channel receiver for incoming messages and errors
    receiver: UnboundedReceiver<Result<Frame>>,
    /// Event handlers, kept alive for as long as the socket is and freed with it.
    _handlers: Handlers,
}

impl WebSocket {
    /// Creates a new WebSocket connection to the specified URL
    ///
    /// # Arguments
    ///
    /// * `url` - The WebSocket server URL, usually starts with "ws://" or "wss://"
    ///
    /// # Returns
    ///
    /// A Result containing the WebSocket instance if successful, or a JsValue error
    ///
    /// # Example
    ///
    /// ```
    /// let websocket = WebSocket::connect("wss://example.com/socket").await?;
    /// ```
    pub async fn connect(url: Url) -> Result<Self> {
        let (socket, mut outcome) = Self::open(&url)?;

        // `open`, `error` and `close` all feed `outcome`, and per WHATWG's "fail the
        // WebSocket connection" a failed handshake always fires `error` then `close`, so
        // one of the three is guaranteed to arrive.
        match outcome.next().await {
            Some(Ok(())) => Ok(socket),
            // Dropping `socket` unregisters the handlers and closes the browser socket.
            _ => Err(WebSocketError::ConnectionClosed),
        }
    }

    /// Creates the browser socket, registers every event handler on it, and returns the
    /// socket alongside the channel reporting how the handshake ended.
    ///
    /// Kept out of `connect` so the handler locals stay out of that future's layout.
    fn open(url: &Url) -> Result<(Self, UnboundedReceiver<Result<()>>)> {
        let stream = web_sys::WebSocket::new(url.as_str()).map_err(WebSocketError::Js)?;
        // Set the binary type to be arraybuffers so that we can wrap them in `Bytes`
        stream.set_binary_type(web_sys::BinaryType::Arraybuffer);

        // Frames and errors delivered to the caller once connected.
        let (tx, rx) = unbounded();
        // How the handshake ended. Cloneable senders let the three handlers share it
        // without wrapping a oneshot in an `Rc<RefCell<_>>`.
        let (outcome_tx, outcome_rx) = unbounded();

        let onopen = {
            let outcome_tx = outcome_tx.clone();
            EventClosure::new(move |_: Event| {
                let _ = outcome_tx.unbounded_send(Ok(()));
            })
        };

        let onerror = {
            let outcome_tx = outcome_tx.clone();
            // The event carries no detail by design, so it only has to unblock `connect`.
            EventClosure::new(move |_: Event| {
                let _ = outcome_tx.unbounded_send(Err(WebSocketError::ConnectionClosed));
            })
        };

        let onmessage = {
            let tx = tx.clone();
            EventClosure::new(move |event: Event| {
                let data = event.unchecked_into::<MessageEvent>().data();
                let maybe_fv = if data.has_type::<js_sys::JsString>() {
                    let str_value = data.unchecked_into::<js_sys::JsString>();
                    Some(Frame::text(String::from(str_value)))
                } else if data.has_type::<js_sys::ArrayBuffer>() {
                    let buffer_value =
                        js_sys::Uint8Array::new(&data.unchecked_into::<js_sys::ArrayBuffer>())
                            .to_vec();
                    Some(Frame::binary(buffer_value))
                } else {
                    None
                };

                if let Some(fv) = maybe_fv {
                    // ignore the error, it could be that the other end closed the
                    // connection and we don't want to panic
                    let _ = tx.unbounded_send(Ok(fv));
                }
            })
        };

        let onclose = EventClosure::new(move |event: Event| {
            let close_event = event.unchecked_into::<CloseEvent>();
            if !close_event.was_clean() {
                web_sys::console::warn_1(
                    &js_sys::JsString::from_str("WebSocket CloseEvent wasClean() == false")
                        .unwrap(), // SAFETY: This always succeeds
                );
            }
            let close_frame = Frame::close(close_event.code().into(), close_event.reason());
            let _ = tx.unbounded_send(Ok(close_frame));
            let _ = tx.unbounded_send(Err(WebSocketError::ConnectionClosed));
            // A close before the handshake completed is a failed connect.
            let _ = outcome_tx.unbounded_send(Err(WebSocketError::ConnectionClosed));
        });

        stream.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        stream.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        stream.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        stream.set_onclose(Some(onclose.as_ref().unchecked_ref()));

        let socket = Self {
            stream,
            receiver: rx,
            _handlers: Handlers([onopen, onerror, onmessage, onclose]),
        };

        Ok((socket, outcome_rx))
    }

    /// Receive the next frame from the websocket
    ///
    /// This is an alias for the `next` method, providing a more semantically clear way
    /// to request the next frame from the WebSocket connection.
    ///
    /// # Returns
    ///
    /// A Result containing the received frame or an error
    pub async fn next_frame(&mut self) -> Result<Frame> {
        use futures::StreamExt;
        match self.next().await {
            Some(res) => res,
            None => Err(WebSocketError::ConnectionClosed),
        }
    }
}

impl Drop for WebSocket {
    fn drop(&mut self) {
        // Unregister first: close() fires its event asynchronously, and by then the
        // handlers that would receive it are gone along with the channels they send to.
        self.stream.set_onopen(None);
        self.stream.set_onerror(None);
        self.stream.set_onmessage(None);
        self.stream.set_onclose(None);
        // A no-op on an already-closed socket, and aborts the handshake on a connecting
        // one, so no `ready_state` check is needed.
        let _ = self.stream.close();
    }
}

impl futures::Sink<Frame> for WebSocket {
    type Error = WebSocketError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        // WebSocket's send is always ready in this implementation
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, frame: Frame) -> Result<()> {
        match frame.opcode() {
            OpCode::Text => self
                .stream
                .send_with_str(frame.as_str())
                .map_err(|_| WebSocketError::ConnectionClosed),
            OpCode::Binary => self
                .stream
                .send_with_js_u8_array(&js_sys::Uint8Array::from(frame.payload().as_ref()))
                .map_err(|_| WebSocketError::ConnectionClosed),
            OpCode::Close => {
                let code = frame.close_code().ok_or(WebSocketError::ConnectionClosed)?;

                match frame.close_reason() {
                    Ok(Some(reason)) => self.stream.close_with_code_and_reason(code.into(), reason),
                    Ok(None) => self.stream.close_with_code(code.into()),
                    Err(err) => return Err(err),
                }
                .map_err(|_| WebSocketError::ConnectionClosed)
            }
            // All other types of payloads are taken care by the browser behind the scenes
            _ => Ok(()),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        // WebSocket sends immediately, no need for explicit flush
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        let ret = self.stream.close().map_err(WebSocketError::Js);
        Poll::Ready(ret)
    }
}

impl futures::Stream for WebSocket {
    type Item = Result<Frame>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Use the underlying receiver's poll_next and map the result
        match ready!(self.receiver.poll_next_unpin(cx)) {
            Some(Ok(message)) => Poll::Ready(Some(Ok(message))),
            Some(Err(e)) => {
                if matches!(e, WebSocketError::ConnectionClosed) {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Err(e)))
                }
            }
            None => Poll::Ready(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handshake that cannot succeed has to resolve, not hang. Before `onerror` and
    /// `onclose` fed the outcome channel, this future stayed `Pending` forever and the
    /// test would time out rather than fail.
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn connect_to_closed_port_fails() {
        let url = Url::parse("ws://127.0.0.1:1/").unwrap();
        assert!(WebSocket::connect(url).await.is_err());
    }
}
