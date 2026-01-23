use anyhow::Result;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use yawc::{
    close::CloseCode, frame::OpCode, CompressionLevel, Frame, Options, TcpWebSocket, WebSocket,
};

async fn get_case_info(case: u32) -> Result<String> {
    let mut ws = WebSocket::connect(
        format!("ws://localhost:9001/getCaseInfo?case={case}")
            .parse()
            .unwrap(),
    )
    .await?;

    let msg = ws
        .next()
        .await
        .ok_or_else(|| anyhow::Error::msg("No response"))?;
    let json: Value = serde_json::from_slice(msg.payload())?;
    ws.send(Frame::close(CloseCode::Normal, [])).await?;

    // Extract the "id" field from the JSON
    let case_id = json["id"]
        .as_str()
        .ok_or_else(|| anyhow::Error::msg("Missing 'id' field in case info"))?
        .to_string();

    Ok(case_id)
}

fn get_options_for_case_id(case_id: &str) -> Options {
    let base_options = Options::default()
        .with_utf8()
        .with_max_payload_read(100 * 1024 * 1024)
        .with_max_read_buffer(200 * 1024 * 1024);

    if case_id.starts_with("12.") || case_id.starts_with("13.") {
        #[cfg(feature = "zlib")]
        if case_id.starts_with("13.") {
            if case_id.starts_with("13.3.") {
                return base_options.with_client_max_window_bits(9);
            } else if case_id.starts_with("13.4.") {
                return base_options.with_client_max_window_bits(15);
            } else if case_id.starts_with("13.5.") {
                return base_options
                    .client_no_context_takeover()
                    .with_client_max_window_bits(9);
            } else if case_id.starts_with("13.6.") {
                return base_options
                    .client_no_context_takeover()
                    .with_client_max_window_bits(15);
            }
        }

        // All other compression tests (13.x)
        return base_options
            .with_low_latency_compression()
            .with_compression_level(CompressionLevel::none())
            .client_no_context_takeover()
            .server_no_context_takeover();
    }

    // Non-compression tests
    base_options
}

async fn connect(path: &str, case_id: Option<&str>) -> Result<TcpWebSocket> {
    let options = case_id.map(get_options_for_case_id).unwrap_or_default();

    let client = WebSocket::connect(format!("ws://localhost:9001/{path}").parse().unwrap())
        .with_options(options)
        .await?;
    Ok(client)
}

async fn get_case_count() -> Result<u32> {
    let mut ws = connect("getCaseCount", None).await?;
    let msg = ws.next().await.ok_or_else(|| anyhow::Error::msg("idk"))?;
    ws.send(Frame::close(CloseCode::Normal, [])).await?;
    Ok(std::str::from_utf8(msg.payload())?.parse()?)
}

#[tokio::main]
async fn main() -> Result<()> {
    simple_logger::init_with_level(log::Level::Debug).expect("log");

    let count = get_case_count().await?;

    log::debug!("Loading info for {count} cases in parallel...");

    // Load all case info in parallel
    let case_info_futures: Vec<_> = (1..=count)
        .map(|case| async move {
            let result = get_case_info(case).await;
            (case, result)
        })
        .collect();

    let case_infos: Vec<_> = futures::future::join_all(case_info_futures).await;

    // Build a map of case number to case ID
    let mut case_id_map = HashMap::new();
    for (case, result) in case_infos {
        match result {
            Ok(id) => {
                case_id_map.insert(case, id);
            }
            Err(e) => {
                // log::warn!("Failed to get case info for case {}: {}", case, e);
                panic!("Failed to get case info for case {}: {}", case, e);
            }
        }
    }

    log::debug!("Running {count} cases sequentially");

    for case in 1..=count {
        if case % 10 == 0 {
            let mut ws = connect("updateReports?agent=websocket", None).await?;
            ws.send(Frame::close(CloseCode::Normal, [])).await?;
            ws.close().await?;
        }

        let case_id_str = case_id_map.get(&case).map(|s| s.as_str()).unwrap();
        log::debug!("Running case {case_id_str}");

        let mut ws = connect(
            &format!("runCase?case={case}&agent=yawc"),
            Some(case_id_str),
        )
        .await?;

        loop {
            let msg = match ws.next().await {
                Some(msg) => msg,
                None => break,
            };

            let (opcode, _is_fin, body) = msg.into_parts();
            match opcode {
                OpCode::Text | OpCode::Binary => {
                    ws.send(Frame::from((opcode, body))).await?;
                }
                _ => {}
            }
        }
    }

    let mut ws = connect("updateReports?agent=yawc", None).await?;
    ws.close().await?;

    Ok(())
}
