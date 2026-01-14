use futures::{
    channel::mpsc::{channel, unbounded, Receiver, UnboundedReceiver, UnboundedSender},
    stream::StreamExt,
};
use std::{
    pin::Pin,
    str::FromStr,
    task::{ready, Context, Poll},
};
use url::Url;
use wasm_bindgen::prelude::*;
use web_sys::MessageEvent;

use crate::{
    frame::{FrameView, OpCode},
    Result, WebSocketError,
};

/// A WebSocket wrapper for WASM applications that provides an async interface
/// for WebSocket communication. This implementation wraps the browser's native
/// WebSocket API and provides Rust-friendly methods for sending and receiving messages.
pub struct WebSocket {
    /// The underlying browser WebSocket instance
    stream: web_sys::WebSocket,
    /// Channel receiver for incoming messages and errors
    receiver: UnboundedReceiver<Result<FrameView>>,
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
        // Initialize the WebSocket connection
        let stream = web_sys::WebSocket::new(url.as_str()).map_err(WebSocketError::Js)?;
        // Set the binary type to be arraybuffers so that we can wrap them in `Bytes`
        stream.set_binary_type(web_sys::BinaryType::Arraybuffer);

        // Create a communication channel
        let (tx, rx) = unbounded();

        // Set up the event handlers
        Self::setup_message_handler(&stream, tx.clone());
        Self::setup_close_handler(&stream, tx);

        // Wait for the connection to open
        let mut open_future = Self::setup_open_handler(&stream);
        let _ = open_future.next().await;

        Ok(Self {
            stream,
            receiver: rx,
        })
    }

    /// Sets up the close handler for the WebSocket
    ///
    /// # Arguments
    ///
    /// * `stream` - Reference to the WebSocket instance
    /// * `tx` - Channel sender to forward close events
    fn setup_close_handler(stream: &web_sys::WebSocket, tx: UnboundedSender<Result<FrameView>>) {
        let onclose_callback: Closure<dyn Fn(web_sys::CloseEvent)> =
            Closure::new(move |close_event: web_sys::CloseEvent| {
                if !close_event.was_clean() {
                    web_sys::console::warn_1(
                        &js_sys::JsString::from_str("WebSocket CloseEvent wasClean() == false")
                            .unwrap(), // SAFETY: This always succeeds
                    );
                }
                let close_frame = FrameView::close(close_event.code().into(), close_event.reason());
                let _ = tx.unbounded_send(Ok(close_frame));
                let _ = tx.unbounded_send(Err(WebSocketError::ConnectionClosed));
            });

        stream.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        onclose_callback.forget();
    }

    /// Sets up the open handler for the WebSocket and returns a future that resolves
    /// when the connection is established
    ///
    /// # Arguments
    ///
    /// * `stream` - Reference to the WebSocket instance
    ///
    /// # Returns
    ///
    /// A future that resolves when the connection is opened
    fn setup_open_handler(stream: &web_sys::WebSocket) -> Receiver<()> {
        let (mut open_tx, open_rx) = channel(1);

        let onopen_callback = Closure::<dyn FnMut(_)>::new(move |_: MessageEvent| {
            let _ = open_tx.try_send(());
        });

        stream.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        onopen_callback.forget();

        open_rx
    }

    /// Sets up the message handler for the WebSocket
    ///
    /// # Arguments
    ///
    /// * `stream` - Reference to the WebSocket instance
    /// * `tx` - Channel sender to forward received messages
    fn setup_message_handler(stream: &web_sys::WebSocket, tx: UnboundedSender<Result<FrameView>>) {
        let onmessage_callback: Closure<dyn Fn(_)> = Closure::new(move |e: MessageEvent| {
            let data = e.data();
            let maybe_fv = if data.has_type::<js_sys::JsString>() {
                let str_value = data.unchecked_into::<js_sys::JsString>();
                Some(FrameView::text(String::from(str_value)))
            } else if data.has_type::<js_sys::ArrayBuffer>() {
                let buffer_value =
                    js_sys::Uint8Array::new(&data.unchecked_into::<js_sys::ArrayBuffer>()).to_vec();
                Some(FrameView::binary(buffer_value))
            } else {
                None
            };

            if let Some(fv) = maybe_fv {
                // ignore the error, it could be that the other end closed the
                // connection and we don't want to panic
                let _ = tx.unbounded_send(Ok(fv));
            }
        });

        stream.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
        onmessage_callback.forget();
    }

    /// Sends a JSON-serialized message over the WebSocket
    ///
    /// This method serializes the provided data structure into JSON and sends it
    /// as a text frame over the WebSocket connection.
    ///
    /// # Type Parameters
    ///
    /// * `T` - Any type that implements the `serde::Serialize` trait
    ///
    /// # Arguments
    ///
    /// * `data` - Reference to the data structure to serialize and send
    ///
    /// # Returns
    ///
    /// A Result indicating success or any serialization/transmission errors
    ///
    /// # Examples
    ///
    /// ```
    /// #[derive(serde::Serialize)]
    /// struct Message {
    ///     content: String,
    ///     timestamp: u64,
    /// }
    ///
    /// let msg = Message {
    ///     content: "Hello, WebSocket!".to_string(),
    ///     timestamp: 1625097600,
    /// };
    ///
    /// websocket.send_json(&msg).await?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if JSON serialization fails or if the WebSocket
    /// connection encounters an error during transmission.
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub async fn send_json<T: serde::Serialize>(&mut self, data: &T) -> Result<()> {
        let bytes = serde_json::to_vec(data)?;
        futures::SinkExt::send(self, FrameView::text(bytes)).await
    }

    /// Receive the next frame from the websocket
    ///
    /// This is an alias for the `next` method, providing a more semantically clear way
    /// to request the next frame from the WebSocket connection.
    ///
    /// # Returns
    ///
    /// A Result containing the received frame or an error
    pub async fn next_frame(&mut self) -> Result<FrameView> {
        use futures::StreamExt;
        match self.next().await {
            Some(res) => res,
            None => Err(WebSocketError::ConnectionClosed),
        }
    }
}

impl futures::Sink<FrameView> for WebSocket {
    type Error = WebSocketError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        // WebSocket's send is always ready in this implementation
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, frame: FrameView) -> Result<()> {
        match frame.opcode {
            OpCode::Text => self
                .stream
                .send_with_str(frame.as_str())
                .map_err(|_| WebSocketError::ConnectionClosed),
            OpCode::Binary => self
                .stream
                .send_with_js_u8_array(&js_sys::Uint8Array::from(&*frame.payload))
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
    type Item = Result<FrameView>;

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
    use crate::{frame::FrameView, wasm::WebSocket};
    use futures::SinkExt;

    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn can_connect_and_roundtrip_payloads() {
        const ECHO_URL: &str = "wss://echo.websocket.org";
        const TEXT_PAYLOAD: &str = "Hello Echo WebSocket in Text!";
        const BINARY_PAYLOAD: &[u8] = b"Hello Echo WebSocket in Binary!";

        let mut ws = WebSocket::connect(ECHO_URL.parse().unwrap()).await.unwrap();
        let _ = ws.next_frame().await.unwrap(); // Skip the "Request served by whatever"

        let text_fv = FrameView::text(TEXT_PAYLOAD);
        let binary_fv = FrameView::binary(BINARY_PAYLOAD);

        ws.send(text_fv.clone()).await.unwrap();
        assert_eq!(ws.next_frame().await.unwrap(), text_fv);

        ws.send(binary_fv.clone()).await.unwrap();
        assert_eq!(ws.next_frame().await.unwrap(), binary_fv);

        let close_fv = FrameView::close(crate::close::CloseCode::Normal, b"");
        ws.send(close_fv.clone()).await.unwrap();
        assert_eq!(ws.next_frame().await.unwrap(), close_fv);

        ws.close().await.unwrap();
    }
}
