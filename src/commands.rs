// ============================================================================
// CLI subcommand implementations (headless operation)
// ============================================================================

use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::data::{self, FrameDecoder, ParsedMessage};
use crate::gallery::{self, Gallery};
use crate::screen_setup::{AioCoolerController, ScreenConfig};
use crate::sysinfo::{SysInfo, SysInfoReader};

const TRYX_VID: u16 = 0x18d1;
const TRYX_PID: u16 = 0x2d03;

/// Open the device transport. A `tcp://host:port` path connects to a remote
/// `bridge` (see [`bridge`]) instead of a local serial device, so the desktop
/// GUI/CLI can drive a cooler attached to another machine on the LAN. Everything
/// downstream keeps operating on a `Box<dyn serialport::SerialPort>`.
pub fn open_port(path: &str) -> Result<Box<dyn serialport::SerialPort>> {
    if let Some(addr) = path.strip_prefix("tcp://") {
        let stream = TcpStream::connect(addr).with_context(|| {
            format!(
                "Failed to connect to serial bridge {addr}\n\
                 Start the bridge on the machine wired to the cooler:\n\
                 tryx_panorama_linux --port /dev/tryx0 bridge --listen 0.0.0.0:9600"
            )
        })?;
        stream.set_nodelay(true).ok();
        let timeout = Duration::from_millis(100);
        stream.set_read_timeout(Some(timeout)).ok();
        return Ok(Box::new(TcpSerial {
            stream,
            addr: addr.to_string(),
            timeout,
        }));
    }
    let port = serialport::new(path, 115200)
        .timeout(Duration::from_millis(100))
        .open()
        .with_context(|| {
            format!(
                "Failed to open serial port {path}\n\
                 If this is a permission error: sudo usermod -aG dialout $USER (then re-login),\n\
                 or install the udev rule from the README. Run `detect` for diagnostics."
            )
        })?;
    Ok(port)
}

/// A `TcpStream` masquerading as a `serialport::SerialPort` so the network
/// transport is a drop-in for the local one. The one behavior that matters to
/// callers: a read timeout on a socket surfaces as `WouldBlock`, but the framing
/// loops expect a serial-style `TimedOut` — so we remap it in `read`.
struct TcpSerial {
    stream: TcpStream,
    addr: String,
    timeout: Duration,
}

impl io::Read for TcpSerial {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.stream.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                Err(io::Error::new(io::ErrorKind::TimedOut, "tcp read timeout"))
            }
            other => other,
        }
    }
}

impl io::Write for TcpSerial {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl serialport::SerialPort for TcpSerial {
    fn name(&self) -> Option<String> {
        Some(format!("tcp://{}", self.addr))
    }
    fn baud_rate(&self) -> serialport::Result<u32> {
        Ok(115200)
    }
    fn data_bits(&self) -> serialport::Result<serialport::DataBits> {
        Ok(serialport::DataBits::Eight)
    }
    fn flow_control(&self) -> serialport::Result<serialport::FlowControl> {
        Ok(serialport::FlowControl::None)
    }
    fn parity(&self) -> serialport::Result<serialport::Parity> {
        Ok(serialport::Parity::None)
    }
    fn stop_bits(&self) -> serialport::Result<serialport::StopBits> {
        Ok(serialport::StopBits::One)
    }
    fn timeout(&self) -> Duration {
        self.timeout
    }
    fn set_baud_rate(&mut self, _baud_rate: u32) -> serialport::Result<()> {
        Ok(())
    }
    fn set_data_bits(&mut self, _data_bits: serialport::DataBits) -> serialport::Result<()> {
        Ok(())
    }
    fn set_flow_control(
        &mut self,
        _flow_control: serialport::FlowControl,
    ) -> serialport::Result<()> {
        Ok(())
    }
    fn set_parity(&mut self, _parity: serialport::Parity) -> serialport::Result<()> {
        Ok(())
    }
    fn set_stop_bits(&mut self, _stop_bits: serialport::StopBits) -> serialport::Result<()> {
        Ok(())
    }
    fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
        self.timeout = timeout;
        self.stream.set_read_timeout(Some(timeout)).map_err(|e| {
            serialport::Error::new(serialport::ErrorKind::Io(e.kind()), e.to_string())
        })
    }
    fn write_request_to_send(&mut self, _level: bool) -> serialport::Result<()> {
        Ok(())
    }
    fn write_data_terminal_ready(&mut self, _level: bool) -> serialport::Result<()> {
        Ok(())
    }
    fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn bytes_to_read(&self) -> serialport::Result<u32> {
        Ok(0)
    }
    fn bytes_to_write(&self) -> serialport::Result<u32> {
        Ok(0)
    }
    fn clear(&self, _buffer_to_clear: serialport::ClearBuffer) -> serialport::Result<()> {
        Ok(())
    }
    fn try_clone(&self) -> serialport::Result<Box<dyn serialport::SerialPort>> {
        let stream = self.stream.try_clone().map_err(|e| {
            serialport::Error::new(serialport::ErrorKind::Io(e.kind()), e.to_string())
        })?;
        Ok(Box::new(TcpSerial {
            stream,
            addr: self.addr.clone(),
            timeout: self.timeout,
        }))
    }
    fn set_break(&self) -> serialport::Result<()> {
        Ok(())
    }
    fn clear_break(&self) -> serialport::Result<()> {
        Ok(())
    }
}

/// Run a raw TCP <-> serial bridge so a remote machine can drive this cooler.
/// One client at a time (the serial port is exclusive); bytes are relayed
/// verbatim in both directions, so the remote side runs the exact same command
/// code against a `tcp://` transport. Ctrl-C to stop.
///
/// Security: this exposes full control of the cooler to anyone who can reach
/// `listen`. Bind to a trusted LAN interface (e.g. `192.168.1.50:9600`) or keep
/// it firewalled; there is no authentication.
pub fn bridge(port_path: &str, listen: &str) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .with_context(|| format!("Failed to bind bridge listener on {listen}"))?;
    println!("Serial bridge on {listen}  <->  {port_path}");
    println!("Remote side: tryx_panorama_linux --port tcp://<this-host>:{} <cmd>",
        listen.rsplit(':').next().unwrap_or("9600"));
    println!("Waiting for a client (one at a time)...");

    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(e) => {
                log::warn!("accept failed: {e}");
                continue;
            }
        };
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".into());
        println!("Client connected: {peer}");
        if let Err(e) = bridge_session(port_path, stream) {
            log::warn!("bridge session ended: {e:#}");
        }
        println!("Client disconnected: {peer}");
    }
    Ok(())
}

/// Pump bytes both ways for a single connected client until either side closes.
fn bridge_session(port_path: &str, stream: TcpStream) -> Result<()> {
    // A short read timeout on both endpoints lets each copy thread notice the
    // `alive` flag flip and exit promptly when the other direction tears down.
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok();
    let serial_to_net = open_port(port_path)?;
    let net_to_serial = serial_to_net
        .try_clone()
        .context("serial try_clone for bridge")?;
    let net_reader = stream.try_clone().context("tcp try_clone for bridge")?;

    let alive = Arc::new(AtomicBool::new(true));

    // serial -> net
    let a_alive = alive.clone();
    let mut serial_rx = serial_to_net;
    let mut net_tx = stream;
    let up = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while a_alive.load(Ordering::Relaxed) {
            match serial_rx.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => {
                    if net_tx.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
                Err(_) => break,
            }
        }
        a_alive.store(false, Ordering::Relaxed);
    });

    // net -> serial (this thread)
    let mut net_rx = net_reader;
    let mut serial_tx = net_to_serial;
    let mut buf = [0u8; 4096];
    while alive.load(Ordering::Relaxed) {
        match net_rx.read(&mut buf) {
            Ok(0) => break, // client closed
            Ok(n) => {
                if serial_tx.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    alive.store(false, Ordering::Relaxed);
    let _ = up.join();
    Ok(())
}

/// Read whatever is available on the port, decode frames, return parsed messages.
pub fn pump_incoming(
    port: &mut Box<dyn serialport::SerialPort>,
    dec: &mut FrameDecoder,
    hex: bool,
) -> Result<Vec<ParsedMessage>> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match port.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if hex {
                    println!("RX {:4} bytes: {}", n, data::hex_string(&chunk[..n]));
                }
                dec.push(&chunk[..n]);
                while let Some(frame) = dec.next_frame() {
                    match data::parse_message(&frame) {
                        Ok(msg) => out.push(msg),
                        Err(e) => log::warn!("Unparseable frame ({} bytes): {e}", frame.len()),
                    }
                }
                // Keep draining until a read times out so we don't fall behind
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(e).context("Serial read error"),
        }
    }
    Ok(out)
}

pub fn print_message(msg: &ParsedMessage) {
    let ts = chrono::Local::now().format("%H:%M:%S%.3f");
    if msg.is_reply() {
        println!(
            "◀ [{ts}] reply {} v{} ack={}",
            msg.cmd_type,
            msg.version,
            msg.ack_number().unwrap_or(-1),
        );
    } else {
        println!(
            "◀ [{ts}] {} {} v{} seq={} ack={}",
            msg.method,
            msg.cmd_type,
            msg.version,
            msg.seq_number().unwrap_or(-1),
            msg.ack_number().unwrap_or(-1),
        );
    }
    log::debug!("headers: {:?}", msg.headers);
    if let Some(json) = msg.json() {
        let pretty = serde_json::to_string_pretty(&json).unwrap_or_else(|_| msg.body.clone());
        println!("{pretty}");
    } else if !msg.body.is_empty() {
        println!(
            "(non-JSON body, {} bytes) {}",
            msg.body.len(),
            msg.body.chars().take(300).collect::<String>()
        );
    }
}

// ============================================================================
// detect — device / permission / adb diagnostics
// ============================================================================

pub fn detect() -> Result<()> {
    println!("── Serial ports ─────────────────────────────────────────────");
    let mut candidates: Vec<String> = Vec::new();
    match serialport::available_ports() {
        Ok(ports) if !ports.is_empty() => {
            for p in ports {
                match &p.port_type {
                    serialport::SerialPortType::UsbPort(info) => {
                        let is_tryx = info.vid == TRYX_VID && info.pid == TRYX_PID;
                        println!(
                            "  {}  USB {:04x}:{:04x}  manufacturer={:?} product={:?} serial={:?}{}",
                            p.port_name,
                            info.vid,
                            info.pid,
                            info.manufacturer.as_deref().unwrap_or("?"),
                            info.product.as_deref().unwrap_or("?"),
                            info.serial_number.as_deref().unwrap_or("?"),
                            if is_tryx { "  ← Tryx Panorama" } else { "" },
                        );
                        if is_tryx {
                            candidates.insert(0, p.port_name.clone());
                        } else {
                            candidates.push(p.port_name.clone());
                        }
                    }
                    other => println!("  {}  ({other:?})", p.port_name),
                }
            }
        }
        Ok(_) => println!("  (none found)"),
        Err(e) => println!("  enumeration failed: {e}"),
    }

    println!("\n── Stable symlinks (/dev/serial/by-id) ─────────────────────");
    match std::fs::read_dir("/dev/serial/by-id") {
        Ok(entries) => {
            for entry in entries.flatten() {
                let target = std::fs::canonicalize(entry.path())
                    .map(|t| t.display().to_string())
                    .unwrap_or_else(|_| "?".into());
                println!("  {} → {}", entry.path().display(), target);
            }
        }
        Err(_) => println!("  (none)"),
    }

    println!("\n── Open test ───────────────────────────────────────────────");
    if candidates.is_empty() {
        candidates.push("/dev/ttyACM0".to_string());
    }
    for path in &candidates {
        match serialport::new(path.as_str(), 115200)
            .timeout(Duration::from_millis(100))
            .open()
        {
            Ok(_) => println!("  {path}: OK (openable)"),
            Err(e) => {
                println!("  {path}: FAILED — {e}");
                if e.to_string().to_lowercase().contains("permission") {
                    println!(
                        "    hint: sudo usermod -aG dialout $USER  (then log out/in),\n\
                         \x20   or a udev rule granting access (see README)"
                    );
                }
            }
        }
    }

    println!("\n── ADB ─────────────────────────────────────────────────────");
    match Command::new("adb").arg("version").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout);
            println!("  {}", v.lines().next().unwrap_or("adb present"));
            if let Ok(devs) = Command::new("adb").args(["devices", "-l"]).output() {
                let list = String::from_utf8_lossy(&devs.stdout);
                for line in list.lines().skip(1).filter(|l| !l.trim().is_empty()) {
                    println!("  {line}");
                }
            }
        }
        _ => println!("  adb not found — install with: sudo apt install adb"),
    }

    Ok(())
}

// ============================================================================
// listen — decode and print incoming traffic
// ============================================================================

pub fn listen(port_path: &str, hex: bool, timeout_secs: Option<u64>, keepalive: bool) -> Result<()> {
    let mut port = open_port(port_path)?;
    let mut dec = FrameDecoder::new();
    println!("Listening on {port_path} (Ctrl-C to stop)…");
    if keepalive {
        // 1 Hz like the Windows app: holds the device's 10s RX watchdog open
        // AND feeds cpu.temperature to its smart fan curve
        println!("Keepalive on: sending STATE all at 1Hz (like the Windows app)");
    }

    let mut reader = keepalive.then(SysInfoReader::new);
    let start = Instant::now();
    let mut last_keepalive = Instant::now() - Duration::from_secs(60);
    let mut last_discarded = 0u64;
    loop {
        if let Some(reader) = reader.as_mut() {
            if last_keepalive.elapsed() >= Duration::from_secs(1) {
                let info = reader.sample();
                data::send_state_command(&mut port, "all", &serde_json::to_value(&info)?)?;
                last_keepalive = Instant::now();
            }
        }
        for msg in pump_incoming(&mut port, &mut dec, hex)? {
            print_message(&msg);
        }
        if dec.discarded != last_discarded {
            log::warn!("Discarded {} bytes hunting for frame sync", dec.discarded - last_discarded);
            last_discarded = dec.discarded;
        }
        if let Some(t) = timeout_secs {
            if start.elapsed() >= Duration::from_secs(t) {
                println!("Done after {t}s. ({} bytes of garbage discarded)", dec.discarded);
                break;
            }
        }
    }
    Ok(())
}

// ============================================================================
// conn — handshake: query device identity/capabilities
// ============================================================================

/// Send `POST conn` and wait for the capability reply. Retries because the
/// device ignores everything for the first ~4-5s after its service starts
/// (pump-detection init gate), and reopens its port after 10s of silence.
pub fn conn(port_path: &str, retries: u32) -> Result<()> {
    let mut port = open_port(port_path)?;
    let mut dec = FrameDecoder::new();

    for attempt in 1..=retries {
        log::info!("conn attempt {attempt}/{retries}");
        data::send_command(&mut port, "conn", &serde_json::json!({}))?;

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            for msg in pump_incoming(&mut port, &mut dec, false)? {
                print_message(&msg);
                if msg.is_reply() && !msg.body.trim().is_empty() {
                    println!("Device identified ✔");
                    return Ok(());
                }
            }
        }
    }
    anyhow::bail!(
        "No conn reply after {retries} attempts. The device may still be booting, \
         the wrong port may be selected (try `detect`), or the service is not running."
    )
}

// ============================================================================
// pump — turbo pump control (sysfs: aio_cooler control_source/pwm)
// ============================================================================

pub fn pump(port_path: &str, enable: bool, value: u32, wait_secs: u64) -> Result<()> {
    let mut port = open_port(port_path)?;
    let mut dec = FrameDecoder::new();

    data::send_command(
        &mut port,
        "turboPump",
        &serde_json::json!({ "enable": enable, "value": value }),
    )?;

    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    while Instant::now() < deadline {
        for msg in pump_incoming(&mut port, &mut dec, false)? {
            print_message(&msg);
        }
    }
    Ok(())
}

// ============================================================================
// send — raw command for protocol experimentation
// ============================================================================

pub fn send(
    port_path: &str,
    method: &str,
    cmd_type: &str,
    json: &str,
    wait_secs: u64,
) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(json).context("--json is not valid JSON")?;

    let mut port = open_port(port_path)?;
    let mut dec = FrameDecoder::new();

    // Drain anything stale first so replies correlate with our command
    for msg in pump_incoming(&mut port, &mut dec, false)? {
        print_message(&msg);
    }

    data::send_request(&mut port, method, cmd_type, &value)?;

    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    while Instant::now() < deadline {
        for msg in pump_incoming(&mut port, &mut dec, false)? {
            print_message(&msg);
        }
    }
    Ok(())
}

// ============================================================================
// sysinfo — stream STATE all updates (what makes temps show on the display)
// ============================================================================

pub fn sysinfo_stream(port_path: &str, interval_ms: u64, count: u64, dry_run: bool) -> Result<()> {
    let mut port = if dry_run { None } else { Some(open_port(port_path)?) };
    let mut dec = FrameDecoder::new();
    let mut reader = SysInfoReader::new();
    reader.log_detected();
    let mut sent = 0u64;

    loop {
        // Rate fields (load, IO, net, power) are deltas; prime with a short
        // wait before the first sample so tick 1 isn't all zeros.
        if sent == 0 {
            thread::sleep(Duration::from_millis(interval_ms.min(300)));
        }
        let info = reader.sample();
        let json = serde_json::to_value(&info)?;

        if let Some(port) = port.as_mut() {
            data::send_state_command(port, "all", &json)?;
            log::info!(
                "sysinfo #{}: CPU {}°C {}% {}MHz {}W, GPU {}°C {}% {}W",
                sent + 1,
                info.cpu.temperature,
                info.cpu.load,
                info.cpu.speedAverage,
                info.cpu.power,
                info.gpu.temperature,
                info.gpu.load,
                info.gpu.power,
            );
            for msg in pump_incoming(port, &mut dec, false)? {
                print_message(&msg);
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&json)?);
        }

        sent += 1;
        if count > 0 && sent >= count {
            break;
        }
        thread::sleep(Duration::from_millis(interval_ms));
    }
    Ok(())
}

// ============================================================================
// daemon — long-running sysinfo streamer with auto-reconnect + graceful stop
// ============================================================================

/// Latest device-reported status, parsed from the 200 reply bodies.
#[derive(Default)]
struct DeviceStatus {
    fan_lcd: String,
    turbo_pump: String,
    available_storage: u64,
    warning: String,
}

impl DeviceStatus {
    fn update(&mut self, msg: &ParsedMessage) {
        if let Some(json) = msg.json() {
            if let Some(s) = json.get("status") {
                if let Some(v) = s.get("fanLCD").and_then(|v| v.as_str()) {
                    self.fan_lcd = v.to_string();
                }
                if let Some(v) = s.get("turboPump").and_then(|v| v.as_str()) {
                    self.turbo_pump = v.to_string();
                }
            }
            if let Some(v) = json.get("availableStorage").and_then(|v| v.as_u64()) {
                self.available_storage = v;
            }
            if let Some(v) = json.get("warning").and_then(|v| v.as_str()) {
                self.warning = v.to_string();
            }
        }
    }
}

fn install_signal_handler() -> Arc<AtomicBool> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    }) {
        log::warn!("Could not install signal handler: {e} (Ctrl-C won't be graceful)");
    }
    running
}

/// Sleep until `deadline`, waking early (and returning false) if shutdown fires.
fn sleep_until(deadline: Instant, running: &AtomicBool) -> bool {
    while running.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(100)));
    }
    false
}

pub fn daemon(
    port_path: &str,
    interval_ms: u64,
    do_conn: bool,
    quiet: bool,
    status_every: u64,
    gallery_path: &Path,
    no_gallery: bool,
) -> Result<()> {
    let running = install_signal_handler();
    let mut reader = SysInfoReader::new();
    reader.log_detected();
    log::info!(
        "Daemon starting: {port_path} @ {interval_ms}ms interval (Ctrl-C / SIGTERM to stop)"
    );

    let reconnect_delay = Duration::from_millis(interval_ms.clamp(500, 3000));

    // Outer loop: (re)establish the connection whenever it drops
    while running.load(Ordering::SeqCst) {
        let mut port = match open_port(port_path) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("Open failed: {e:#}. Retrying in {:?}…", reconnect_delay);
                if !sleep_until(Instant::now() + reconnect_delay, &running) {
                    break;
                }
                continue;
            }
        };
        let mut dec = FrameDecoder::new();
        log::info!("Connected on {port_path}");

        if do_conn {
            if let Err(e) = data::send_command(&mut port, "conn", &serde_json::json!({})) {
                log::warn!("conn send failed: {e:#}");
            } else if let Ok(msgs) = pump_for(&mut port, &mut dec, Duration::from_millis(1500)) {
                for m in &msgs {
                    if m.is_reply() && !m.body.trim().is_empty() {
                        log::info!("Device: {}", m.body.trim());
                    }
                }
            }
        }

        // Re-apply the persistent gallery on every (re)connect so the display
        // survives reboots and the device's 60s watchdog. Reloaded each time so
        // CLI/GUI edits are picked up on the next reconnect.
        if !no_gallery {
            match Gallery::load(gallery_path) {
                Ok(g) if !g.media.is_empty() => match send_gallery_on(&mut port, &mut dec, &g) {
                    Ok(()) => log::info!(
                        "Applied gallery: {} image(s) [{}]",
                        g.media.len(),
                        g.effective_play_mode()
                    ),
                    Err(e) => log::warn!("gallery apply failed: {e:#}"),
                },
                Ok(_) => {} // empty gallery: nothing to restore
                Err(e) => log::warn!("gallery load failed ({}): {e:#}", gallery_path.display()),
            }
        }

        let mut status = DeviceStatus::default();
        let mut ticks = 0u64;

        // Inner loop: stream sysinfo until shutdown or a serial error
        'stream: while running.load(Ordering::SeqCst) {
            let info = reader.sample();
            let json = match serde_json::to_value(&info) {
                Ok(j) => j,
                Err(e) => {
                    log::error!("sysinfo serialize failed: {e}");
                    break 'stream;
                }
            };

            if let Err(e) = data::send_state_command(&mut port, "all", &json) {
                log::warn!("Send failed ({e}); assuming disconnect, will reconnect");
                break 'stream;
            }

            match pump_incoming(&mut port, &mut dec, false) {
                Ok(msgs) => {
                    for m in &msgs {
                        status.update(m);
                    }
                }
                Err(e) => {
                    log::warn!("Read failed ({e}); assuming disconnect, will reconnect");
                    break 'stream;
                }
            }

            ticks += 1;
            if !quiet && status_every > 0 && ticks % status_every == 0 {
                println!(
                    "[{}] sent CPU {}°C {}% {}W / GPU {}°C {}% {}W  │  device fanLCD={} pump={} free={:.2}GB",
                    chrono::Local::now().format("%H:%M:%S"),
                    info.cpu.temperature,
                    info.cpu.load,
                    info.cpu.power,
                    info.gpu.temperature,
                    info.gpu.load,
                    info.gpu.power,
                    if status.fan_lcd.is_empty() { "?" } else { &status.fan_lcd },
                    if status.turbo_pump.is_empty() { "?" } else { &status.turbo_pump },
                    status.available_storage as f64 / 1e9,
                );
            }

            if !sleep_until(Instant::now() + Duration::from_millis(interval_ms), &running) {
                break 'stream;
            }
        }

        // Left the stream loop: either shutting down, or reconnecting
        if !running.load(Ordering::SeqCst) {
            break;
        }
        drop(port);
        if !sleep_until(Instant::now() + reconnect_delay, &running) {
            break;
        }
    }

    log::info!("Daemon stopped.");
    Ok(())
}

/// Pump incoming for a fixed duration, collecting all parsed messages.
fn pump_for(
    port: &mut Box<dyn serialport::SerialPort>,
    dec: &mut FrameDecoder,
    dur: Duration,
) -> Result<Vec<ParsedMessage>> {
    let deadline = Instant::now() + dur;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        out.extend(pump_incoming(port, dec, false)?);
    }
    Ok(out)
}

// ============================================================================
// image / screen — media upload + display configuration
// ============================================================================

/// Print any replies that arrive within `dur`.
fn drain_replies(
    port: &mut Box<dyn serialport::SerialPort>,
    dec: &mut FrameDecoder,
    dur: Duration,
) -> Result<()> {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        for msg in pump_incoming(port, dec, false)? {
            print_message(&msg);
        }
    }
    Ok(())
}

/// Full upload flow, mirroring the Windows app exactly (from logcat captures):
/// sysinfo → `transport` (announce) → ADB push (the actual bytes; they never
/// cross the serial link) → `transported` (commit). Returns the device
/// filename. Does NOT touch the playlist/config — callers decide gallery
/// semantics (accumulate vs replace).
fn upload_file(
    port: &mut Box<dyn serialport::SerialPort>,
    dec: &mut FrameDecoder,
    controller: &AioCoolerController,
    path: &Path,
) -> Result<String> {
    let path_buf = path.to_path_buf();
    let file_md5 = AioCoolerController::calculate_md5(&path_buf)?;
    let file_size = std::fs::metadata(path)?.len();
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("png");
    let remote_name = AioCoolerController::generate_filename(extension);

    log::info!(
        "File: {} ({file_size} bytes, MD5 {file_md5}) → {remote_name}",
        path.display()
    );

    // Establish traffic (device resets its port after 10s of RX silence)
    data::send_state_command(port, "all", &serde_json::to_value(SysInfo::get_sysinfo())?)?;
    drain_replies(port, dec, Duration::from_millis(300))?;

    // 1. Announce the transfer (device opens/truncates the target file)
    data::send_command(
        port,
        "transport",
        &serde_json::json!({ "type": "media", "fileSize": file_size, "fileName": remote_name }),
    )?;
    drain_replies(port, dec, Duration::from_millis(500))?;

    // 2. Actual bytes go over ADB (the Windows app does the same — its serial
    //    md5 is literally the string "todo"; we send the real one, also unchecked)
    controller.adb_push(&path_buf, &remote_name)?;

    // 3. Commit
    data::send_command(
        port,
        "transported",
        &serde_json::json!({ "md5": file_md5, "fileName": remote_name }),
    )?;
    drain_replies(port, dec, Duration::from_millis(500))?;

    Ok(remote_name)
}

/// Send the gallery's playlist + display settings on an already-open port.
/// Used by upload/apply here and by the daemon (which owns its own port).
fn send_gallery_on(
    port: &mut Box<dyn serialport::SerialPort>,
    dec: &mut FrameDecoder,
    gallery: &Gallery,
) -> Result<()> {
    let mut cfg = gallery.config.clone();
    cfg.play_mode = gallery.effective_play_mode().to_string();
    data::send_command(port, "waterBlockScreenId", &cfg.to_water_block_json(&gallery.media))?;
    drain_replies(port, dec, Duration::from_millis(400))?;
    Ok(())
}

/// A few sysinfo ticks so temps populate and the link stays up after a config push.
fn keepalive_ticks(
    port: &mut Box<dyn serialport::SerialPort>,
    dec: &mut FrameDecoder,
    n: u32,
) -> Result<()> {
    for _ in 0..n {
        thread::sleep(Duration::from_millis(800));
        data::send_state_command(port, "all", &serde_json::to_value(SysInfo::get_sysinfo())?)?;
        drain_replies(port, dec, Duration::from_millis(200))?;
    }
    Ok(())
}

/// Upload an image and show it. Default is **accumulate**: the new file joins
/// the persistent gallery playlist (nothing is deleted) and the whole playlist
/// is (re)displayed. `replace` restores the old behavior — wipe every other
/// file and show only this one. `config` becomes the gallery's display settings.
pub fn image(
    port_path: &str,
    path: &PathBuf,
    config: &ScreenConfig,
    gallery_path: &Path,
    replace: bool,
) -> Result<()> {
    anyhow::ensure!(path.is_file(), "No such file: {}", path.display());

    let controller = AioCoolerController::new(port_path);
    let mut port = open_port(port_path)?;
    let mut dec = FrameDecoder::new();

    let remote_name = upload_file(&mut port, &mut dec, &controller, path)?;

    let mut gallery = Gallery::load(gallery_path)?;
    if replace {
        // Prune every other file, show just this one.
        data::send_command(
            &mut port,
            "mediaDelete",
            &serde_json::json!({ "type": "custom", "exclude": [remote_name] }),
        )?;
        drain_replies(&mut port, &mut dec, Duration::from_millis(300))?;
        gallery.media = vec![remote_name.clone()];
    } else {
        gallery.add(remote_name.clone());
    }
    gallery.config = config.clone();
    gallery.save(gallery_path)?;

    send_gallery_on(&mut port, &mut dec, &gallery)?;
    keepalive_ticks(&mut port, &mut dec, 5)?;

    println!(
        "Displayed {} image(s) [{}]; newest: {remote_name}",
        gallery.media.len(),
        gallery.effective_play_mode()
    );
    Ok(())
}

pub fn screen(port_path: &str, media: &[String], config: &ScreenConfig) -> Result<()> {
    let controller = AioCoolerController::new(port_path);
    controller.send_screen_config(media, config)?;
    println!("Screen configuration sent.");
    Ok(())
}

// ============================================================================
// gallery — persistent, accumulating image playlist
// ============================================================================

/// Re-send the saved gallery playlist to the device now (manual restore).
pub fn apply_gallery(port_path: &str, gallery_path: &Path) -> Result<()> {
    let gallery = Gallery::load(gallery_path)?;
    let mut port = open_port(port_path)?;
    let mut dec = FrameDecoder::new();
    // Prime the link before the config push.
    data::send_state_command(&mut port, "all", &serde_json::to_value(SysInfo::get_sysinfo())?)?;
    drain_replies(&mut port, &mut dec, Duration::from_millis(300))?;
    send_gallery_on(&mut port, &mut dec, &gallery)?;
    keepalive_ticks(&mut port, &mut dec, 3)?;
    println!(
        "Applied gallery: {} image(s) [{}]",
        gallery.media.len(),
        gallery.effective_play_mode()
    );
    Ok(())
}

/// Persist an explicit gallery (playlist order + play mode + display config)
/// and push it to the device. Used by the GUI, whose in-memory gallery is the
/// source of truth, rather than reading from disk first.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn gallery_write_apply(port_path: &str, gallery: &Gallery, gallery_path: &Path) -> Result<()> {
    gallery.save(gallery_path)?;
    let mut port = open_port(port_path)?;
    let mut dec = FrameDecoder::new();
    data::send_state_command(&mut port, "all", &serde_json::to_value(SysInfo::get_sysinfo())?)?;
    drain_replies(&mut port, &mut dec, Duration::from_millis(300))?;
    send_gallery_on(&mut port, &mut dec, gallery)?;
    keepalive_ticks(&mut port, &mut dec, 3)?;
    Ok(())
}

/// Add an image to the gallery without changing its display settings.
pub fn gallery_add(port_path: &str, path: &PathBuf, gallery_path: &Path) -> Result<()> {
    let gallery = Gallery::load(gallery_path)?;
    image(port_path, path, &gallery.config, gallery_path, false)
}

/// List device media, annotated with playlist position / foreign status.
pub fn gallery_list(gallery_path: &Path) -> Result<()> {
    let gallery = Gallery::load(gallery_path)?;
    let device = gallery::list_device_media()?;

    println!(
        "Gallery ({} in playlist, play mode: {}):",
        gallery.media.len(),
        gallery.effective_play_mode()
    );
    if gallery.media.is_empty() {
        println!("  (empty)");
    }
    for (i, name) in gallery.media.iter().enumerate() {
        let on_device = if device.iter().any(|d| d == name) { "" } else { "  [MISSING on device]" };
        println!("  {}. {name}{on_device}", i + 1);
    }

    let extras: Vec<&String> = device.iter().filter(|d| !gallery.contains(d)).collect();
    if !extras.is_empty() {
        println!("\nOn device but not in playlist:");
        for name in extras {
            let kind = if gallery::is_our_upload(name) { "upload" } else { "foreign" };
            println!("  {name}  ({kind})");
        }
    }
    Ok(())
}

/// Remove one image: delete its file on the device and drop it from the playlist.
pub fn gallery_rm(port_path: &str, name: &str, gallery_path: &Path) -> Result<()> {
    let mut gallery = Gallery::load(gallery_path)?;
    let was_in_playlist = gallery.remove(name);

    let mut port = open_port(port_path)?;
    let mut dec = FrameDecoder::new();
    data::send_state_command(&mut port, "all", &serde_json::to_value(SysInfo::get_sysinfo())?)?;
    drain_replies(&mut port, &mut dec, Duration::from_millis(300))?;
    // Delete ONLY this file (the doc's `include` form).
    data::send_command(
        &mut port,
        "mediaDelete",
        &serde_json::json!({ "type": "custom", "include": [name] }),
    )?;
    drain_replies(&mut port, &mut dec, Duration::from_millis(300))?;

    gallery.save(gallery_path)?;
    send_gallery_on(&mut port, &mut dec, &gallery)?;
    keepalive_ticks(&mut port, &mut dec, 3)?;

    println!(
        "Removed {name}{}. {} image(s) remain.",
        if was_in_playlist { "" } else { " (was not in playlist)" },
        gallery.media.len()
    );
    Ok(())
}

/// Delete all of *our* uploads (keeps foreign files) and empty the playlist.
pub fn gallery_clear(port_path: &str, gallery_path: &Path) -> Result<()> {
    let ours: Vec<String> = gallery::list_device_media()?
        .into_iter()
        .filter(|f| gallery::is_our_upload(f))
        .collect();

    let mut port = open_port(port_path)?;
    let mut dec = FrameDecoder::new();
    data::send_state_command(&mut port, "all", &serde_json::to_value(SysInfo::get_sysinfo())?)?;
    drain_replies(&mut port, &mut dec, Duration::from_millis(300))?;
    if !ours.is_empty() {
        data::send_command(
            &mut port,
            "mediaDelete",
            &serde_json::json!({ "type": "custom", "include": ours }),
        )?;
        drain_replies(&mut port, &mut dec, Duration::from_millis(300))?;
    }

    let mut gallery = Gallery::load(gallery_path)?;
    gallery.media.clear();
    gallery.save(gallery_path)?;
    // Empty playlist → device shows nothing; push so the screen updates.
    send_gallery_on(&mut port, &mut dec, &gallery)?;

    println!("Cleared {} upload(s); foreign files kept.", ours.len());
    Ok(())
}

/// Set the play mode ("Single" | "Loop" | "Shuffle") and re-apply.
pub fn gallery_mode(port_path: &str, mode: &str, gallery_path: &Path) -> Result<()> {
    anyhow::ensure!(
        matches!(mode, "Single" | "Loop" | "Shuffle"),
        "play mode must be Single, Loop, or Shuffle (got {mode:?})"
    );
    let mut gallery = Gallery::load(gallery_path)?;
    gallery.play_mode = mode.to_string();
    gallery.save(gallery_path)?;
    apply_gallery(port_path, gallery_path)?;
    println!("Play mode set to {mode}.");
    Ok(())
}

// ============================================================================
// Device controls (forwarded to the HomeUI app on the device)
//
// All are POST cmdTypes that get a bare 200 ACK. JSON shapes and enums are from
// docs/homeui-protocol.md (decompiled MsgReceiverManager + wire captures).
// ============================================================================

/// Open the port, flush stale input, send one POST command, print replies.
fn post_json(
    port_path: &str,
    cmd_type: &str,
    value: serde_json::Value,
    wait_secs: u64,
) -> Result<()> {
    let mut port = open_port(port_path)?;
    let mut dec = FrameDecoder::new();
    // Drain anything stale so the reply we print correlates with our command
    pump_incoming(&mut port, &mut dec, false)?;
    data::send_command(&mut port, cmd_type, &value)?;
    drain_replies(&mut port, &mut dec, Duration::from_secs(wait_secs))?;
    Ok(())
}

/// brightness: 0-100 percent (device maps to 0-250 internally)
pub fn brightness(port_path: &str, value: u8, wait_secs: u64) -> Result<()> {
    anyhow::ensure!(value <= 100, "brightness is a percentage (0-100)");
    post_json(port_path, "brightness", serde_json::json!({ "value": value }), wait_secs)
}

/// waterBlockScreen: display panel on/off (off = device blanks via brightness 0)
pub fn screen_power(port_path: &str, on: bool, wait_secs: u64) -> Result<()> {
    post_json(port_path, "waterBlockScreen", serde_json::json!({ "enable": on }), wait_secs)
}

/// displayInSleep: whether the panel shows standby video while the PC sleeps
pub fn display_in_sleep(port_path: &str, on: bool, wait_secs: u64) -> Result<()> {
    post_json(port_path, "displayInSleep", serde_json::json!({ "enable": on }), wait_secs)
}

/// power: screen-off/on event. Valid: suspend|shutdown|lock-screen|resume|unlock-screen
/// (never actually powers Android off — just the panel).
pub fn power(port_path: &str, event: &str, wait_secs: u64) -> Result<()> {
    post_json(port_path, "power", serde_json::json!({ "event": event }), wait_secs)
}

/// temperature: display unit. "Celsius" or "Fahrenheit". Telemetry stays °C;
/// the device converts for display (and, if Fahrenheit, for the fan curve — so
/// keep this Celsius unless you also express the fan curve in °F).
pub fn temperature(port_path: &str, unit: &str, wait_secs: u64) -> Result<()> {
    post_json(port_path, "temperature", serde_json::json!({ "value": unit }), wait_secs)
}

/// rotate: sets persist.vendor.orientation. Applies on display re-init/reboot.
pub fn rotate(port_path: &str, degree: i32, wait_secs: u64) -> Result<()> {
    post_json(port_path, "rotate", serde_json::json!({ "degree": degree }), wait_secs)
}

/// disconn: graceful screen-off; the link stays up and any frame restores it.
pub fn disconn(port_path: &str, wait_secs: u64) -> Result<()> {
    post_json(port_path, "disconn", serde_json::json!({}), wait_secs)
}

/// spec: set the CPU/GPU badge title strings. Auto-detects from this machine
/// when not given. Badge background auto-colors on the vendor substring
/// (Intel→blue, NVIDIA→green, else red).
pub fn spec(
    port_path: &str,
    cpu: Option<String>,
    gpu: Option<String>,
    wait_secs: u64,
) -> Result<()> {
    let cpu = cpu.unwrap_or_else(detect_cpu_name);
    let gpu = gpu.unwrap_or_else(detect_gpu_name);
    log::info!("spec: cpu={cpu:?} gpu={gpu:?}");
    post_json(port_path, "spec", serde_json::json!({ "cpu": cpu, "gpu": gpu }), wait_secs)
}

/// The 13 overlay metrics HomeUI recognizes (case-sensitive).
pub const SYSINFO_METRICS: &[&str] = &[
    "CPU Temperature",
    "GPU Temperature",
    "CPU Frequency",
    "GPU Frequency",
    "CPU Usage",
    "GPU Usage",
    "CPU Voltage",
    "GPU Voltage",
    "Motherboard Temperature",
    "Hard Disk Temperature",
    "Memory Frequency",
    "Memory Utilization",
    "Date&Time",
];

/// sysinfoDisplay: replace the overlay metric list (single-screen flat form).
pub fn sysinfo_display(port_path: &str, items: &[String], wait_secs: u64) -> Result<()> {
    for it in items {
        if !SYSINFO_METRICS.contains(&it.as_str()) {
            log::warn!(
                "'{it}' is not a recognized metric (device will ignore it). Valid: {}",
                SYSINFO_METRICS.join(", ")
            );
        }
    }
    post_json(port_path, "sysinfoDisplay", serde_json::json!({ "items": items }), wait_secs)
}

/// fanLCDSet (Smart Mode): temperature→duty curve.
///
/// Points are `[[tempC, duty%], …]`. The device's interpolation has two
/// bytecode-confirmed quirks: the *last* point is never used, and above the
/// second-to-last point it writes duty 0. Unless `raw`, we append a ceiling
/// sentinel so your real top point holds instead of dropping to 0 at high temps.
pub fn fan_smart(
    port_path: &str,
    mut points: Vec<(i32, i32)>,
    raw: bool,
    wait_secs: u64,
) -> Result<()> {
    anyhow::ensure!(!points.is_empty(), "curve needs at least one temp:duty point");
    points.sort_by_key(|p| p.0);
    for (_, d) in &points {
        anyhow::ensure!((0..=100).contains(d), "duty must be 0-100, got {d}");
    }
    if !raw {
        let (last_t, last_d) = *points.last().unwrap();
        let sentinel_t = (last_t + 30).max(130);
        points.push((sentinel_t, last_d));
        log::info!(
            "Appended ceiling sentinel [{sentinel_t},{last_d}] so {last_d}% holds above {last_t}°C \
             (device quirk workaround; use --raw to disable)"
        );
    }
    let smart: Vec<Vec<i32>> = points.iter().map(|(t, d)| vec![*t, *d]).collect();
    let body = serde_json::json!({
        "speed": "Mid Speed",
        "fixedMode": 45,
        "mode": "Smart Mode",
        "smartMode": smart,
    });
    log::info!("fanLCDSet smart curve: {}", serde_json::to_string(&smart)?);
    post_json(port_path, "fanLCDSet", body, wait_secs)
}

/// fanLCDSet (Fixed Mode): constant duty percent.
pub fn fan_fixed(port_path: &str, duty: u8, wait_secs: u64) -> Result<()> {
    anyhow::ensure!(duty <= 100, "duty is a percentage (0-100)");
    let body = serde_json::json!({
        "speed": "Mid Speed",
        "fixedMode": duty,
        "mode": "Fixed Mode",
        "smartMode": serde_json::Value::Null,
    });
    post_json(port_path, "fanLCDSet", body, wait_secs)
}

/// Parse "t1:d1,t2:d2,…" into curve points.
pub fn parse_curve(s: &str) -> Result<Vec<(i32, i32)>> {
    s.split(',')
        .map(|pair| {
            let (t, d) = pair
                .split_once(':')
                .with_context(|| format!("bad curve point '{pair}', expected temp:duty"))?;
            Ok((
                t.trim().parse().with_context(|| format!("bad temp in '{pair}'"))?,
                d.trim().parse().with_context(|| format!("bad duty in '{pair}'"))?,
            ))
        })
        .collect()
}

pub(crate) fn detect_cpu_name() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|c| {
            c.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "CPU".to_string())
}

pub(crate) fn detect_gpu_name() -> String {
    // Best-effort from lspci's VGA/3D controller line, cleaned up for badge use.
    if let Ok(out) = Command::new("lspci").output() {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = text
                .lines()
                .find(|l| l.contains("VGA compatible controller") || l.contains("3D controller"))
            {
                if let Some(desc) = line.splitn(2, ": ").nth(1) {
                    return clean_gpu_name(desc.trim());
                }
            }
        }
    }
    "GPU".to_string()
}

/// Trim lspci noise so the badge reads cleanly, while keeping the vendor word
/// (the device colors the badge by matching "NVIDIA"/"Intel"/else-AMD).
fn clean_gpu_name(raw: &str) -> String {
    let mut s = raw.to_string();
    // Drop a trailing "(rev xx)"
    if let Some(i) = s.find(" (rev") {
        s.truncate(i);
    }
    // Prefer the marketing name inside the last [...] if present
    if let (Some(open), Some(close)) = (s.rfind('['), s.rfind(']')) {
        if close > open {
            let inner = s[open + 1..close].trim();
            // First alternative of a "A/B/C" marketing list
            let first = inner.split('/').next().unwrap_or(inner).trim();
            let vendor = if raw.contains("NVIDIA") {
                "NVIDIA "
            } else if raw.contains("Intel") {
                "Intel "
            } else {
                "AMD "
            };
            if !first.is_empty() {
                return format!("{vendor}{first}");
            }
        }
    }
    s.trim().to_string()
}
