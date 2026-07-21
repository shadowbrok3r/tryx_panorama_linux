// ============================================================================
// Serial Protocol Implementation
// Reverse-engineered from com.baiyi.service.serialservice.serialdataservice
// ============================================================================

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::Write,
    sync::atomic::{AtomicI64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const FRAME_MARKER: u8 = 0x5A;
const ESCAPE_MARKER: u8 = 0x5B;
const CRLF: &str = "\r\n";

/// Session-wide monotonic sequence counter. The Windows app uses one counter
/// across all message types; the device replies with AckNumber = seq + 1 and
/// never generates its own sequence numbers.
static SEQ: AtomicI64 = AtomicI64::new(1);

pub fn next_seq() -> i64 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

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

/// Outgoing protocol message. Serializes to the HTTP-like wire format the
/// Windows app uses (verified from logcat captures — exactly these headers,
/// in this order; `-1`-valued headers are omitted on the wire):
///
/// ```text
/// METHOD cmdType 1\r\n
/// SeqNumber=<n>\r\n
/// Date=<epoch_ms>\r\n
/// ContentType=json\r\n
/// ContentLength=<len>\r\n
/// \r\n
/// {json}
/// ```
#[derive(Debug)]
pub struct CommandMessage<'a> {
    pub method: &'a str,
    pub cmd_type: &'a str,
    pub seq_number: i64,
    pub content_type: ContentType,
    pub body: &'a str,
    pub date: i64,
    /// Set only during file transfers (default "-1" = omitted from the wire)
    pub file_name: String,
    pub file_size: i64,
}

impl<'a> CommandMessage<'a> {
    pub fn new(method: &'a str, cmd_type: &'a str, body: &'a str) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();

        Self {
            method,
            cmd_type,
            seq_number: next_seq(),
            content_type: ContentType::Json,
            body,
            date: now as i64,
            file_name: "-1".to_string(),
            file_size: -1,
        }
    }

    /// Override the auto-assigned sequence number (file transfers reuse
    /// the transport seq)
    #[allow(dead_code)]
    pub fn seq_number(mut self, seq: i64) -> Self {
        self.seq_number = seq;
        self
    }

    /// Serialize with `pad` trailing spaces appended to the body (used to
    /// steer the frame CRC away from 0x5A/0x5B — see `to_frame`).
    fn to_bytes_padded(&self, pad: usize) -> anyhow::Result<Vec<u8>> {
        let mut msg = String::with_capacity(128 + self.body.len() + pad);

        // Request line: METHOD cmdType version
        write!(&mut msg, "{} {} 1{CRLF}", self.method, self.cmd_type)?;

        write!(&mut msg, "SeqNumber={}{CRLF}", self.seq_number)?;
        write!(&mut msg, "Date={}{CRLF}", self.date)?;
        write!(&mut msg, "ContentType={}{CRLF}", self.content_type.as_str())?;
        write!(&mut msg, "ContentLength={}{CRLF}", self.body.len() + pad)?;
        if self.file_name != "-1" {
            write!(&mut msg, "FileName={}{CRLF}", self.file_name)?;
        }
        if self.file_size != -1 {
            write!(&mut msg, "FileSize={}{CRLF}", self.file_size)?;
        }

        // Blank line + body
        msg.push_str(CRLF);
        msg.push_str(self.body);
        for _ in 0..pad {
            msg.push(' ');
        }

        Ok(msg.into_bytes())
    }

    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        self.to_bytes_padded(0)
    }

    /// Build the complete wire frame.
    ///
    /// Quirk handling: if the frame CRC lands on 0x5A/0x5B it would need
    /// escaping, but the device's parser strips the CRC *before* unescaping
    /// and mis-parses that case (the Windows app has the mirror bug: it sends
    /// such CRCs raw and the device leaks a junk byte into the JSON body).
    /// We sidestep the whole mess by padding the body with spaces until the
    /// CRC is a safe value — JSON-invisible and always terminates.
    pub fn to_frame(&self) -> anyhow::Result<Vec<u8>> {
        for pad in 0..16 {
            let bytes = self.to_bytes_padded(pad)?;
            let crc = frame_crc(&bytes);
            if crc != FRAME_MARKER && crc != ESCAPE_MARKER {
                return Ok(build_frame(&bytes));
            }
        }
        // Unreachable in practice (+0x20 per pad step escapes the 2-value zone)
        Ok(build_frame(&self.to_bytes()?))
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

/// Reverse of `escape_data`: 0x5B 0x01 -> 0x5A, 0x5B 0x02 -> 0x5B
fn unescape_data(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut iter = data.iter().copied();
    while let Some(b) = iter.next() {
        if b == ESCAPE_MARKER {
            match iter.next() {
                Some(0x01) => result.push(0x5A),
                Some(0x02) => result.push(0x5B),
                // Unknown escape — keep both bytes so nothing is silently lost
                Some(other) => {
                    result.push(b);
                    result.push(other);
                }
                None => result.push(b),
            }
        } else {
            result.push(b);
        }
    }
    result
}

/// Calculate simple sum CRC (1 byte)
fn calc_crc(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

/// CRC the frame for `message` would carry: sum over [len_hi, len_lo, message…]
fn frame_crc(message: &[u8]) -> u8 {
    let [hi, lo] = ((message.len() + 5) as u16).to_be_bytes();
    message
        .iter()
        .fold(hi.wrapping_add(lo), |acc, &b| acc.wrapping_add(b))
}

/// Frame builder, matching the device's `SerialMsgManager.sendRequestMsg`:
///
/// ```text
/// interior = [len:2 BE][message][crc:1]
///   len = message.len() + 5   (covers len:2 + message + crc:1 + both 0x5A markers)
///   crc = sum(len_hi, len_lo, message…) & 0xFF   (over UNESCAPED bytes)
/// frame = [0x5A][escape(interior)][0x5A]
/// ```
///
/// The whole interior is escaped, so 0x5A never occurs between the markers.
/// (The device parses frames delimiter-based and ignores len/crc on receive,
/// but we emit them correctly.)
fn build_frame(message: &[u8]) -> Vec<u8> {
    let len = (message.len() + 5) as u16;

    let mut interior = Vec::with_capacity(message.len() + 3);
    interior.extend_from_slice(&len.to_be_bytes());
    interior.extend_from_slice(message);
    interior.push(calc_crc(&interior));

    let escaped = escape_data(&interior);
    let mut frame = Vec::with_capacity(escaped.len() + 2);
    frame.push(FRAME_MARKER);
    frame.extend_from_slice(&escaped);
    frame.push(FRAME_MARKER);
    frame
}

// ============================================================================
// Receive side: incremental frame decoder + message parser
// ============================================================================

/// Incremental decoder for the frame format above. Feed raw serial bytes with
/// `push()`, then drain complete frames with `next_frame()`.
///
/// Since the interior is fully escaped, the next 0x5A after a start marker is
/// always the end marker — frames are extracted delimiter-based, then length
/// and CRC are checked on the unescaped interior. (Unlike the device's own
/// fragile single-read parser, this reassembles frames across reads.)
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// Bytes discarded while hunting for frame sync (diagnostics)
    pub discarded: u64,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    #[allow(dead_code)]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Try to extract the next complete frame. Returns the unescaped message
    /// payload (the HTTP-like text). Resyncs automatically on invalid frames.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        loop {
            // Hunt for a start marker
            match self.buf.iter().position(|&b| b == FRAME_MARKER) {
                Some(0) => {}
                Some(n) => {
                    self.discarded += n as u64;
                    self.buf.drain(..n);
                }
                None => {
                    self.discarded += self.buf.len() as u64;
                    self.buf.clear();
                    return None;
                }
            }

            // Find the end marker (first 0x5A after the start)
            let Some(end) = self.buf[1..].iter().position(|&b| b == FRAME_MARKER).map(|p| p + 1)
            else {
                // Incomplete frame — wait for more bytes
                return None;
            };

            let interior = unescape_data(&self.buf[1..end]);

            // Minimum interior: len:2 + crc:1 (payload could in theory be empty)
            if interior.len() < 3 {
                // Adjacent markers (frame boundary) or false sync — skip one byte
                self.discarded += 1;
                self.buf.drain(..1);
                continue;
            }

            let declared_len = u16::from_be_bytes([interior[0], interior[1]]) as usize;
            let payload = &interior[2..interior.len() - 1];
            let crc = interior[interior.len() - 1];
            let crc_calc = calc_crc(&interior[..interior.len() - 1]);

            let len_ok = declared_len == payload.len() + 5;
            let crc_ok = crc == crc_calc;

            if !len_ok && !crc_ok {
                // Both checks failed: we probably synced onto a stray 0x5A.
                // Skip this marker and rescan from the next byte.
                log::debug!(
                    "Frame candidate failed len ({} vs {}) and crc ({:02x} vs {:02x}) — resyncing",
                    declared_len,
                    payload.len() + 5,
                    crc,
                    crc_calc
                );
                self.discarded += 1;
                self.buf.drain(..1);
                continue;
            }
            if !crc_ok {
                log::warn!("Frame CRC mismatch (got {crc:02x}, calc {crc_calc:02x}) — accepting anyway");
            } else if !len_ok {
                log::warn!(
                    "Frame length field {} != payload+5 ({}) — accepting anyway",
                    declared_len,
                    payload.len() + 5
                );
            }

            let payload = payload.to_vec();
            self.buf.drain(..=end);
            return Some(payload);
        }
    }
}

/// A parsed incoming protocol message.
///
/// Two request-line shapes exist on the wire (from `SerialMsgManager`):
/// - Requests:  `POST <cmdType> 1` (3 tokens: method, cmdType, version)
/// - Replies:   `1 200`            (2 tokens: version, code — method empty)
#[derive(Debug, Clone)]
pub struct ParsedMessage {
    /// POST/STATE/GET/DELETE for requests; empty for replies
    pub method: String,
    /// Command type (conn, all, config, …) or numeric status code for replies
    pub cmd_type: String,
    /// Protocol version ("1")
    pub version: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl ParsedMessage {
    pub fn header_i64(&self, key: &str) -> Option<i64> {
        self.headers.get(key).and_then(|v| v.parse().ok())
    }

    pub fn seq_number(&self) -> Option<i64> {
        self.header_i64("SeqNumber")
    }

    pub fn ack_number(&self) -> Option<i64> {
        self.header_i64("AckNumber")
    }

    /// Body parsed as JSON, if possible
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(self.body.trim()).ok()
    }

    /// True for device replies (`1 200` request line, no method token)
    pub fn is_reply(&self) -> bool {
        self.method.is_empty()
    }
}

/// Parse a frame payload into a message. Lenient: tolerates missing tokens,
/// unknown headers and non-JSON bodies, since we're still mapping the protocol.
pub fn parse_message(payload: &[u8]) -> anyhow::Result<ParsedMessage> {
    let text = String::from_utf8_lossy(payload);

    // Split headers from body at the first blank line
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => (text.as_ref(), ""),
    };

    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let tokens: Vec<&str> = request_line.split_whitespace().collect();
    let (method, cmd_type, version) = match tokens.len() {
        // Request: "POST conn 1"
        3.. => (tokens[0], tokens[1], tokens[2]),
        // Reply: "1 200" (version, code)
        2 => ("", tokens[1], tokens[0]),
        1 => ("", tokens[0], ""),
        _ => ("", "", ""),
    };
    let (method, cmd_type, version) =
        (method.to_string(), cmd_type.to_string(), version.to_string());

    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once('=') {
            headers.insert(k.trim().to_string(), v.trim().to_string());
        }
    }

    Ok(ParsedMessage {
        method,
        cmd_type,
        version,
        headers,
        body: body.to_string(),
    })
}

// ============================================================================
// Send helpers
// ============================================================================

/// Send a framed POST command over serial
pub fn send_command(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd_type: &str,
    json_value: &serde_json::Value,
) -> anyhow::Result<()> {
    send_request(port, "POST", cmd_type, json_value)
}

/// Send a framed STATE command over serial (used for sysinfo updates)
pub fn send_state_command(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd_type: &str,
    json_value: &serde_json::Value,
) -> anyhow::Result<()> {
    send_request(port, "STATE", cmd_type, json_value)
}

/// Send a framed request with an arbitrary method (POST/STATE/...)
pub fn send_request(
    port: &mut Box<dyn serialport::SerialPort>,
    method: &str,
    cmd_type: &str,
    json_value: &serde_json::Value,
) -> anyhow::Result<()> {
    let body = serde_json::to_string(json_value)?;
    let msg = CommandMessage::new(method, cmd_type, &body);
    send_message(port, &msg)
}

/// Frame and send a fully-constructed message (for custom headers, transfers…)
pub fn send_message(
    port: &mut Box<dyn serialport::SerialPort>,
    msg: &CommandMessage,
) -> anyhow::Result<()> {
    let frame = msg.to_frame()?;

    log::info!(
        "Sending {} {} seq={} ({} bytes)",
        msg.method,
        msg.cmd_type,
        msg.seq_number,
        msg.body.len()
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

pub fn hex_string(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_escape() {
        let data = [0x00, 0x5A, 0x5B, 0xFF, 0x5A, 0x5A];
        assert_eq!(unescape_data(&escape_data(&data)), data);
    }

    #[test]
    fn roundtrip_frame() {
        let msg = CommandMessage::new("POST", "conn", "{\"a\":1}");
        let bytes = msg.to_bytes().unwrap();
        let frame = build_frame(&bytes);

        let mut dec = FrameDecoder::new();
        // Feed in two chunks to exercise incremental parsing
        dec.push(&frame[..7]);
        assert!(dec.next_frame().is_none());
        dec.push(&frame[7..]);
        let payload = dec.next_frame().unwrap();
        assert_eq!(payload, bytes);

        let parsed = parse_message(&payload).unwrap();
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.cmd_type, "conn");
        assert_eq!(parsed.version, "1");
        assert_eq!(parsed.body, "{\"a\":1}");
        assert!(parsed.seq_number().is_some());
    }

    #[test]
    fn resync_after_garbage() {
        let msg = CommandMessage::new("POST", "all", "{}");
        let frame = build_frame(&msg.to_bytes().unwrap());

        let mut dec = FrameDecoder::new();
        dec.push(&[0xDE, 0xAD, 0x5A, 0x00]); // garbage incl. a false start marker
        dec.push(&frame);
        let payload = dec.next_frame().expect("should resync onto real frame");
        assert_eq!(parse_message(&payload).unwrap().cmd_type, "all");
    }

    #[test]
    fn back_to_back_frames() {
        let f1 = build_frame(&CommandMessage::new("POST", "conn", "{}").to_bytes().unwrap());
        let f2 = build_frame(&CommandMessage::new("STATE", "all", "{\"x\":2}").to_bytes().unwrap());

        let mut dec = FrameDecoder::new();
        dec.push(&f1);
        dec.push(&f2);
        assert_eq!(parse_message(&dec.next_frame().unwrap()).unwrap().cmd_type, "conn");
        assert_eq!(parse_message(&dec.next_frame().unwrap()).unwrap().cmd_type, "all");
        assert!(dec.next_frame().is_none());
    }

    #[test]
    fn parse_device_reply_line() {
        // Device replies use "version code" (2 tokens), e.g. "1 200"
        let payload = b"1 200\r\nAckNumber=124\r\nContentLength=0\r\nContentType=json\r\n\r\n";
        let parsed = parse_message(payload).unwrap();
        assert!(parsed.is_reply());
        assert_eq!(parsed.cmd_type, "200");
        assert_eq!(parsed.version, "1");
        assert_eq!(parsed.ack_number(), Some(124));
    }

    #[test]
    fn windows_header_format() {
        let msg = CommandMessage::new("STATE", "all", "{}");
        let text = String::from_utf8(msg.to_bytes().unwrap()).unwrap();
        let head = text.split("\r\n\r\n").next().unwrap();
        let lines: Vec<&str> = head.split("\r\n").collect();
        assert_eq!(lines[0], "STATE all 1");
        assert!(lines[1].starts_with("SeqNumber="));
        assert!(lines[2].starts_with("Date="));
        assert_eq!(lines[3], "ContentType=json");
        assert_eq!(lines[4], "ContentLength=2");
        assert_eq!(lines.len(), 5, "requests carry exactly these 4 headers");
    }

    #[test]
    fn seq_is_monotonic() {
        let a = CommandMessage::new("POST", "x", "{}").seq_number;
        let b = CommandMessage::new("POST", "y", "{}").seq_number;
        assert!(b > a);
    }

    #[test]
    fn crc_avoidance_padding() {
        // Across many body sizes, the emitted CRC byte must never be
        // 0x5A/0x5B (padded away), and every frame must round-trip.
        for i in 0..600 {
            let body = format!("{{\"v\":\"{}\"}}", "x".repeat(i));
            let msg = CommandMessage::new("POST", "t", &body);
            let frame = msg.to_frame().unwrap();

            let interior = unescape_data(&frame[1..frame.len() - 1]);
            let crc = *interior.last().unwrap();
            assert!(crc != 0x5A && crc != 0x5B, "crc {crc:02x} not padded away (i={i})");

            let mut dec = FrameDecoder::new();
            dec.push(&frame);
            let parsed = parse_message(&dec.next_frame().expect("frame")).unwrap();
            assert_eq!(parsed.cmd_type, "t");
            // Padding must stay JSON-invisible
            assert!(parsed.json().is_some(), "padded body must still parse as JSON");
        }
    }

    #[test]
    fn frame_bytes_never_contain_marker_inside() {
        // A payload engineered so len/crc bytes would be 0x5A without escaping
        let body = "x".repeat(0x5A - 5 - 30); // aim length low byte at tricky values
        let msg = CommandMessage::new("POST", "t", &body);
        let frame = build_frame(&msg.to_bytes().unwrap());
        let interior = &frame[1..frame.len() - 1];
        assert!(!interior.contains(&0x5A), "interior must be fully escaped");
        assert_eq!(frame[0], 0x5A);
        assert_eq!(*frame.last().unwrap(), 0x5A);
    }
}
