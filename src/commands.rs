// ============================================================================
// CLI subcommand implementations (headless operation)
// ============================================================================

use std::{
    io::Read,
    path::PathBuf,
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
use crate::screen_setup::{AioCoolerController, ScreenConfig};
use crate::sysinfo::{SysInfo, SysInfoReader};

const TRYX_VID: u16 = 0x18d1;
const TRYX_PID: u16 = 0x2d03;

pub fn open_port(path: &str) -> Result<Box<dyn serialport::SerialPort>> {
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
/// cross the serial link) → `transported` (commit) → `mediaDelete` (prune) →
/// `waterBlockScreenId` (configure) → 1 Hz sysinfo keepalives.
pub fn image(port_path: &str, path: &PathBuf, config: &ScreenConfig, keep_media: bool) -> Result<()> {
    anyhow::ensure!(path.is_file(), "No such file: {}", path.display());

    let controller = AioCoolerController::new(port_path);
    let file_md5 = AioCoolerController::calculate_md5(path)?;
    let file_size = std::fs::metadata(path)?.len();
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    let remote_name = AioCoolerController::generate_filename(extension);

    log::info!(
        "File: {} ({} bytes, MD5 {}) → {}",
        path.display(),
        file_size,
        file_md5,
        remote_name
    );

    let mut port = open_port(port_path)?;
    let mut dec = FrameDecoder::new();

    // Establish traffic (device resets its port after 10s of RX silence)
    data::send_state_command(&mut port, "all", &serde_json::to_value(SysInfo::get_sysinfo())?)?;
    drain_replies(&mut port, &mut dec, Duration::from_millis(300))?;

    // 1. Announce the transfer (device opens/truncates the target file)
    data::send_command(
        &mut port,
        "transport",
        &serde_json::json!({
            "type": "media",
            "fileSize": file_size,
            "fileName": remote_name
        }),
    )?;
    drain_replies(&mut port, &mut dec, Duration::from_millis(500))?;

    // 2. Actual bytes go over ADB (the Windows app does the same — its serial
    //    md5 is literally the string "todo"; we send the real one, also unchecked)
    controller.adb_push(path, &remote_name)?;

    // 3. Commit
    data::send_command(
        &mut port,
        "transported",
        &serde_json::json!({ "md5": file_md5, "fileName": remote_name }),
    )?;
    drain_replies(&mut port, &mut dec, Duration::from_millis(500))?;

    // 4. Prune old media, 5. configure the screen (the app always pairs these)
    if keep_media {
        log::info!("--keep-media: skipping mediaDelete");
    } else {
        data::send_command(
            &mut port,
            "mediaDelete",
            &serde_json::json!({ "type": "custom", "exclude": [remote_name] }),
        )?;
        drain_replies(&mut port, &mut dec, Duration::from_millis(300))?;
    }

    data::send_command(
        &mut port,
        "waterBlockScreenId",
        &config.to_water_block_json(&[remote_name.clone()]),
    )?;
    drain_replies(&mut port, &mut dec, Duration::from_millis(500))?;

    // 6. A few sysinfo ticks so temps populate and the link stays up
    for _ in 0..5 {
        thread::sleep(Duration::from_millis(1000));
        data::send_state_command(&mut port, "all", &serde_json::to_value(SysInfo::get_sysinfo())?)?;
        drain_replies(&mut port, &mut dec, Duration::from_millis(200))?;
    }

    println!("Transfer complete: {remote_name}");
    Ok(())
}

pub fn screen(port_path: &str, media: &[String], config: &ScreenConfig) -> Result<()> {
    let controller = AioCoolerController::new(port_path);
    controller.send_screen_config(media, config)?;
    println!("Screen configuration sent.");
    Ok(())
}

// ============================================================================
// Simple device controls (forwarded to the HomeUI app on the device)
// ============================================================================

/// brightness: 0-100 percent (device maps to 0-250 internally)
pub fn brightness(port_path: &str, value: u8, wait_secs: u64) -> Result<()> {
    anyhow::ensure!(value <= 100, "brightness is a percentage (0-100)");
    send(
        port_path,
        "POST",
        "brightness",
        &serde_json::json!({ "value": value }).to_string(),
        wait_secs,
    )
}

/// waterBlockScreen: display panel on/off (off = device blanks via brightness 0)
pub fn screen_power(port_path: &str, on: bool, wait_secs: u64) -> Result<()> {
    send(
        port_path,
        "POST",
        "waterBlockScreen",
        &serde_json::json!({ "enable": on }).to_string(),
        wait_secs,
    )
}

/// displayInSleep: whether the panel stays on while the PC sleeps
pub fn display_in_sleep(port_path: &str, on: bool, wait_secs: u64) -> Result<()> {
    send(
        port_path,
        "POST",
        "displayInSleep",
        &serde_json::json!({ "enable": on }).to_string(),
        wait_secs,
    )
}
