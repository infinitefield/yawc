use anyhow::Result;
use futures::{SinkExt, StreamExt};
use yawc::{close::CloseCode, frame::OpCode, CompressionLevel, FrameView, Options, WebSocket};

async fn connect(path: &str) -> Result<WebSocket> {
    let client = WebSocket::connect(format!("ws://localhost:9001/{path}").parse().unwrap())
        .with_options(
            Options::default()
                .with_compression_level(CompressionLevel::none())
                .with_utf8()
                .with_max_payload_read(100 * 1024 * 1024)
                .with_max_read_buffer(200 * 1024 * 1024)
                .client_no_context_takeover()
                .server_no_context_takeover(),
        )
        .await?;
    Ok(client)
}

async fn get_case_count() -> Result<u32> {
    let mut ws = connect("getCaseCount").await?;
    let msg = ws.next().await.ok_or_else(|| anyhow::Error::msg("idk"))?;
    ws.send(FrameView::close(CloseCode::Normal, [])).await?;
    Ok(std::str::from_utf8(&msg.payload)?.parse()?)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    simple_logger::init_with_level(log::Level::Debug).expect("log");

    let count = get_case_count().await?;

    log::debug!("Running {count} cases");

    for case in 1..=count {
        log::debug!("Running {case}");

        if case % 10 == 0 {
            let mut ws = connect("updateReports?agent=websocket").await?;
            ws.send(FrameView::close(CloseCode::Normal, [])).await?;
            ws.close().await?;
        }

        let mut ws = connect(&format!("runCase?case={case}&agent=yawc")).await?;
        loop {
            let msg = match ws.next().await {
                Some(msg) => msg,
                None => break,
            };

            match msg.opcode {
                OpCode::Text | OpCode::Binary => {
                    ws.send(FrameView::from((msg.opcode, msg.payload))).await?;
                }
                _ => {}
            }
        }
    }

    let mut ws = connect("updateReports?agent=yawc").await?;
    ws.close().await?;

    Ok(())
}
