use futures::channel::mpsc::{Receiver, Sender};
use futures::stream::StreamExt;
use wasm_bindgen::prelude::*;
use web_sys::{ErrorEvent, MessageEvent};

/// A WebSocket wrapper for WASM applications that provides an async interface
/// for WebSocket communication. This implementation wraps the browser's native
/// WebSocket API and provides Rust-friendly methods for sending and receiving messages.
pub struct WebSocket {
    /// The underlying browser WebSocket instance
    stream: web_sys::WebSocket,
    /// Channel receiver for incoming messages and errors
    receiver: Receiver<Result<String, String>>,
    /// Channel sender for outgoing messages
    sender: Sender<Result<String, String>>,
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
    pub async fn connect(url: Url) -> Result<Self, JsValue> {
        // Initialize the WebSocket connection
        let stream = web_sys::WebSocket::new(url.as_str())?;

        // Create a communication channel
        let (tx, rx) = futures::channel::mpsc::unbounded();

        // Set up the event handlers
        Self::setup_message_handler(&stream, tx.clone());
        Self::setup_error_handler(&stream, tx.clone());

        // Wait for the connection to open
        let open_future = Self::setup_open_handler(&stream);
        open_future.await;

        Ok(Self {
            stream,
            receiver: rx,
            sender: tx,
        })
    }

    /// Sets up the message handler for the WebSocket
    ///
    /// # Arguments
    ///
    /// * `stream` - Reference to the WebSocket instance
    /// * `tx` - Channel sender to forward received messages
    fn setup_message_handler(stream: &web_sys::WebSocket, tx: Sender<Result<String, String>>) {
        let message_sender = tx;

        let onmessage_callback = Closure::wrap(Box::new(move |e: MessageEvent| {
            if let Ok(txt) = e.data().dyn_into::<js_sys::JsString>() {
                let _ = message_sender.unbounded_send(Ok(String::from(txt)));
            }
        }) as Box<dyn FnMut(MessageEvent)>);

        stream.set_onmessage(Some(onmessage_callback.into_js_value()));
    }

    /// Sets up the error handler for the WebSocket
    ///
    /// # Arguments
    ///
    /// * `stream` - Reference to the WebSocket instance
    /// * `tx` - Channel sender to forward error messages
    fn setup_error_handler(stream: &web_sys::WebSocket, tx: Sender<Result<String, String>>) {
        let error_sender = tx;

        let onerror_callback = Closure::wrap(Box::new(move |e: ErrorEvent| {
            let _ = error_sender.unbounded_send(Err(e.message()));
        }) as Box<dyn FnMut(ErrorEvent)>);

        stream.set_onerror(Some(onerror_callback.into_js_value()));
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
    fn setup_open_handler(stream: &web_sys::WebSocket) -> futures::channel::oneshot::Receiver<()> {
        let (open_tx, open_rx) = futures::channel::oneshot::channel();

        let onopen_callback = Closure::wrap(Box::new(move |_| {
            let _ = open_tx.send(());
        }) as Box<dyn FnMut(JsValue)>);

        stream.set_onopen(Some(onopen_callback.into_js_value()));

        open_rx
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
    pub async fn send(&self, message: &str) -> anyhow::Result<()> {
        self.stream.send_with_str(message)?;
        Ok(())
    }

    /// Receive the next message from the websocket
    ///
    /// # Returns
    ///
    /// A Result containing the received message string or an error
    pub async fn recv(&mut self) -> anyhow::Result<String> {
        match self.receiver.next().await {
            Some(Ok(message)) => Ok(message),
            Some(Err(e)) => Err(anyhow::anyhow!("WebSocket error: {}", e)),
            None => Err(anyhow::anyhow!("WebSocket channel closed")),
        }
    }

    /// Close the websocket connection
    ///
    /// # Returns
    ///
    /// Result indicating success or failure
    pub fn close(&self) -> anyhow::Result<()> {
        self.stream.close()?;
        Ok(())
    }
}
