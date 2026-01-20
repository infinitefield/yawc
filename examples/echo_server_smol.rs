use futures::{SinkExt, StreamExt};
use http_body_util::Empty;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use smol::net::TcpListener;
use yawc::{OpCode, WebSocket};

struct Adapter<S>(S);

impl<S> tokio::io::AsyncRead for Adapter<S>
where
    S: futures::AsyncRead + Unpin,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // Get the unfilled portion of the buffer as a mutable slice
        let unfilled = buf.initialize_unfilled();

        // Call the futures AsyncRead trait method
        match std::pin::Pin::new(&mut self.0).poll_read(cx, unfilled) {
            std::task::Poll::Ready(Ok(n)) => {
                // Advance the buffer by the number of bytes read
                buf.advance(n);
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl<S> tokio::io::AsyncWrite for Adapter<S>
where
    S: futures::AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_close(cx)
    }
}

async fn handle_websocket(req: Request<Incoming>) -> Result<Response<Empty<Bytes>>, hyper::Error> {
    let (response, upgrade_fut) = match WebSocket::upgrade(req) {
        Ok(result) => result,
        Err(e) => {
            log::error!("WebSocket upgrade failed: {e}");
            return Ok(Response::builder().status(400).body(Empty::new()).unwrap());
        }
    };

    // Spawn a task to handle the WebSocket connection
    smol::spawn(async move {
        match upgrade_fut.await {
            Ok(mut ws) => {
                while let Some(frame) = ws.next().await {
                    match frame.opcode() {
                        OpCode::Text | OpCode::Binary => {
                            if ws.send(frame).await.is_err() {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                log::error!("WebSocket upgrade error: {e}");
            }
        }
    })
    .detach();

    Ok(response)
}

fn main() {
    // Initialize logging
    simple_logger::init_with_level(log::Level::Debug).expect("Failed to initialize logger");

    smol::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:9002")
            .await
            .expect("Failed to bind to address");

        loop {
            let (stream, addr) = match listener.accept().await {
                Ok(result) => result,
                Err(e) => {
                    log::error!("Failed to accept connection: {e}");
                    continue;
                }
            };

            // Spawn a task to handle this connection
            smol::spawn(async move {
                // Wrap the smol stream with our adapter, then wrap with TokioIo for hyper
                let stream = TokioIo::new(Adapter(stream));

                if let Err(e) = http1::Builder::new()
                    .serve_connection(stream, service_fn(handle_websocket))
                    .with_upgrades()
                    .await
                {
                    log::error!("Error serving connection from {addr}: {e}");
                }
            })
            .detach();
        }
    });
}
