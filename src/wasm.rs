use bytes::Bytes;
use futures::channel::mpsc::{channel, Receiver, Sender};
use futures::stream::StreamExt;
use url::Url;
use wasm_bindgen::prelude::*;
// use wasm_bindgen_futures::spawn_local;
use web_sys::{ErrorEvent, MessageEvent};

/// A WebSocket wrapper for WASM applications that provides an async interface
/// for WebSocket communication. This implementation wraps the browser's native
/// WebSocket API and provides Rust-friendly methods for sending and receiving messages.
pub struct WebSocket {
    /// The underlying browser WebSocket instance
    stream: web_sys::WebSocket,
    /// Channel receiver for incoming messages and errors
    receiver: Receiver<anyhow::Result<String>>,
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
    pub async fn connect(url: Url) -> anyhow::Result<Self, JsValue> {
        // Initialize the WebSocket connection
        let stream = web_sys::WebSocket::new(url.as_str())?;

        // Create a communication channel
        let (tx, rx) = channel(1024);

        // Set up the event handlers
        Self::setup_message_handler(&stream, tx.clone());
        Self::setup_error_handler(&stream, tx);

        // Wait for the connection to open
        let mut open_future = Self::setup_open_handler(&stream);
        let _ = open_future.next().await;

        Ok(Self {
            stream,
            receiver: rx,
        })
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
    fn setup_message_handler(stream: &web_sys::WebSocket, mut tx: Sender<anyhow::Result<String>>) {
        let onmessage_callback = Closure::new(move |e: MessageEvent| {
            if let Ok(txt) = e.data().dyn_into::<js_sys::JsString>() {
                tx.try_send(Ok(String::from(txt))).expect("try send");
            }
        });

        stream.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
        onmessage_callback.forget();
    }

    /// Sets up the error handler for the WebSocket
    ///
    /// # Arguments
    ///
    /// * `stream` - Reference to the WebSocket instance
    /// * `tx` - Channel sender to forward error messages
    fn setup_error_handler(stream: &web_sys::WebSocket, mut tx: Sender<anyhow::Result<String>>) {
        let onerror_callback = Closure::new(move |e: ErrorEvent| {
            let err = anyhow::anyhow!("{}", e.message());
            tx.try_send(Err(err)).expect("send");
        });

        stream.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
        onerror_callback.forget();
    }

    /// Send a string message over the websocket
    ///
    /// # Arguments
    ///
    /// * `message` - The string message to send
    ///
    /// # Returns
    ///
    /// Result indicating success or failure
    pub async fn send(&self, frame: FrameView) -> anyhow::Result<(), JsValue> {
        let data = frame.as_str();
        self.stream.send_with_str(data)?;
        Ok(())
    }

    /// Receive the next frame from the websocket
    ///
    /// This is an alias for the `next` method, providing a more semantically clear way
    /// to request the next frame from the WebSocket connection.
    ///
    /// # Returns
    ///
    /// A Result containing the received frame or an error
    pub async fn next_frame(&mut self) -> anyhow::Result<FrameView> {
        self.next().await
    }

    /// Receive the next message from the websocket
    ///
    /// # Returns
    ///
    /// A Result containing the received message string or an error
    pub async fn next(&mut self) -> anyhow::Result<FrameView> {
        match self.receiver.next().await {
            Some(Ok(message)) => Ok(FrameView::text(message)),
            Some(Err(e)) => Err(anyhow::anyhow!("WebSocket error: {}", e)),
            None => Err(anyhow::anyhow!("WebSocket channel closed")),
        }
    }

    /// Close the websocket connection
    ///
    /// # Returns
    ///
    /// Result indicating success or failure
    pub fn close(&self) -> anyhow::Result<(), JsValue> {
        self.stream.close()
    }
}
