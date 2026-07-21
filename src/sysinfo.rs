// System information reader for Linux
//
// Produces the `all` sysinfo payload the Tryx Panorama display renders. Field
// names, types and units match what the device's HomeUI app parses (verified
// against the decompiled PcInfo/PcCpu/PcGpu/... entities and wire captures):
//
//   memory.total/used  MB      cpu.speedAverage  MHz     disk.total/used   GB
//   cpu/gpu.power      W        *.temperature     °C      disk.read/writeSpeed KB/s
//   *.voltage          V        fans[].value      RPM     network.up/download  KB/s
//   disk.load = used%           disk.activity = IO-busy %
//
// Rate fields (cpu.load, network, disk IO, cpu.power) are deltas between
// successive samples, so a `SysInfoReader` is stateful — construct once, call
// `sample()` each tick. Sensor paths are discovered once and cached; discovery
// is generic across AMD (k10temp/amdgpu), Intel (coretemp/RAPL) and NVIDIA.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// System info payload matching the device's PcInfo schema.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SysInfo {
    pub network: NetworkInfo,
    pub memory: MemoryInfo,
    pub cpu: CpuInfo,
    pub gpu: GpuInfo,
    pub disk: DiskInfo,
    pub fans: Vec<FanInfo>,
    pub motherboard: MotherboardInfo,
    pub timestamp: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NetworkInfo {
    /// KB/s
    pub upload: u64,
    /// KB/s
    pub download: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryInfo {
    pub total: u64,
    pub used: u64,
    pub load: u8,
    pub temperature: u8,
    pub speed: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
#[allow(non_snake_case)]
pub struct CpuInfo {
    pub load: u8,
    pub temperature: u8,
    pub speedAverage: u32,
    pub power: u32,
    pub voltage: f32,
    /// Present in wire captures though HomeUI's PcCpu ignores it; kept for parity.
    pub usage: u8,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GpuInfo {
    pub load: u8,
    pub temperature: u8,
    pub fan: u32,
    pub speed: u32,
    pub power: u32,
    pub voltage: f32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DiskInfo {
    pub total: u64,
    pub used: u64,
    pub load: u8,
    pub activity: u8,
    pub temperature: u8,
    #[serde(rename = "readSpeed")]
    pub read_speed: u64,
    #[serde(rename = "writeSpeed")]
    pub write_speed: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FanInfo {
    #[serde(rename = "onBoard")]
    pub on_board: bool,
    pub name: String,
    pub value: u32,
    /// Rated max RPM if known (0 = unknown); drives the app's fan gauge fill.
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub max: u32,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MotherboardInfo {
    pub temperature: u8,
    #[serde(rename = "pchTemperature")]
    pub pch_temperature: u8,
}

// ============================================================================
// Stateful reader
// ============================================================================

struct CpuTimes {
    idle: u64,
    total: u64,
}

/// Sensor paths discovered once at construction.
#[derive(Default)]
struct SensorPaths {
    cpu_temp: Option<PathBuf>,
    mem_temp: Vec<PathBuf>,   // DDR module sensors (spd5118); we take the max
    disk_temp: Option<PathBuf>,
    mb_temp: Option<PathBuf>,
    rapl_energy: Option<PathBuf>,
    gpu: Option<GpuPaths>,
    net_iface: Option<String>,
    disk_stat_name: Option<String>, // e.g. "dm-0" or "nvme0n1"
    fans: Vec<FanPath>,
}

struct FanPath {
    input: PathBuf,
    max: Option<PathBuf>,
    name: String,
}

struct GpuPaths {
    device: PathBuf, // /sys/class/drm/cardN/device
    temp: Option<PathBuf>,
    fan: Option<PathBuf>,
    power: Option<PathBuf>,
    voltage: Option<PathBuf>,
    freq: Option<PathBuf>,
    is_nvidia: bool,
}

pub struct SysInfoReader {
    paths: SensorPaths,
    prev_cpu: Option<CpuTimes>,
    prev_net: Option<(u64, u64, Instant)>,   // rx, tx, when
    prev_disk: Option<(u64, u64, u64, Instant)>, // read_sectors, write_sectors, io_ms, when
    prev_rapl: Option<(u64, Instant)>,       // energy_uj, when
}

impl Default for SysInfoReader {
    fn default() -> Self {
        Self::new()
    }
}

impl SysInfoReader {
    pub fn new() -> Self {
        let mut reader = Self {
            paths: discover_sensors(),
            prev_cpu: None,
            prev_net: None,
            prev_disk: None,
            prev_rapl: None,
        };
        // Prime the delta counters so the first sample() has a baseline.
        reader.prev_cpu = read_cpu_times();
        reader.prev_net = reader.read_net_counters();
        reader.prev_disk = reader.read_disk_counters();
        reader.prev_rapl = reader.read_rapl();
        reader
    }

    /// Log a one-line summary of what was detected (call once at startup).
    pub fn log_detected(&self) {
        log::info!(
            "sensors: cpu_temp={} gpu={} mem_temp={} disk_temp={} rapl={} net={} disk={} fans={}",
            opt(&self.paths.cpu_temp),
            self.paths
                .gpu
                .as_ref()
                .map(|g| if g.is_nvidia { "nvidia" } else { "amdgpu" })
                .unwrap_or("none"),
            self.paths.mem_temp.len(),
            opt(&self.paths.disk_temp),
            opt(&self.paths.rapl_energy),
            self.paths.net_iface.as_deref().unwrap_or("none"),
            self.paths.disk_stat_name.as_deref().unwrap_or("none"),
            self.paths.fans.len(),
        );
    }

    pub fn sample(&mut self) -> SysInfo {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let (cpu_load, cpu_usage) = self.cpu_load();
        let cpu_temp = self
            .paths
            .cpu_temp
            .as_ref()
            .and_then(|p| read_milli_c(p))
            .unwrap_or(0);
        let cpu_power = self.cpu_power();
        let cpu_freq = read_cpu_freq_mhz();

        let (mem_total, mem_used, mem_load) = read_memory_info();
        let mem_temp = self
            .paths
            .mem_temp
            .iter()
            .filter_map(|p| read_milli_c(p))
            .max()
            .unwrap_or(0);

        let gpu = self.gpu_info();

        let (disk_total, disk_used, disk_used_pct) = read_disk_capacity();
        let disk_temp = self
            .paths
            .disk_temp
            .as_ref()
            .and_then(|p| read_milli_c(p))
            .unwrap_or(0);
        let (read_kbs, write_kbs, disk_busy) = self.disk_rates();

        let (up_kbs, down_kbs) = self.net_rates();

        let fans = self.read_fans();

        let mb_temp = self
            .paths
            .mb_temp
            .as_ref()
            .and_then(|p| read_milli_c(p))
            .or(Some(cpu_temp))
            .unwrap_or(0);

        SysInfo {
            network: NetworkInfo {
                upload: up_kbs,
                download: down_kbs,
            },
            memory: MemoryInfo {
                total: mem_total,
                used: mem_used,
                load: mem_load,
                temperature: mem_temp,
                speed: 0, // needs dmidecode/root; honest 0 rather than a fake clock
            },
            cpu: CpuInfo {
                load: cpu_load,
                temperature: cpu_temp,
                speedAverage: cpu_freq,
                power: cpu_power,
                voltage: 0.0, // not readable without root/superio on most boards
                usage: cpu_usage,
            },
            gpu,
            disk: DiskInfo {
                total: disk_total,
                used: disk_used,
                load: disk_used_pct,
                activity: disk_busy,
                temperature: disk_temp,
                read_speed: read_kbs,
                write_speed: write_kbs,
            },
            fans,
            motherboard: MotherboardInfo {
                temperature: mb_temp,
                pch_temperature: 0,
            },
            timestamp,
        }
    }

    fn cpu_load(&mut self) -> (u8, u8) {
        let now = match read_cpu_times() {
            Some(t) => t,
            None => return (0, 0),
        };
        let load = match &self.prev_cpu {
            Some(prev) => {
                let dt = now.total.saturating_sub(prev.total);
                let di = now.idle.saturating_sub(prev.idle);
                if dt > 0 {
                    (((dt - di) as f64 / dt as f64) * 100.0).round() as u8
                } else {
                    0
                }
            }
            None => 0,
        };
        self.prev_cpu = Some(now);
        (load.min(100), load.min(100))
    }

    fn cpu_power(&mut self) -> u32 {
        let now = match self.read_rapl() {
            Some(v) => v,
            None => return 0,
        };
        let watts = match self.prev_rapl {
            Some((e, t)) => {
                let secs = now.1.duration_since(t).as_secs_f64();
                // energy_uj wraps at max_energy_range_uj; ignore wrap (rare, → 0)
                let duj = now.0.checked_sub(e);
                match duj {
                    Some(d) if secs > 0.05 => (d as f64 / 1e6 / secs).round() as u32,
                    _ => 0,
                }
            }
            None => 0,
        };
        self.prev_rapl = Some(now);
        watts.min(1000)
    }

    fn read_rapl(&self) -> Option<(u64, Instant)> {
        let p = self.paths.rapl_energy.as_ref()?;
        let v = read_u64(p)?;
        Some((v, Instant::now()))
    }

    fn read_net_counters(&self) -> Option<(u64, u64, Instant)> {
        let iface = self.paths.net_iface.as_ref()?;
        let rx = read_u64(Path::new(&format!(
            "/sys/class/net/{iface}/statistics/rx_bytes"
        )))?;
        let tx = read_u64(Path::new(&format!(
            "/sys/class/net/{iface}/statistics/tx_bytes"
        )))?;
        Some((rx, tx, Instant::now()))
    }

    fn net_rates(&mut self) -> (u64, u64) {
        let now = match self.read_net_counters() {
            Some(v) => v,
            None => return (0, 0),
        };
        let rates = match self.prev_net {
            Some((rx, tx, t)) => {
                let secs = now.2.duration_since(t).as_secs_f64();
                if secs > 0.05 {
                    let down = (now.0.saturating_sub(rx) as f64 / 1024.0 / secs).round() as u64;
                    let up = (now.1.saturating_sub(tx) as f64 / 1024.0 / secs).round() as u64;
                    (up, down)
                } else {
                    (0, 0)
                }
            }
            None => (0, 0),
        };
        self.prev_net = Some(now);
        rates
    }

    fn read_disk_counters(&self) -> Option<(u64, u64, u64, Instant)> {
        let name = self.paths.disk_stat_name.as_ref()?;
        let stats = fs::read_to_string("/proc/diskstats").ok()?;
        for line in stats.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            // 3=name 6=sectors_read 10=sectors_written 13=io_ticks(ms)
            if f.len() >= 14 && f[2] == name {
                let rd = f[5].parse().ok()?;
                let wr = f[9].parse().ok()?;
                let io = f[12].parse().ok()?;
                return Some((rd, wr, io, Instant::now()));
            }
        }
        None
    }

    fn disk_rates(&mut self) -> (u64, u64, u8) {
        let now = match self.read_disk_counters() {
            Some(v) => v,
            None => return (0, 0, 0),
        };
        let out = match self.prev_disk {
            Some((rd, wr, io, t)) => {
                let secs = now.3.duration_since(t).as_secs_f64();
                if secs > 0.05 {
                    // sectors are 512 bytes → KB/s = sectors*512/1024/s = sectors/2/s
                    let read_kbs = (now.0.saturating_sub(rd) as f64 * 0.5 / secs).round() as u64;
                    let write_kbs = (now.1.saturating_sub(wr) as f64 * 0.5 / secs).round() as u64;
                    let busy_ms = now.2.saturating_sub(io) as f64;
                    let busy = ((busy_ms / (secs * 1000.0)) * 100.0).round().min(100.0) as u8;
                    (read_kbs, write_kbs, busy)
                } else {
                    (0, 0, 0)
                }
            }
            None => (0, 0, 0),
        };
        self.prev_disk = Some(now);
        out
    }

    fn gpu_info(&self) -> GpuInfo {
        let Some(g) = &self.paths.gpu else {
            return GpuInfo {
                load: 0,
                temperature: 0,
                fan: 0,
                speed: 0,
                power: 0,
                voltage: 0.0,
            };
        };
        if g.is_nvidia {
            return read_nvidia_gpu();
        }
        let temp = g.temp.as_ref().and_then(|p| read_milli_c(p)).unwrap_or(0);
        let fan = g.fan.as_ref().and_then(|p| read_u64(p)).unwrap_or(0) as u32;
        let load = read_u64(&g.device.join("gpu_busy_percent")).unwrap_or(0) as u8;
        // freq*_input is in Hz
        let speed = g
            .freq
            .as_ref()
            .and_then(|p| read_u64(p))
            .map(|hz| (hz / 1_000_000) as u32)
            .unwrap_or(0);
        let power = g
            .power
            .as_ref()
            .and_then(|p| read_u64(p))
            .map(|uw| (uw / 1_000_000) as u32) // µW → W
            .unwrap_or(0);
        let voltage = g
            .voltage
            .as_ref()
            .and_then(|p| read_u64(p))
            .map(|mv| mv as f32 / 1000.0) // mV → V
            .unwrap_or(0.0);
        GpuInfo {
            load: load.min(100),
            temperature: temp,
            fan,
            speed,
            power,
            voltage,
        }
    }

    fn read_fans(&self) -> Vec<FanInfo> {
        self.paths
            .fans
            .iter()
            .filter_map(|f| {
                let value = read_u64(&f.input)? as u32;
                let max = f.max.as_ref().and_then(|p| read_u64(p)).unwrap_or(0) as u32;
                Some(FanInfo {
                    on_board: true,
                    name: f.name.clone(),
                    value,
                    max,
                })
            })
            .collect()
    }
}

// ============================================================================
// Sensor discovery
// ============================================================================

fn discover_sensors() -> SensorPaths {
    let hwmons = enumerate_hwmon();
    let mut p = SensorPaths::default();

    // CPU temperature: AMD Tctl, else Intel package, else thermal_zone, else any
    p.cpu_temp = find_labeled_temp(&hwmons, "k10temp", &["Tctl"])
        .or_else(|| find_labeled_temp(&hwmons, "coretemp", &["Package id 0"]))
        .or_else(|| find_thermal_zone("x86_pkg_temp"))
        .or_else(|| hwmons.iter().find_map(|h| exists(h.dir.join("temp1_input"))));

    // Motherboard temp: an "asus"/"nct*" chip if present, else fall back to CPU at sample time
    p.mb_temp = hwmons
        .iter()
        .find(|h| h.name == "asus" || h.name.starts_with("nct"))
        .and_then(|h| exists(h.dir.join("temp1_input")));

    // RAM module temps (all DDR5 SPD hubs)
    p.mem_temp = hwmons
        .iter()
        .filter(|h| h.name == "spd5118" || h.name.starts_with("jc42"))
        .filter_map(|h| exists(h.dir.join("temp1_input")))
        .collect();

    // Disk temp: NVMe composite
    p.disk_temp = hwmons
        .iter()
        .find(|h| h.name == "nvme")
        .and_then(|h| exists(h.dir.join("temp1_input")));

    // CPU package power via RAPL (root-readable only; we handle EACCES → 0)
    p.rapl_energy = find_rapl_package();

    p.gpu = discover_gpu();
    p.net_iface = default_net_iface();
    p.disk_stat_name = root_disk_stat_name();
    p.fans = discover_fans(&hwmons);

    p
}

struct Hwmon {
    dir: PathBuf,
    name: String,
}

fn enumerate_hwmon() -> Vec<Hwmon> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for e in entries.flatten() {
            let dir = e.path();
            if let Some(name) = read_trimmed(&dir.join("name")) {
                out.push(Hwmon { dir, name });
            }
        }
    }
    out
}

fn find_labeled_temp(hwmons: &[Hwmon], chip: &str, labels: &[&str]) -> Option<PathBuf> {
    let h = hwmons.iter().find(|h| h.name == chip)?;
    for i in 1..=20 {
        let label_path = h.dir.join(format!("temp{i}_label"));
        if let Some(label) = read_trimmed(&label_path) {
            if labels.iter().any(|l| l.eq_ignore_ascii_case(&label)) {
                return exists(h.dir.join(format!("temp{i}_input")));
            }
        }
    }
    // No matching label — fall back to temp1_input of that chip
    exists(h.dir.join("temp1_input"))
}

fn find_thermal_zone(kind: &str) -> Option<PathBuf> {
    for i in 0..20 {
        let base = PathBuf::from(format!("/sys/class/thermal/thermal_zone{i}"));
        if !base.exists() {
            break;
        }
        if read_trimmed(&base.join("type")).as_deref() == Some(kind) {
            return exists(base.join("temp"));
        }
    }
    None
}

fn find_rapl_package() -> Option<PathBuf> {
    for i in 0..8 {
        let base = PathBuf::from(format!("/sys/class/powercap/intel-rapl:{i}"));
        let name = read_trimmed(&base.join("name"));
        if matches!(name.as_deref(), Some(n) if n.starts_with("package")) {
            return exists(base.join("energy_uj"));
        }
    }
    // Fall back to domain 0 regardless of name
    exists(PathBuf::from("/sys/class/powercap/intel-rapl:0/energy_uj"))
}

fn discover_gpu() -> Option<GpuPaths> {
    // NVIDIA: no hwmon-by-name reliably; detect via nvidia-smi presence
    if Command::new("nvidia-smi").arg("-L").output().map(|o| o.status.success()).unwrap_or(false) {
        return Some(GpuPaths {
            device: PathBuf::new(),
            temp: None,
            fan: None,
            power: None,
            voltage: None,
            freq: None,
            is_nvidia: true,
        });
    }

    // AMD: pick the discrete card (largest VRAM) among cards with an amdgpu hwmon
    let mut best: Option<(u64, GpuPaths)> = None;
    if let Ok(cards) = fs::read_dir("/sys/class/drm") {
        for c in cards.flatten() {
            let name = c.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("card") && name.len() == 5) {
                continue; // skip card0-DP-1 etc; want cardN only
            }
            let device = c.path().join("device");
            let hwmon_root = device.join("hwmon");
            let Some(hwmon) = first_hwmon_named(&hwmon_root, "amdgpu") else {
                continue;
            };
            let vram = read_u64(&device.join("mem_info_vram_total")).unwrap_or(0);
            let paths = GpuPaths {
                temp: labeled_or_first_temp(&hwmon, "edge"),
                fan: exists(hwmon.join("fan1_input")),
                power: exists(hwmon.join("power1_average"))
                    .or_else(|| exists(hwmon.join("power1_input"))),
                voltage: exists(hwmon.join("in0_input")),
                freq: exists(hwmon.join("freq1_input")),
                device,
                is_nvidia: false,
            };
            if best.as_ref().map(|(v, _)| vram > *v).unwrap_or(true) {
                best = Some((vram, paths));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn first_hwmon_named(hwmon_root: &Path, chip: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(hwmon_root).ok()?;
    for e in entries.flatten() {
        if read_trimmed(&e.path().join("name")).as_deref() == Some(chip) {
            return Some(e.path());
        }
    }
    None
}

fn labeled_or_first_temp(hwmon: &Path, label: &str) -> Option<PathBuf> {
    for i in 1..=8 {
        if read_trimmed(&hwmon.join(format!("temp{i}_label")))
            .as_deref()
            .map(|l| l.eq_ignore_ascii_case(label))
            .unwrap_or(false)
        {
            return exists(hwmon.join(format!("temp{i}_input")));
        }
    }
    exists(hwmon.join("temp1_input"))
}

fn discover_fans(hwmons: &[Hwmon]) -> Vec<FanPath> {
    // System/case/CPU fans only. GPU fans are reported separately via gpu.fan,
    // so we skip amdgpu/nvidia here to avoid double-counting. On boards whose
    // super-I/O driver (nct6775, it87, …) isn't loaded, this is legitimately
    // empty — the headless box has no readable case fans.
    let mut fans = Vec::new();
    let mut used_names: Vec<String> = Vec::new();
    for h in hwmons {
        if h.name == "amdgpu" || h.name == "nvidia" {
            continue;
        }
        for i in 1..=8 {
            let input = h.dir.join(format!("fan{i}_input"));
            if !input.exists() {
                continue;
            }
            let label = read_trimmed(&h.dir.join(format!("fan{i}_label")));
            let mut name = label.unwrap_or_else(|| format!("{}{i}", h.name));
            // Ensure unique display names
            if used_names.contains(&name) {
                name = format!("{name}#{}", used_names.len() + 1);
            }
            used_names.push(name.clone());
            fans.push(FanPath {
                max: exists(h.dir.join(format!("fan{i}_max"))),
                input,
                name,
            });
        }
    }
    fans
}

fn default_net_iface() -> Option<String> {
    // /proc/net/route: the line with Destination 00000000 is the default route
    if let Ok(route) = fs::read_to_string("/proc/net/route") {
        for line in route.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 2 && f[1] == "00000000" {
                return Some(f[0].to_string());
            }
        }
    }
    // Fallback: first UP, non-loopback, non-virtual iface
    if let Ok(ifaces) = fs::read_dir("/sys/class/net") {
        for e in ifaces.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name == "lo" || name.starts_with("docker") || name.starts_with("veth") || name.starts_with("br-") {
                continue;
            }
            if read_trimmed(&e.path().join("operstate")).as_deref() == Some("up") {
                return Some(name);
            }
        }
    }
    None
}

fn root_disk_stat_name() -> Option<String> {
    // Resolve "/" mount source to a /sys/block name usable in /proc/diskstats
    let source = Command::new("findmnt")
        .args(["-n", "-o", "SOURCE", "/"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
    // Canonicalize e.g. /dev/mapper/x → /dev/dm-0, then take the basename
    let real = fs::canonicalize(&source).unwrap_or_else(|_| PathBuf::from(&source));
    let name = real.file_name()?.to_string_lossy().to_string();
    // Confirm it appears in diskstats
    if let Ok(stats) = fs::read_to_string("/proc/diskstats") {
        if stats.lines().any(|l| {
            l.split_whitespace().nth(2).map(|n| n == name).unwrap_or(false)
        }) {
            return Some(name);
        }
    }
    Some(name)
}

// ============================================================================
// Low-level readers
// ============================================================================

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

/// Read a millidegree-C sysfs value and return whole °C.
fn read_milli_c(path: &Path) -> Option<u8> {
    let milli: i64 = read_trimmed(path)?.parse().ok()?;
    Some((milli / 1000).clamp(0, 255) as u8)
}

fn exists(p: PathBuf) -> Option<PathBuf> {
    p.exists().then_some(p)
}

fn opt(p: &Option<PathBuf>) -> &'static str {
    if p.is_some() { "yes" } else { "no" }
}

fn read_cpu_times() -> Option<CpuTimes> {
    let content = fs::read_to_string("/proc/stat").ok()?;
    let line = content.lines().next()?; // aggregate "cpu ..."
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();
    if vals.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal ...
    let idle = vals[3] + vals.get(4).copied().unwrap_or(0); // idle + iowait
    let total: u64 = vals.iter().sum();
    Some(CpuTimes { idle, total })
}

fn read_cpu_freq_mhz() -> u32 {
    // Average scaling_cur_freq (kHz) across all cores
    let mut sum = 0u64;
    let mut n = 0u64;
    if let Ok(cpus) = fs::read_dir("/sys/devices/system/cpu") {
        for e in cpus.flatten() {
            let khz = e.path().join("cpufreq/scaling_cur_freq");
            if let Some(v) = read_u64(&khz) {
                sum += v;
                n += 1;
            }
        }
    }
    if n > 0 {
        return (sum / n / 1000) as u32;
    }
    // Fallback: /proc/cpuinfo "cpu MHz"
    if let Ok(info) = fs::read_to_string("/proc/cpuinfo") {
        let (sum, n) = info
            .lines()
            .filter(|l| l.starts_with("cpu MHz"))
            .filter_map(|l| l.split(':').nth(1)?.trim().parse::<f64>().ok())
            .fold((0.0, 0u32), |(s, c), v| (s + v, c + 1));
        if n > 0 {
            return (sum / n as f64).round() as u32;
        }
    }
    0
}

fn read_memory_info() -> (u64, u64, u8) {
    let content = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut total = 0u64;
    let mut available = 0u64;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = parse_kb(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            available = parse_kb(v);
        }
    }
    let used = total.saturating_sub(available);
    let load = if total > 0 { ((used * 100) / total) as u8 } else { 0 };
    (total / 1024, used / 1024, load) // KB → MB
}

fn parse_kb(s: &str) -> u64 {
    s.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn read_disk_capacity() -> (u64, u64, u8) {
    // Capacity of the root filesystem in GB, plus used%
    if let Ok(output) = Command::new("df").args(["--output=size,used,pcent", "/"]).output() {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = stdout.lines().nth(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    let total_kb: u64 = parts[0].parse().unwrap_or(0);
                    let used_kb: u64 = parts[1].parse().unwrap_or(0);
                    let pct: u8 = parts[2].trim_end_matches('%').parse().unwrap_or(0);
                    return (total_kb / 1024 / 1024, used_kb / 1024 / 1024, pct);
                }
            }
        }
    }
    (0, 0, 0)
}

fn read_nvidia_gpu() -> GpuInfo {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=temperature.gpu,utilization.gpu,clocks.sm,power.draw,fan.speed",
            "--format=csv,noheader,nounits",
        ])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let line = String::from_utf8_lossy(&out.stdout);
            let f: Vec<String> = line
                .trim()
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            let g = |i: usize| f.get(i).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.0);
            return GpuInfo {
                temperature: g(0) as u8,
                load: g(1) as u8,
                speed: g(2) as u32,
                power: g(3) as u32,
                fan: g(4) as u32,
                voltage: 0.0,
            };
        }
    }
    GpuInfo { load: 0, temperature: 0, fan: 0, speed: 0, power: 0, voltage: 0.0 }
}

// ============================================================================
// Back-compat shim
// ============================================================================

impl SysInfo {
    /// Convenience one-shot read. Rate fields (cpu.load, network, disk IO,
    /// power) require two samples spaced in time, so this primes and takes a
    /// second sample after a short delay. For streaming, hold a `SysInfoReader`.
    pub fn get_sysinfo() -> Self {
        let mut reader = SysInfoReader::new();
        std::thread::sleep(std::time::Duration::from_millis(250));
        reader.sample()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kb_ok() {
        assert_eq!(parse_kb(" 65900388 kB"), 65900388);
        assert_eq!(parse_kb("garbage"), 0);
        assert_eq!(parse_kb(""), 0);
    }

    #[test]
    fn read_milli_c_clamps() {
        // via a temp file, since it takes a path
        let dir = std::env::temp_dir();
        let p = dir.join(format!("tryx_test_temp_{}", std::process::id()));
        std::fs::write(&p, "52125\n").unwrap();
        assert_eq!(read_milli_c(&p), Some(52));
        std::fs::write(&p, "999999999").unwrap();
        assert_eq!(read_milli_c(&p), Some(255)); // clamped to u8
        std::fs::write(&p, "-5000").unwrap();
        assert_eq!(read_milli_c(&p), Some(0)); // clamped low
        let _ = std::fs::remove_file(&p);
    }

    /// The device's HomeUI parses these exact JSON keys; renaming any would
    /// silently blank a field on the display. Lock the wire schema.
    #[test]
    fn wire_schema_keys() {
        let info = SysInfo {
            network: NetworkInfo { upload: 1, download: 2 },
            memory: MemoryInfo { total: 1, used: 1, load: 1, temperature: 1, speed: 1 },
            cpu: CpuInfo {
                load: 1, temperature: 1, speedAverage: 1, power: 1, voltage: 1.0, usage: 1,
            },
            gpu: GpuInfo {
                load: 1, temperature: 1, fan: 1, speed: 1, power: 1, voltage: 1.0,
            },
            disk: DiskInfo {
                total: 1, used: 1, load: 1, activity: 1, temperature: 1,
                read_speed: 1, write_speed: 1,
            },
            fans: vec![FanInfo { on_board: true, name: "CPU1".into(), value: 1200, max: 3000 }],
            motherboard: MotherboardInfo { temperature: 1, pch_temperature: 1 },
            timestamp: 1,
        };
        let json = serde_json::to_string(&info).unwrap();
        for key in [
            "\"network\"", "\"memory\"", "\"cpu\"", "\"gpu\"", "\"disk\"", "\"fans\"",
            "\"motherboard\"", "\"timestamp\"",
            "\"speedAverage\"", "\"usage\"", "\"readSpeed\"", "\"writeSpeed\"",
            "\"pchTemperature\"", "\"onBoard\"", "\"activity\"",
        ] {
            assert!(json.contains(key), "missing wire key {key} in {json}");
        }
    }

    /// max=0 is omitted (matches the device's Fan schema where 0 = unknown).
    #[test]
    fn fan_max_zero_omitted() {
        let f = FanInfo { on_board: true, name: "X".into(), value: 500, max: 0 };
        let json = serde_json::to_string(&f).unwrap();
        assert!(!json.contains("max"), "max:0 should be omitted: {json}");
        let f2 = FanInfo { max: 3000, ..f };
        assert!(serde_json::to_string(&f2).unwrap().contains("\"max\":3000"));
    }

    /// Discovery + one sample must never panic on whatever host runs the tests,
    /// and must produce a serializable payload with a real timestamp.
    #[test]
    fn sample_smoke() {
        let mut reader = SysInfoReader::new();
        let info = reader.sample();
        assert!(info.timestamp > 0);
        assert!(info.cpu.load <= 100);
        assert!(info.memory.load <= 100);
        assert!(info.disk.load <= 100);
        assert!(serde_json::to_value(&info).is_ok());
    }
}
