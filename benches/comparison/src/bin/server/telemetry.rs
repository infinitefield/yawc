use anyhow::{ensure, Result};
use bytes::{Bytes, BytesMut};

#[derive(Default)]
pub struct TelemetryState {
    messages: u64,
    total: u64,
    reply_buffer: BytesMut,
}

impl TelemetryState {
    pub fn acknowledge_buffered(&mut self, payload: &[u8]) -> Result<Bytes> {
        let reply = self.acknowledge(payload)?;
        // The encoder drops each reply before the next feed, allowing buffer reuse.
        self.reply_buffer.extend_from_slice(&reply);
        Ok(self.reply_buffer.split().freeze())
    }

    // Shared with the allocation benchmark, which supports Rust 1.82.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    pub fn acknowledge(&mut self, payload: &[u8]) -> Result<[u8; 24]> {
        ensure!(payload.len() >= 12, "invalid telemetry batch");
        let readings = payload[8..].chunks_exact(4);
        ensure!(readings.remainder().is_empty(), "invalid telemetry batch");
        let sequence = u64::from_le_bytes(payload[..8].try_into()?);
        for reading in readings {
            self.total += u32::from_le_bytes(reading.try_into()?) as u64;
        }
        self.messages += 1;

        let mut reply = [0; 24];
        reply[..8].copy_from_slice(&sequence.to_le_bytes());
        reply[8..16].copy_from_slice(&self.total.to_le_bytes());
        reply[16..].copy_from_slice(&self.messages.to_le_bytes());
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tracks_readings_across_batches() {
        let mut state = super::TelemetryState::default();
        let mut request = Vec::new();
        request.extend_from_slice(&7_u64.to_le_bytes());
        request.extend_from_slice(&11_u32.to_le_bytes());
        request.extend_from_slice(&13_u32.to_le_bytes());

        let first = state.acknowledge(&request).expect("valid first batch");
        assert_eq!(
            u64::from_le_bytes(first[..8].try_into().expect("sequence is eight bytes")),
            7
        );
        assert_eq!(
            u64::from_le_bytes(first[8..16].try_into().expect("total is eight bytes")),
            24
        );
        assert_eq!(
            u64::from_le_bytes(first[16..].try_into().expect("count is eight bytes")),
            1
        );

        let second = state.acknowledge(&request).expect("valid second batch");
        assert_eq!(
            u64::from_le_bytes(second[8..16].try_into().expect("total is eight bytes")),
            48
        );
        assert_eq!(
            u64::from_le_bytes(second[16..].try_into().expect("count is eight bytes")),
            2
        );
    }

    #[test]
    fn rejects_incomplete_readings() {
        let mut state = super::TelemetryState::default();
        assert!(state.acknowledge(&[0; 11]).is_err());
        assert!(state.acknowledge(&[0; 13]).is_err());
    }
}
