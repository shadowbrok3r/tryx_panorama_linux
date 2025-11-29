// ============================================================================
// Serial Protocol Implementation
// Reverse-engineered from com.baiyi.service.serialservice.serialdataservice
// ============================================================================

use std::time::{SystemTime, UNIX_EPOCH};

const FRAME_MARKER: u8 = 0x5A;
const ESCAPE_MARKER: u8 = 0x5B;

/// Escape special bytes in the data
/// 0x5A -> 0x5B 0x01
/// 0x5B -> 0x5B 0x02
fn escape_data(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len() * 2);
    for &b in data {
        match b {
            0x5A => {
                result.push(ESCAPE_MARKER);
                result.push(0x01);
            }
            0x5B => {
                result.push(ESCAPE_MARKER);
                result.push(0x02);
            }
            _ => result.push(b),
        }
    }
    result
}

/// Calculate simple sum CRC (1 byte)
fn calc_crc(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

/// Frame builder
/// [0x5A][length:2bytes BE][escaped_message][CRC:1byte][0x5A]
fn build_frame(message: &[u8]) -> Vec<u8> {
    let escaped = escape_data(message);
    let length = escaped.len() as u16;

    let mut frame = Vec::with_capacity(escaped.len() + 5);
    frame.push(FRAME_MARKER); // Start marker
    frame.extend_from_slice(&length.to_be_bytes()); // 2-byte length, big-endian
    frame.extend_from_slice(&escaped); // Escaped message
    frame.push(calc_crc(&escaped)); // CRC of escaped data
    frame.push(FRAME_MARKER); // End marker

    frame
}

/// Build message content in HTTP-like format:
/// POST cmdType version\r\n
/// Key=Value\r\n
/// ...\r\n
/// \r\n
/// {json}
fn build_message(cmd_type: &str, json_content: &str) -> Vec<u8> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();

    let seq = (now % 100000) as i64;
    let ts = now as i64;

    let headers = format!(
        "SeqNumber={}\r\n\
         AckNumber=-1\r\n\
         ContentLength={}\r\n\
         ContentType=json\r\n\
         FileName=-1\r\n\
         FileSize=-1\r\n\
         ContentRange=-1\r\n\
         Counter=-1\r\n\
         Date={}\r\n\
         msgId=-1",
        seq,
        json_content.len(),
        ts
    );

    let message = format!("POST {} 1\r\n{}\r\n\r\n{}", cmd_type, headers, json_content);
    message.into_bytes()
}

pub fn send_command(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd_type: &str,
    json_value: &serde_json::Value,
) -> anyhow::Result<(), anyhow::Error> {
    let json_content = serde_json::to_string(json_value)?;
    let message = build_message(cmd_type, &json_content);
    let frame = build_frame(&message);

    log::info!(
        "Sending {} ({} bytes, frame: {} bytes)",
        cmd_type,
        json_content.len(),
        frame.len()
    );
    log::debug!(
        "Frame hex: {}...{}",
        hex_string(&frame[..30.min(frame.len())]),
        hex_string(&frame[frame.len().saturating_sub(10)..])
    );

    port.write_all(&frame)?;
    port.flush()?;

    Ok(())
}

fn hex_string(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}
