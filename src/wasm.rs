use bytes::Bytes;
use futures::{
    channel::mpsc::{channel, unbounded, Receiver, UnboundedReceiver, UnboundedSender},
    stream::StreamExt,
};
use std::{
    pin::Pin,
    task::{ready, Context, Poll},
};
use url::Url;
use wasm_bindgen::prelude::*;
use web_sys::MessageEvent;

use crate::{Result, WebSocketError};

/// A WebSocket wrapper for WASM applications that provides an async interface
/// for WebSocket communication. This implementation wraps the browser's native
/// WebSocket API and provides Rust-friendly methods for sending and receiving messages.
pub struct WebSocket {
    /// The underlying browser WebSocket instance
    stream: web_sys::WebSocket,
    /// Channel receiver for incoming messages and errors
    receiver: UnboundedReceiver<Result<String>>,
}

/// A struct representing a view over a WebSocket frame's payload.
///
/// This struct provides an immutable view of the data payload within a WebSocket frame,
/// storing the content as bytes that can be used for various message types.
pub struct FrameView {
    /// The actual data payload of the frame stored as immutable bytes
    pub payload: Bytes,
}

impl FrameView {
    /// Creates a new immutable text frame view with the given payload.
    /// The payload is converted to immutable `Bytes`.
    pub fn text(payload: impl Into<Bytes>) -> Self {
        Self {
            payload: payload.into(),
        }
    }

    /// Converts the frame's payload to a UTF-8 string slice.
    ///
    /// # Returns
    ///
    /// A reference to the payload as a UTF-8 string slice.
    ///
    /// # Panics
    ///
    /// This function will panic if the payload is not valid UTF-8.
    #[inline]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.payload).expect("utf8")
    }
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
    fn setup_close_handler(stream: &web_sys::WebSocket, tx: UnboundedSender<Result<String>>) {
        let onclose_callback: Closure<dyn Fn()> = Closure::new(move || {
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
    fn setup_message_handler(stream: &web_sys::WebSocket, tx: UnboundedSender<Result<String>>) {
        let onmessage_callback: Closure<dyn Fn(_)> = Closure::new(move |e: MessageEvent| {
            if let Ok(txt) = e.data().dyn_into::<js_sys::JsString>() {
                // ignore the error, it could be that the other end closed the
                // connection and we don't want to panic
                let _ = tx.unbounded_send(Ok(String::from(txt)));
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
        // Convert JsValue errors to anyhow errors
        self.stream
            .send_with_str(frame.as_str())
            .map_err(|_| WebSocketError::ConnectionClosed)
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
            Some(Ok(message)) => Poll::Ready(Some(Ok(FrameView::text(message)))),
            Some(Err(e)) => {
                if matches!(e, WebSocketError::ConnectionClosed) {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Err(e)))
                }
            }
            None => return Poll::Ready(None),
        }
    }
}
