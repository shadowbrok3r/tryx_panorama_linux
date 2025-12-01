// System information reader for Linux
// Reads CPU/GPU temps, memory, disk stats for AIO cooler display

use std::fs;
use std::process::Command;

/// System info payload matching APK protocol
#[derive(Debug, serde::Serialize)]
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

#[derive(Debug, serde::Serialize)]
pub struct NetworkInfo {
    pub upload: u64,
    pub download: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct MemoryInfo {
    pub total: u64,
    pub used: u64,
    pub load: u8,
    pub temperature: u8,
    pub speed: u32,
}

#[derive(Debug, serde::Serialize)]
#[allow(non_snake_case)]
pub struct CpuInfo {
    pub load: u8,
    pub temperature: u8,
    pub speedAverage: u32,
    pub power: u32,
    pub voltage: f32,
    pub usage: u8,
}

#[derive(Debug, serde::Serialize)]
pub struct GpuInfo {
    pub load: u8,
    pub temperature: u8,
    pub fan: u32,
    pub speed: u32,
    pub power: u32,
    pub voltage: f32,
}

#[derive(Debug, serde::Serialize)]
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

#[derive(Debug, serde::Serialize)]
pub struct FanInfo {
    #[serde(rename = "onBoard")]
    pub on_board: bool,
    pub name: String,
    pub value: u32,
}

#[derive(Debug, serde::Serialize)]
pub struct MotherboardInfo {
    pub temperature: u8,
    #[serde(rename = "pchTemperature")]
    pub pch_temperature: u8,
}

impl Default for SysInfo {
    fn default() -> Self {
        Self {
            network: NetworkInfo { upload: 0, download: 0 },
            memory: MemoryInfo { total: 0, used: 0, load: 0, temperature: 0, speed: 0 },
            cpu: CpuInfo { load: 0, temperature: 0, speedAverage: 0, power: 0, voltage: 0.0, usage: 0 },
            gpu: GpuInfo { load: 0, temperature: 0, fan: 0, speed: 0, power: 0, voltage: 0.0 },
            disk: DiskInfo { total: 0, used: 0, load: 0, activity: 0, temperature: 0, read_speed: 0, write_speed: 0 },
            fans: vec![],
            motherboard: MotherboardInfo { temperature: 0, pch_temperature: 0 },
            timestamp: 0,
        }
    }
}

impl SysInfo {
    pub fn get_sysinfo() -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let cpu_temp = read_cpu_temp().unwrap_or(0);
        let gpu_temp = read_gpu_temp().unwrap_or(0);
        let (mem_total, mem_used, mem_load) = read_memory_info();
        let (disk_total, disk_used, disk_load) = read_disk_info();

        Self {
            network: NetworkInfo { upload: 0, download: 0 },
            memory: MemoryInfo {
                total: mem_total,
                used: mem_used,
                load: mem_load,
                temperature: 0,
                speed: 3200, // placeholder
            },
            cpu: CpuInfo {
                load: read_cpu_load().unwrap_or(0),
                temperature: cpu_temp,
                speedAverage: 3000,
                power: 0,
                voltage: 1.0,
                usage: read_cpu_load().unwrap_or(0),
            },
            gpu: GpuInfo {
                load: 0,
                temperature: gpu_temp,
                fan: 0,
                speed: 0,
                power: 0,
                voltage: 0.0,
            },
            disk: DiskInfo {
                total: disk_total,
                used: disk_used,
                load: disk_load,
                activity: 0,
                temperature: 0,
                read_speed: 0,
                write_speed: 0,
            },
            fans: vec![],
            motherboard: MotherboardInfo { temperature: 0, pch_temperature: 0 },
            timestamp,
        }
    }
}

/// Read CPU temp from thermal zones
fn read_cpu_temp() -> Option<u8> {
    for i in 0..10 {
        let path = format!("/sys/class/thermal/thermal_zone{}/temp", i);
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(temp_milli) = content.trim().parse::<i32>() {
                return Some((temp_milli / 1000) as u8);
            }
        }
    }

    // Try hwmon coretemp
    for i in 0..10 {
        let path = format!("/sys/class/hwmon/hwmon{}/temp1_input", i);
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(temp_milli) = content.trim().parse::<i32>() {
                return Some((temp_milli / 1000) as u8);
            }
        }
    }

    None
}

/// Read GPU temp (supports NVIDIA and AMD)
fn read_gpu_temp() -> Option<u8> {
    // Try nvidia-smi first..
    if let Ok(output) = Command::new("nvidia-smi")
        .args(["--query-gpu=temperature.gpu", "--format=csv,noheader,nounits"])
        .output()
    {
        if output.status.success() {
            if let Ok(temp) = String::from_utf8_lossy(&output.stdout).trim().parse::<u8>() {
                return Some(temp);
            }
        }
    }

    // Otherwise, try AMD hwmon
    for card in &["card0", "card1"] {
        for i in 0..5 {
            let path = format!("/sys/class/drm/{}/device/hwmon/hwmon{}/temp1_input", card, i);
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(temp_milli) = content.trim().parse::<i32>() {
                    return Some((temp_milli / 1000) as u8);
                }
            }
        }
    }

    None
}

/// Read memory info from /proc/meminfo
fn read_memory_info() -> (u64, u64, u8) {
    let content = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut total: u64 = 0;
    let mut available: u64 = 0;

    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            total = parse_meminfo_value(line);
        } else if line.starts_with("MemAvailable:") {
            available = parse_meminfo_value(line);
        }
    }

    let used = total.saturating_sub(available);
    let load = if total > 0 { ((used * 100) / total) as u8 } else { 0 };

    // Convert from KB to MB
    (total / 1024, used / 1024, load)
}

fn parse_meminfo_value(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Read disk info for root partition
fn read_disk_info() -> (u64, u64, u8) {
    if let Ok(output) = Command::new("df")
        .args(["--output=size,used,pcent", "/"])
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = stdout.lines().nth(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    let total: u64 = parts[0].parse().unwrap_or(0) / 1024; // KB to MB
                    let used: u64 = parts[1].parse().unwrap_or(0) / 1024;
                    let load: u8 = parts[2].trim_end_matches('%').parse().unwrap_or(0);
                    return (total / 1024, used / 1024, load); // MB to GB
                }
            }
        }
    }
    (0, 0, 0)
}

/// Read CPU load from /proc/stat (rough estimate for now, will probably be replaced with sysinfo eventually)
fn read_cpu_load() -> Option<u8> {
    let content = fs::read_to_string("/proc/loadavg").ok()?;
    let load_1min: f32 = content.split_whitespace().next()?.parse().ok()?;
    Some((load_1min * 25.0).min(100.0) as u8)
}

