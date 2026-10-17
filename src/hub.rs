// ============================================================================
// Daemon serial hub — the one process that holds the cooler's serial port.
//
// The tty is exclusive (serialport opens it with TIOCEXCL + flock), and the
// panel blanks unless something keeps streaming `STATE all` (HomeUI's 60 s
// watchdog). So the daemon owns the port for good, and every other tool — CLI,
// GUI, `bridge` — reaches the device *through* it over a Unix socket instead of
// opening the port itself (`commands::open_port` finds the socket on its own).
//
// Clients speak the device protocol unchanged: framed requests in, framed
// replies out, exactly as on the tty. The hub
// - re-sequences every request into the link's one SeqNumber space and routes
//   each reply back to its sender by AckNumber, restoring the sender's own ack,
//   so a client never sees another client's (or the daemon's) replies;
// - spaces device writes `MIN_FRAME_GAP` apart (the device drops a read that
//   holds more than two frames, and it reads every 50 ms);
// - is the only sysinfo source: it streams at the configured interval and
//   answers clients' own `all` frames itself, so a remote client never mixes its
//   machine's stats into the display;
// - records the last value of each state-setting command it relays and replays
//   them once the device answers again after a reconnect (reboot, USB reset),
//   so a setting made from any client survives — not only the gallery file.
//
// Private cmdTypes under `hub.` are answered here and never reach the device:
// `hub.monitor` subscribes a client to a copy of every device frame (`listen`).
// A request the hub cannot deliver is answered `1 503` with `Source=tryx-daemon`
// and an `{"error": …}` body, which `commands` surfaces as an error.
// ============================================================================

use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::data::{self, CommandMessage, FrameDecoder, ParsedMessage};
use crate::gallery::Gallery;
use crate::sysinfo::SysInfo;

/// Where the systemd unit's daemon listens (`RuntimeDirectory=tryx-panorama`).
pub const SYSTEM_SOCKET: &str = "/run/tryx-panorama/tryx.sock";
/// Subscribes the sending client to a copy of every device frame.
pub const MONITOR_CMD: &str = "hub.monitor";
/// `Source=` header value on replies the hub writes itself.
pub const SOURCE: &str = "tryx-daemon";

/// Minimum spacing between frames written to the device.
const MIN_FRAME_GAP: Duration = Duration::from_millis(120);
/// How long a relayed request's route waits for the device's reply.
const REPLY_TTL: Duration = Duration::from_secs(10);
/// Restore even if the device never answers; it ignores input ~5 s after boot.
const RESTORE_FALLBACK: Duration = Duration::from_secs(8);
/// A client that stops reading is dropped rather than stalling the hub.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How often blocked threads re-check for shutdown.
const POLL: Duration = Duration::from_millis(200);

/// Commands whose last value *is* device state, so replaying it restores state.
const RESTORABLE: &[&str] = &[
    "waterBlockScreenId",
    "preset",
    "sysinfoDisplay",
    "brightness",
    "displayInSleep",
    "temperature",
    "spec",
    "fanLCDSet",
    "fanLCD",
    "turboPump",
    "config",
];
/// The restorable commands that make up the display layout.
const DISPLAY: &[&str] = &["waterBlockScreenId", "preset", "sysinfoDisplay"];

// ---- Socket discovery ------------------------------------------------------

static SOCKET_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();
static DIRECT: AtomicBool = AtomicBool::new(false);
static ROUTED_NOTED: AtomicBool = AtomicBool::new(false);

/// Use `path` as the daemon socket (`--socket`), for clients and daemon alike.
pub fn set_socket_override(path: PathBuf) {
    let _ = SOCKET_OVERRIDE.set(path);
}

/// Never route through a daemon (`--direct`).
pub fn set_direct(direct: bool) {
    DIRECT.store(direct, Ordering::Relaxed);
}

fn direct() -> bool {
    DIRECT.load(Ordering::Relaxed)
        || std::env::var("TRYX_DIRECT")
            .is_ok_and(|v| !matches!(v.trim(), "" | "0" | "false" | "no" | "off"))
}

/// Where a daemon may listen, most preferred first: `--socket` or
/// `$TRYX_SOCKET` alone if set, else the system socket, then the per-user one.
fn socket_candidates() -> Vec<PathBuf> {
    if let Some(p) = SOCKET_OVERRIDE.get() {
        return vec![p.clone()];
    }
    if let Ok(p) = std::env::var("TRYX_SOCKET")
        && !p.is_empty()
    {
        return vec![PathBuf::from(p)];
    }
    let mut out = vec![PathBuf::from(SYSTEM_SOCKET)];
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        out.push(Path::new(&dir).join("tryx-panorama.sock"));
    }
    out
}

/// Connect to a running daemon unless routing is off. Returns the stream and
/// the socket it came from.
pub fn connect() -> Option<(UnixStream, PathBuf)> {
    if direct() {
        return None;
    }
    socket_candidates()
        .into_iter()
        .find_map(|p| UnixStream::connect(&p).ok().map(|s| (s, p)))
}

/// Socket of the daemon commands would route through, if one is running.
pub fn running_daemon() -> Option<PathBuf> {
    connect().map(|(_, p)| p)
}

/// Note that commands go through the daemon: once per process at info level.
pub fn note_routed(sock: &Path) {
    if !ROUTED_NOTED.swap(true, Ordering::Relaxed) {
        log::info!(
            "Routing through the tryx daemon at {} (it owns the serial port)",
            sock.display()
        );
    } else {
        log::debug!("Routing through the tryx daemon at {}", sock.display());
    }
}

/// The bound socket file; removed when dropped (daemon shutdown).
pub struct SocketFile(PathBuf);

impl SocketFile {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Bind the daemon socket at the first usable candidate, shared with the
/// serial device's group (see [`share_with_device_group`]).
pub fn bind(device: &Path) -> Result<(UnixListener, SocketFile)> {
    let mut errors = Vec::new();
    for path in socket_candidates() {
        match bind_at(&path) {
            Ok(listener) => {
                share_with_device_group(&path, device);
                return Ok((listener, SocketFile(path)));
            }
            Err(e) => errors.push(format!("{}: {e:#}", path.display())),
        }
    }
    anyhow::bail!("Could not bind a daemon socket:\n  {}", errors.join("\n  "))
}

/// Bind `path`, refusing if a daemon already answers there and clearing a
/// stale socket file a crashed daemon left behind.
fn bind_at(path: &Path) -> Result<UnixListener> {
    if UnixStream::connect(path).is_ok() {
        anyhow::bail!("another tryx daemon is already listening there");
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => log::debug!("Removed stale socket {}", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("removing a stale socket"),
    }
    UnixListener::bind(path).context("bind")
}

/// Mode 0660 with the serial device node's group, so whoever may open the tty
/// may talk to the daemon instead. Best effort; re-run whenever the device
/// (re)appears, since its node is absent while unplugged.
pub fn share_with_device_group(sock: &Path, device: &Path) {
    if let Err(e) = std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o660)) {
        log::warn!("chmod {}: {e}", sock.display());
    }
    if let Ok(meta) = std::fs::metadata(device)
        && let Err(e) = std::os::unix::fs::chown(sock, None, Some(meta.gid()))
    {
        log::warn!(
            "Could not give {} the group of {} ({e}); only its owner can use the daemon",
            sock.display(),
            device.display()
        );
    }
}

// ---- Hub -------------------------------------------------------------------

pub struct Options {
    /// Sysinfo cadence.
    pub interval: Duration,
    /// Wait between attempts to (re)open the device.
    pub reconnect_delay: Duration,
    /// Send `conn` and log the device identity on each (re)connect.
    pub conn: bool,
    /// Suppress the periodic status line.
    pub quiet: bool,
    /// Print the status line every N sysinfo ticks (0 = never).
    pub status_every: u64,
    /// Gallery re-applied on reconnect when it is newer than the last layout a
    /// client set through the hub.
    pub gallery_path: PathBuf,
    /// Restore the display layout on reconnect (`--no-gallery` turns it off).
    pub restore_display: bool,
    /// Where relayed settings persist across daemon restarts.
    pub state_path: Option<PathBuf>,
}

enum Event {
    /// A complete frame from the device, tagged with its link generation.
    Device(u64, Vec<u8>),
    /// The device read side of that link generation failed.
    DeviceLost(u64, String),
    ClientUp(u64, UnixStream),
    ClientFrame(u64, Vec<u8>),
    ClientDown(u64),
}

/// Serve the device until `running` clears: stream sysinfo, relay clients
/// accepted on `listener`, and reopen the device via `open` whenever the link
/// drops. Every port `open` returns is driven from this thread plus one reader.
pub fn run(
    opts: Options,
    listener: UnixListener,
    mut open: impl FnMut() -> Result<Box<dyn serialport::SerialPort>>,
    mut sample: impl FnMut() -> SysInfo,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    {
        let (tx, running) = (tx.clone(), running.clone());
        thread::spawn(move || accept_clients(listener, tx, running));
    }

    let interval = opts.interval.max(Duration::from_millis(100));
    let reconnect_delay = opts.reconnect_delay;
    let mut hub = Hub::new(opts);
    let mut link: Option<Link> = None;
    let mut generation = 0u64;
    let mut failed_opens = 0u32;
    let mut next_open = Instant::now();
    let mut next_tick = Instant::now();

    while running.load(Ordering::SeqCst) {
        let now = Instant::now();
        if link.is_none() && now >= next_open {
            generation += 1;
            match open().and_then(|port| Link::start(port, generation, tx.clone())) {
                Ok(l) => {
                    log::info!("Connected to the cooler");
                    hub.on_connected();
                    link = Some(l);
                    failed_opens = 0;
                    next_tick = now;
                }
                Err(e) => {
                    if failed_opens == 0 {
                        log::warn!("Open failed: {e:#}. Retrying every {reconnect_delay:?}…");
                    } else {
                        log::debug!("Open failed again: {e:#}");
                    }
                    failed_opens += 1;
                    next_open = now + reconnect_delay;
                }
            }
        }

        if let Some(l) = link.as_mut() {
            if now >= next_tick {
                hub.queue_sysinfo(sample());
                next_tick += interval;
                if next_tick < now {
                    next_tick = now + interval;
                }
            }
            hub.maybe_restore(l);
            if let Err(e) = hub.flush(l) {
                log::warn!("Send failed ({e:#}); assuming disconnect, will reconnect");
                hub.on_lost();
                link = None;
                next_open = Instant::now() + reconnect_delay;
            }
        }
        hub.expire_routes();

        let mut wake = Instant::now() + POLL;
        if link.is_some() {
            wake = wake.min(next_tick);
            if let Some(t) = hub.next_send_at() {
                wake = wake.min(t);
            }
        } else {
            wake = wake.min(next_open);
        }
        match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok(Event::DeviceLost(g, why)) if g == generation && link.is_some() => {
                log::warn!("Read failed ({why}); assuming disconnect, will reconnect");
                hub.on_lost();
                link = None;
                next_open = Instant::now() + reconnect_delay;
            }
            Ok(Event::Device(g, payload)) => {
                if let Some(l) = link.as_mut().filter(|l| l.generation == g) {
                    hub.on_device_frame(l, &payload);
                }
            }
            Ok(Event::ClientFrame(id, payload)) => hub.on_client_frame(id, &payload, link.is_some()),
            Ok(Event::ClientUp(id, stream)) => {
                log::debug!("client #{id} connected");
                hub.clients.insert(id, Client { stream, monitor: false });
            }
            Ok(Event::ClientDown(id)) => hub.drop_client(id),
            Ok(Event::DeviceLost(..)) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    drop(link);
    log::info!("Daemon stopped.");
    Ok(())
}

/// Who a relayed frame was sent for, i.e. where its reply goes.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Origin {
    /// A client request; `ack` is the AckNumber the client expects back.
    Client { id: u64, ack: Option<i64> },
    Sysinfo,
    Conn,
    Restore,
}

struct Outgoing {
    method: String,
    cmd_type: String,
    body: String,
    file_name: Option<String>,
    file_size: Option<i64>,
    origin: Origin,
}

struct Route {
    origin: Origin,
    sent: Instant,
}

struct Client {
    stream: UnixStream,
    monitor: bool,
}

struct Hub {
    opts: Options,
    clients: HashMap<u64, Client>,
    queue: VecDeque<Outgoing>,
    /// Device AckNumber (our seq + 1) → who the reply belongs to.
    routes: HashMap<i64, Route>,
    last_write: Option<Instant>,
    status: DeviceStatus,
    restore: Restore,
    ticks: u64,
}

impl Hub {
    fn new(opts: Options) -> Self {
        let restore = Restore::load(opts.state_path.clone());
        Self {
            opts,
            clients: HashMap::new(),
            queue: VecDeque::new(),
            routes: HashMap::new(),
            last_write: None,
            status: DeviceStatus::default(),
            restore,
            ticks: 0,
        }
    }

    fn on_connected(&mut self) {
        self.routes.clear();
        self.last_write = None;
    }

    /// The link dropped: every request in flight will never be answered.
    fn on_lost(&mut self) {
        let waiting: Vec<Origin> = self
            .routes
            .drain()
            .map(|(_, r)| r.origin)
            .chain(self.queue.drain(..).map(|o| o.origin))
            .collect();
        for origin in waiting {
            if let Origin::Client { id, ack } = origin {
                self.reply_error(id, ack, "the cooler disconnected before replying");
            }
        }
    }

    fn drop_client(&mut self, id: u64) {
        if self.clients.remove(&id).is_some() {
            log::debug!("client #{id} disconnected");
        }
        let theirs = |o: &Origin| matches!(o, Origin::Client { id: c, .. } if *c == id);
        self.routes.retain(|_, r| !theirs(&r.origin));
        self.queue.retain(|o| !theirs(&o.origin));
    }

    fn expire_routes(&mut self) {
        self.routes.retain(|_, r| r.sent.elapsed() < REPLY_TTL);
    }

    fn push(&mut self, origin: Origin, method: &str, cmd_type: &str, body: String) {
        self.queue.push_back(Outgoing {
            method: method.to_string(),
            cmd_type: cmd_type.to_string(),
            body,
            file_name: None,
            file_size: None,
            origin,
        });
    }

    fn queue_sysinfo(&mut self, info: SysInfo) {
        if self.queue.iter().any(|o| o.origin == Origin::Sysinfo) {
            return; // still waiting for its write slot; don't pile up
        }
        match serde_json::to_string(&info) {
            Ok(body) => self.push(Origin::Sysinfo, "STATE", "all", body),
            Err(e) => log::error!("sysinfo serialize failed: {e}"),
        }
        self.ticks += 1;
        if !self.opts.quiet
            && self.opts.status_every > 0
            && self.ticks.is_multiple_of(self.opts.status_every)
        {
            let s = &self.status;
            println!(
                "[{}] sent CPU {}°C {}% {}W / GPU {}°C {}% {}W  │  device fanLCD={} pump={} free={:.2}GB  │  clients={}",
                chrono::Local::now().format("%H:%M:%S"),
                info.cpu.temperature,
                info.cpu.load,
                info.cpu.power,
                info.gpu.temperature,
                info.gpu.load,
                info.gpu.power,
                if s.fan_lcd.is_empty() { "?" } else { &s.fan_lcd },
                if s.turbo_pump.is_empty() { "?" } else { &s.turbo_pump },
                s.available_storage as f64 / 1e9,
                self.clients.len(),
            );
        }
    }

    /// Queue the reconnect handshake and restore once the device is past its
    /// boot gate: it answered something, or long enough has passed anyway.
    fn maybe_restore(&mut self, link: &mut Link) {
        if link.restored || !(link.answered || link.opened.elapsed() >= RESTORE_FALLBACK) {
            return;
        }
        link.restored = true;
        if self.opts.conn {
            self.push(Origin::Conn, "POST", "conn", "{}".to_string());
        }
        let gallery = self
            .opts
            .restore_display
            .then(|| gallery_layout(&self.opts.gallery_path))
            .flatten();
        let plan = self.restore.plan(gallery, self.opts.restore_display);
        if !plan.is_empty() {
            let names: Vec<&str> = plan.iter().map(|(cmd, _)| cmd.as_str()).collect();
            log::info!("Restoring {} setting(s): {}", plan.len(), names.join(", "));
        }
        for (cmd, body) in plan {
            self.push(Origin::Restore, "POST", &cmd, body);
        }
    }

    /// When the next queued frame may be written.
    fn next_send_at(&self) -> Option<Instant> {
        if self.queue.is_empty() {
            return None;
        }
        Some(self.last_write.map_or_else(Instant::now, |t| t + MIN_FRAME_GAP))
    }

    /// Write the next queued frame if its slot has come.
    fn flush(&mut self, link: &mut Link) -> Result<()> {
        if self.next_send_at().is_none_or(|t| t > Instant::now()) {
            return Ok(());
        }
        let Some(out) = self.queue.pop_front() else {
            return Ok(());
        };
        let mut msg = CommandMessage::new(&out.method, &out.cmd_type, &out.body);
        if let Some(name) = out.file_name {
            msg.file_name = name;
        }
        if let Some(size) = out.file_size {
            msg.file_size = size;
        }
        data::send_message(&mut link.port, &msg)?;
        self.last_write = Some(Instant::now());
        self.routes.insert(
            msg.seq_number + 1,
            Route { origin: out.origin, sent: Instant::now() },
        );
        Ok(())
    }

    fn on_client_frame(&mut self, id: u64, payload: &[u8], connected: bool) {
        let msg = match data::parse_message(payload) {
            Ok(m) if !m.is_reply() => m,
            _ => {
                log::warn!("client #{id}: dropped a frame that is not a request");
                return;
            }
        };
        let ack = msg.seq_number().map(|s| s + 1);
        match msg.cmd_type.as_str() {
            MONITOR_CMD => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.monitor = true;
                }
                self.reply(id, ack, 200, "");
                return;
            }
            other if other.starts_with("hub.") => {
                let body = serde_json::json!({ "error": format!("unknown hub command {other}") });
                self.reply(id, ack, 404, &body.to_string());
                return;
            }
            // The hub is the only sysinfo source: answer with the latest status
            // instead of letting a client's own stats reach the display.
            "all" => {
                let body = self.status.last_body.clone();
                self.reply(id, ack, 200, &body);
                return;
            }
            _ => {}
        }
        if !connected {
            self.reply_error(id, ack, "the cooler is not connected to the tryx daemon");
            return;
        }
        let body = msg.body.trim_end_matches(' ').to_string();
        if msg.method == "POST" {
            self.restore.record(&msg.cmd_type, &body);
        }
        self.queue.push_back(Outgoing {
            file_name: msg.headers.get("FileName").cloned(),
            file_size: msg.header_i64("FileSize"),
            method: msg.method,
            cmd_type: msg.cmd_type,
            body,
            origin: Origin::Client { id, ack },
        });
    }

    fn on_device_frame(&mut self, link: &mut Link, payload: &[u8]) {
        let msg = match data::parse_message(payload) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Unparseable device frame ({} bytes): {e}", payload.len());
                return;
            }
        };
        let mut routed_to = None;
        if msg.is_reply() {
            link.answered = true;
            let route = msg.ack_number().and_then(|a| self.routes.remove(&a));
            match route.map(|r| r.origin) {
                Some(Origin::Client { id, ack }) => {
                    let out = match ack {
                        Some(a) => data::with_ack_number(payload, a),
                        None => payload.to_vec(),
                    };
                    self.send_to(id, &out);
                    routed_to = Some(id);
                }
                Some(Origin::Conn) if !msg.body.trim().is_empty() => {
                    log::info!("Device: {}", msg.body.trim());
                }
                _ => self.status.update(&msg),
            }
        } else {
            log::debug!("Unsolicited {} {} from the device", msg.method, msg.cmd_type);
        }
        let monitors: Vec<u64> = self
            .clients
            .iter()
            .filter(|(id, c)| c.monitor && Some(**id) != routed_to)
            .map(|(id, _)| *id)
            .collect();
        for id in monitors {
            self.send_to(id, payload);
        }
    }

    fn send_to(&mut self, id: u64, payload: &[u8]) {
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        if let Err(e) = client.stream.write_all(&data::frame_payload(payload)) {
            log::debug!("client #{id}: write failed ({e}); dropping it");
            self.drop_client(id);
        }
    }

    /// Answer a client directly, as the device would but marked as ours.
    fn reply(&mut self, id: u64, ack: Option<i64>, code: u16, body: &str) {
        let mut head = format!("1 {code}\r\n");
        if let Some(a) = ack {
            head.push_str(&format!("AckNumber={a}\r\n"));
        }
        head.push_str(&format!(
            "ContentLength={}\r\nContentType=json\r\nSource={SOURCE}\r\n\r\n{body}",
            body.len()
        ));
        self.send_to(id, head.as_bytes());
    }

    fn reply_error(&mut self, id: u64, ack: Option<i64>, why: &str) {
        let body = serde_json::json!({ "error": why }).to_string();
        self.reply(id, ack, 503, &body);
    }
}

/// An open device connection: the write half, plus a thread reading a clone.
struct Link {
    port: Box<dyn serialport::SerialPort>,
    generation: u64,
    stop: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
    opened: Instant,
    /// The device has answered since this link opened, so it is past its boot gate.
    answered: bool,
    restored: bool,
}

impl Link {
    fn start(
        port: Box<dyn serialport::SerialPort>,
        generation: u64,
        tx: mpsc::Sender<Event>,
    ) -> Result<Self> {
        let mut rx_half = port.try_clone().context("cloning the serial port for reading")?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_reader = stop.clone();
        let reader = thread::spawn(move || {
            let mut dec = FrameDecoder::new();
            let mut buf = [0u8; 4096];
            let mut empty_reads = 0;
            while !stop_reader.load(Ordering::SeqCst) {
                match rx_half.read(&mut buf) {
                    // A socket at EOF reads 0 forever; a live tty times out instead.
                    Ok(0) => {
                        empty_reads += 1;
                        if empty_reads >= 3 {
                            let _ = tx.send(Event::DeviceLost(generation, "end of stream".into()));
                            return;
                        }
                    }
                    Ok(n) => {
                        empty_reads = 0;
                        dec.push(&buf[..n]);
                        while let Some(frame) = dec.next_frame() {
                            if tx.send(Event::Device(generation, frame)).is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) if is_timeout(&e) => empty_reads = 0,
                    Err(e) => {
                        let _ = tx.send(Event::DeviceLost(generation, e.to_string()));
                        return;
                    }
                }
            }
        });
        Ok(Self {
            port,
            generation,
            stop,
            reader: Some(reader),
            opened: Instant::now(),
            answered: false,
            restored: false,
        })
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

fn accept_clients(listener: UnixListener, tx: mpsc::Sender<Event>, running: Arc<AtomicBool>) {
    if let Err(e) = listener.set_nonblocking(true) {
        log::warn!("Daemon socket: {e}; shutdown may hang until the next client");
    }
    let mut next_id = 1u64;
    while running.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let id = next_id;
                next_id += 1;
                if let Err(e) = start_client(id, stream, &tx, &running) {
                    log::warn!("client #{id}: {e}");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                log::warn!("Daemon socket accept failed: {e}");
                thread::sleep(POLL);
            }
        }
    }
}

fn start_client(
    id: u64,
    stream: UnixStream,
    tx: &mpsc::Sender<Event>,
    running: &Arc<AtomicBool>,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
    stream.set_read_timeout(Some(POLL))?;
    let writer = stream.try_clone()?;
    if tx.send(Event::ClientUp(id, writer)).is_err() {
        return Ok(());
    }
    let (tx, running) = (tx.clone(), running.clone());
    thread::spawn(move || {
        let mut stream = stream;
        let mut dec = FrameDecoder::new();
        let mut buf = [0u8; 4096];
        while running.load(Ordering::SeqCst) {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    dec.push(&buf[..n]);
                    while let Some(frame) = dec.next_frame() {
                        if tx.send(Event::ClientFrame(id, frame)).is_err() {
                            return;
                        }
                    }
                }
                Err(e) if is_timeout(&e) => {}
                Err(_) => break,
            }
        }
        let _ = tx.send(Event::ClientDown(id));
    });
    Ok(())
}

// ---- Device status ---------------------------------------------------------

/// Latest device-reported status, parsed from `all` reply bodies.
#[derive(Default)]
struct DeviceStatus {
    fan_lcd: String,
    turbo_pump: String,
    available_storage: u64,
    /// The raw body, handed to clients whose own `all` the hub absorbs.
    last_body: String,
}

impl DeviceStatus {
    fn update(&mut self, msg: &ParsedMessage) {
        let Some(json) = msg.json() else {
            return;
        };
        if let Some(s) = json.get("status") {
            if let Some(v) = s.get("fanLCD").and_then(|v| v.as_str()) {
                self.fan_lcd = v.to_string();
            }
            if let Some(v) = s.get("turboPump").and_then(|v| v.as_str()) {
                self.turbo_pump = v.to_string();
            }
            self.last_body = msg.body.trim().to_string();
        }
        if let Some(v) = json.get("availableStorage").and_then(|v| v.as_u64()) {
            self.available_storage = v;
        }
    }
}

// ---- Restore ---------------------------------------------------------------

/// Last value of each restorable command relayed to the device, persisted so
/// it is replayed after a device reboot or a daemon restart.
#[derive(Default, Serialize, Deserialize)]
struct Restore {
    entries: Vec<RestoreEntry>,
    #[serde(skip)]
    path: Option<PathBuf>,
    #[serde(skip)]
    save_failed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct RestoreEntry {
    cmd: String,
    body: serde_json::Value,
    /// When it was set, epoch ms.
    at_ms: i64,
}

/// The gallery file's layout (`None` = empty playlist) and modification time.
struct GalleryLayout {
    body: Option<serde_json::Value>,
    at_ms: i64,
}

impl Restore {
    fn load(path: Option<PathBuf>) -> Self {
        let mut restore = path
            .as_deref()
            .and_then(|p| match std::fs::read(p) {
                Ok(bytes) => serde_json::from_slice::<Restore>(&bytes)
                    .map_err(|e| log::warn!("Ignoring unreadable {}: {e}", p.display()))
                    .ok(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => {
                    log::warn!("Ignoring unreadable {}: {e}", p.display());
                    None
                }
            })
            .unwrap_or_default();
        restore.path = path;
        restore
    }

    /// Remember `body` as the device's current `cmd` state, superseding the
    /// entries it overrides on the device.
    fn record(&mut self, cmd: &str, body: &str) {
        if !RESTORABLE.contains(&cmd) {
            return;
        }
        let Ok(body) = serde_json::from_str::<serde_json::Value>(body) else {
            return;
        };
        let supersedes = |old: &str| {
            old == cmd
                || match cmd {
                    // A full layout carries its own overlay list and settings.
                    "waterBlockScreenId" => old == "preset" || old == "sysinfoDisplay",
                    "fanLCDSet" => old == "fanLCD",
                    _ => false,
                }
        };
        self.entries.retain(|e| !supersedes(&e.cmd));
        self.entries.push(RestoreEntry { cmd: cmd.to_string(), body, at_ms: now_ms() });
        self.save();
    }

    /// What to replay, oldest first. With `display`, the gallery file stands in
    /// for the recorded layout when it was written more recently (CLI/GUI edits
    /// made while the daemon was down, or by hand).
    fn plan(&self, gallery: Option<GalleryLayout>, display: bool) -> Vec<(String, String)> {
        let is_display = |cmd: &str| DISPLAY.contains(&cmd);
        let mut entries: Vec<RestoreEntry> = self
            .entries
            .iter()
            .filter(|e| display || !is_display(&e.cmd))
            .cloned()
            .collect();
        if display && let Some(g) = gallery {
            let newest = entries.iter().filter(|e| is_display(&e.cmd)).map(|e| e.at_ms).max();
            if newest.is_none_or(|t| g.at_ms > t) {
                entries.retain(|e| !is_display(&e.cmd));
                if let Some(body) = g.body {
                    entries.push(RestoreEntry {
                        cmd: "waterBlockScreenId".into(),
                        body,
                        at_ms: g.at_ms,
                    });
                }
            }
        }
        entries.sort_by_key(|e| e.at_ms);
        entries.into_iter().map(|e| (e.cmd, e.body.to_string())).collect()
    }

    fn save(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let result = (|| -> Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&*self)?)?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        })();
        match result {
            Ok(()) => self.save_failed = false,
            Err(e) if !self.save_failed => {
                log::warn!("Could not save {} ({e:#}); settings restore from memory only", path.display());
                self.save_failed = true;
            }
            Err(_) => {}
        }
    }
}

fn gallery_layout(path: &Path) -> Option<GalleryLayout> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let at_ms = modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64;
    match Gallery::load(path) {
        Ok(g) => Some(GalleryLayout {
            body: (!g.media.is_empty()).then(|| g.layout_json()),
            at_ms,
        }),
        Err(e) => {
            log::warn!("gallery load failed ({}): {e:#}", path.display());
            None
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{open_port, pump_incoming};
    use std::sync::Mutex;

    fn entry(cmd: &str, at_ms: i64) -> RestoreEntry {
        RestoreEntry { cmd: cmd.into(), body: serde_json::json!({ "at": at_ms }), at_ms }
    }

    #[test]
    fn a_layout_supersedes_overlays_and_a_curve_its_subset_update() {
        let mut r = Restore::default();
        r.record("sysinfoDisplay", r#"{"items":["CPU Usage"]}"#);
        r.record("fanLCD", r#"{"speed":"Mid Speed","mode":"Smart Mode"}"#);
        r.record("brightness", r#"{"value":40}"#);
        r.record("waterBlockScreenId", r#"{"id":"Customization","media":["a.png"]}"#);
        r.record("fanLCDSet", r#"{"mode":"Fixed Mode","fixedMode":50}"#);
        r.record("brightness", r#"{"value":70}"#);
        r.record("power", r#"{"event":"suspend"}"#); // not state: never replayed
        r.record("spec", "not json");
        let cmds: Vec<&str> = r.entries.iter().map(|e| e.cmd.as_str()).collect();
        assert_eq!(cmds, ["waterBlockScreenId", "fanLCDSet", "brightness"]);
        assert_eq!(r.entries[2].body["value"], 70);
    }

    #[test]
    fn the_newer_of_gallery_file_and_recorded_layout_wins() {
        let r = Restore {
            entries: vec![entry("fanLCDSet", 100), entry("waterBlockScreenId", 200), entry("brightness", 300)],
            ..Default::default()
        };
        let gallery = |at_ms| Some(GalleryLayout { body: Some(serde_json::json!({ "gallery": at_ms })), at_ms });

        let recorded: Vec<String> = r.plan(gallery(150), true).into_iter().map(|(c, _)| c).collect();
        assert_eq!(recorded, ["fanLCDSet", "waterBlockScreenId", "brightness"]);
        assert!(r.plan(gallery(150), true)[1].1.contains("\"at\":200"));

        let from_file = r.plan(gallery(250), true);
        let cmds: Vec<&str> = from_file.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(cmds, ["fanLCDSet", "waterBlockScreenId", "brightness"]);
        assert!(from_file[1].1.contains("\"gallery\":250"));

        // A newer, emptied gallery means "show nothing": no layout replayed.
        let emptied = r.plan(Some(GalleryLayout { body: None, at_ms: 400 }), true);
        assert!(emptied.iter().all(|(c, _)| c != "waterBlockScreenId"));

        let no_display: Vec<String> = r.plan(gallery(999), false).into_iter().map(|(c, _)| c).collect();
        assert_eq!(no_display, ["fanLCDSet", "brightness"]);
    }

    #[test]
    fn restore_state_round_trips_through_its_file() {
        let path = std::env::temp_dir().join(format!("tryx-restore-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut r = Restore::load(Some(path.clone()));
        r.record("brightness", r#"{"value":55}"#);
        let back = Restore::load(Some(path.clone()));
        assert_eq!(back.entries, r.entries);
        let _ = std::fs::remove_file(&path);
    }

    /// Plays the cooler on the far end of a socket pair: answers every request
    /// with AckNumber = seq + 1 (a status body for `all`), logging cmdTypes.
    fn fake_device(mut stream: UnixStream, seen: Arc<Mutex<Vec<String>>>) {
        let mut dec = FrameDecoder::new();
        let mut buf = [0u8; 4096];
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        loop {
            match stream.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => dec.push(&buf[..n]),
                Err(e) if is_timeout(&e) => continue,
                Err(_) => return,
            }
            while let Some(frame) = dec.next_frame() {
                let msg = data::parse_message(&frame).unwrap();
                seen.lock().unwrap().push(format!("{} {}", msg.method, msg.cmd_type));
                let body = if msg.cmd_type == "all" {
                    r#"{"status":{"fanLCD":"2010","turboPump":"1410"},"availableStorage":1}"#
                } else {
                    ""
                };
                let reply = format!(
                    "1 200\r\nAckNumber={}\r\nContentLength={}\r\nContentType=json\r\n\r\n{body}",
                    msg.seq_number().unwrap() + 1,
                    body.len()
                );
                if stream.write_all(&data::frame_payload(reply.as_bytes())).is_err() {
                    return;
                }
            }
        }
    }

    #[test]
    fn clients_get_their_own_replies_while_the_hub_keeps_streaming() {
        let dir = std::env::temp_dir().join(format!("tryx-hub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("hub.sock");
        let listener = bind_at(&sock).unwrap();
        let (hub_end, device_end) = UnixStream::pair().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            thread::spawn(move || fake_device(device_end, seen));
        }

        let running = Arc::new(AtomicBool::new(true));
        let hub = {
            let running = running.clone();
            let opts = Options {
                interval: Duration::from_millis(150),
                reconnect_delay: Duration::from_millis(100),
                conn: false,
                quiet: true,
                status_every: 0,
                gallery_path: dir.join("no-gallery.json"),
                restore_display: true,
                state_path: None,
            };
            let mut device = Some(hub_end);
            thread::spawn(move || {
                run(
                    opts,
                    listener,
                    move || {
                        let s = device.take().context("device already handed out")?;
                        let port = crate::commands::NetSerial::new(
                            crate::commands::NetStream::Unix(s),
                            "fake".into(),
                        )?;
                        Ok(Box::new(port) as Box<dyn serialport::SerialPort>)
                    },
                    crate::sysinfo::SysInfo::fixture,
                    running,
                )
            })
        };

        let mut client = open_port(&format!("unix://{}", sock.display())).unwrap();
        let mut dec = FrameDecoder::new();
        let msg = CommandMessage::new("POST", "brightness", r#"{"value":50}"#);
        let expect_ack = msg.seq_number + 1;
        data::send_message(&mut client, &msg).unwrap();

        // The client's own `all` is absorbed: answered by the hub, never relayed.
        let all = CommandMessage::new("STATE", "all", "{}");
        let all_ack = all.seq_number + 1;
        data::send_message(&mut client, &all).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut replies = Vec::new();
        while Instant::now() < deadline && replies.len() < 2 {
            replies.extend(pump_incoming(&mut client, &mut dec, false).unwrap());
        }
        // Several sysinfo ticks later, still nothing but our own two replies.
        let quiet_until = Instant::now() + Duration::from_millis(600);
        while Instant::now() < quiet_until {
            replies.extend(pump_incoming(&mut client, &mut dec, false).unwrap());
        }
        running.store(false, Ordering::SeqCst);
        hub.join().unwrap().unwrap();

        let acks: Vec<Option<i64>> = replies.iter().map(|m| m.ack_number()).collect();
        assert!(acks.contains(&Some(expect_ack)), "brightness reply routed back: {acks:?}");
        assert!(acks.contains(&Some(all_ack)), "own `all` answered by the hub: {acks:?}");
        assert_eq!(replies.len(), 2, "no daemon sysinfo replies leaked to the client");

        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|c| c == "POST brightness"), "{seen:?}");
        assert!(seen.iter().any(|c| c == "STATE all"), "hub streamed sysinfo: {seen:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_relayed_before_a_reconnect_are_replayed_after_it() {
        let dir = std::env::temp_dir().join(format!("tryx-hub-replug-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("hub.sock");
        let listener = bind_at(&sock).unwrap();

        let (hub_a, dev_a) = UnixStream::pair().unwrap();
        let (hub_b, dev_b) = UnixStream::pair().unwrap();
        let unplug = dev_a.try_clone().unwrap();
        let (seen_a, seen_b) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
        {
            let (a, b) = (seen_a.clone(), seen_b.clone());
            thread::spawn(move || fake_device(dev_a, a));
            thread::spawn(move || fake_device(dev_b, b));
        }

        let running = Arc::new(AtomicBool::new(true));
        let hub = {
            let running = running.clone();
            let opts = Options {
                interval: Duration::from_millis(150),
                reconnect_delay: Duration::from_millis(100),
                conn: false,
                quiet: true,
                status_every: 0,
                gallery_path: dir.join("no-gallery.json"),
                restore_display: true,
                state_path: None,
            };
            let mut devices = vec![hub_b, hub_a];
            thread::spawn(move || {
                run(
                    opts,
                    listener,
                    move || {
                        let s = devices.pop().context("no more devices")?;
                        let port = crate::commands::NetSerial::new(
                            crate::commands::NetStream::Unix(s),
                            "fake".into(),
                        )?;
                        Ok(Box::new(port) as Box<dyn serialport::SerialPort>)
                    },
                    crate::sysinfo::SysInfo::fixture,
                    running,
                )
            })
        };

        let mut client = open_port(&format!("unix://{}", sock.display())).unwrap();
        let mut dec = FrameDecoder::new();
        data::send_command(&mut client, "brightness", &serde_json::json!({ "value": 42 })).unwrap();
        let layout = serde_json::json!({ "id": "Customization", "media": ["a.png"] });
        data::send_command(&mut client, "waterBlockScreenId", &layout).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut replies = 0;
        while Instant::now() < deadline && replies < 2 {
            replies += pump_incoming(&mut client, &mut dec, false).unwrap().len();
        }
        assert_eq!(replies, 2, "both settings acknowledged before the unplug");

        unplug.shutdown(std::net::Shutdown::Both).unwrap();
        let wanted = ["POST brightness", "POST waterBlockScreenId"];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let seen = seen_b.lock().unwrap().clone();
            if wanted.iter().all(|w| seen.iter().any(|s| s == w)) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        running.store(false, Ordering::SeqCst);
        hub.join().unwrap().unwrap();

        let seen = seen_b.lock().unwrap();
        for w in wanted {
            assert!(seen.iter().any(|s| s == w), "{w} replayed on the new link: {seen:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requests_fail_fast_while_the_cooler_is_unplugged() {
        let dir = std::env::temp_dir().join(format!("tryx-hub-down-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("hub.sock");
        let listener = bind_at(&sock).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let hub = {
            let running = running.clone();
            let opts = Options {
                interval: Duration::from_millis(150),
                reconnect_delay: Duration::from_secs(60),
                conn: false,
                quiet: true,
                status_every: 0,
                gallery_path: dir.join("no-gallery.json"),
                restore_display: true,
                state_path: None,
            };
            thread::spawn(move || {
                run(
                    opts,
                    listener,
                    || anyhow::bail!("no such device"),
                    crate::sysinfo::SysInfo::fixture,
                    running,
                )
            })
        };

        let mut client = open_port(&format!("unix://{}", sock.display())).unwrap();
        let mut dec = FrameDecoder::new();
        data::send_command(&mut client, "brightness", &serde_json::json!({ "value": 1 })).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let err = loop {
            match pump_incoming(&mut client, &mut dec, false) {
                Err(e) => break e,
                Ok(_) if Instant::now() < deadline => continue,
                Ok(_) => panic!("no error reply from the hub"),
            }
        };
        running.store(false, Ordering::SeqCst);
        hub.join().unwrap().unwrap();
        assert!(format!("{err:#}").contains("not connected"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
