// ============================================================================
// Serial Protocol Implementation
// Reverse-engineered from com.baiyi.service.serialservice.serialdataservice
// ============================================================================

use std::{fmt::{self, Write as _}, time::{SystemTime, UNIX_EPOCH}};
const FRAME_MARKER: u8 = 0x5A;
const ESCAPE_MARKER: u8 = 0x5B;
const CRLF: &str = "\r\n";

#[derive(Debug)]
pub enum ContentType {
    Json,
    // Binary,
    // Text,
}

impl ContentType {
    fn as_str(&self) -> &'static str {
        match self {
            ContentType::Json => "json",
        }
    }
}

/// Attempt #2 to fix build_message to make it more ergonomic
#[derive(Debug)]
pub struct CommandMessage<'a> {
    pub cmd_type: &'a str,
    pub seq_number: i64,
    pub ack_number: i64,
    pub content_type: ContentType,
    pub body: &'a str,
    pub date: i64,
    pub file_name: i64,
    pub file_size: i64,
    pub content_range: i64,
    pub counter: i64,
    pub msg_id: i64,
}

impl<'a> CommandMessage<'a> {
    pub fn new(cmd_type: &'a str, body: &'a str) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();

        let seq = (now % 100_000) as i64;
        let ts = now as i64;

        CommandMessage {
            cmd_type,
            seq_number: seq,
            ack_number: -1,
            content_type: ContentType::Json,
            body,
            date: ts,
            file_name: -1,
            file_size: -1,
            content_range: -1,
            counter: -1,
            msg_id: -1,
        }
    }

    fn write_header(
        out: &mut String,
        name: &str,
        value: impl fmt::Display,
    ) -> fmt::Result {
        write!(out, "{name}={value}{CRLF}")
    }

    /// Build message content in HTTP-like format:
    /// POST cmdType version\r\n
    /// Key=Value\r\n
    /// ...\r\n
    /// \r\n
    /// {json}
    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>, anyhow::Error> {
        // This feels disgusting // TODO: Please for the love of god, I need to find a better solution
        let mut msg = String::with_capacity(
            "POST  1\r\n\r\n".len() + self.cmd_type.len() + self.body.len() + 128,
        );

        // Request line
        write!(&mut msg, "POST {} 1{CRLF}", self.cmd_type)?;

        // Headers
        Self::write_header(&mut msg, "SeqNumber", self.seq_number)?;
        Self::write_header(&mut msg, "AckNumber", self.ack_number)?;
        Self::write_header(&mut msg, "ContentLength", self.body.len())?;
        Self::write_header(&mut msg, "ContentType", self.content_type.as_str())?;
        Self::write_header(&mut msg, "FileName", self.file_name)?;
        Self::write_header(&mut msg, "FileSize", self.file_size)?;
        Self::write_header(&mut msg, "ContentRange", self.content_range)?;
        Self::write_header(&mut msg, "Counter", self.counter)?;
        Self::write_header(&mut msg, "Date", self.date)?;
        Self::write_header(&mut msg, "msgId", self.msg_id)?;

        // Blank line + body
        msg.push_str(CRLF);
        msg.push_str(self.body);

        Ok(msg.into_bytes())
    }
}

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
