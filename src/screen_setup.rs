use std::{
    path::PathBuf,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use serde::{Deserialize, Serialize};
use anyhow::{Context, Result};
use std::io::Read;

/// How long `adb wait-for-device` may take before we call the device absent.
const ADB_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
/// Ceiling for one `adb push` (large GIFs over a LAN are slow, not stuck).
const ADB_PUSH_TIMEOUT: Duration = Duration::from_secs(300);
/// How often the serial link is fed while adb runs.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(2);

/// The adb server that sees the cooler, when it isn't the local default:
/// `ADB_SERVER_SOCKET` if set, else — for a remote device (`tcp://host:port`) —
/// that host's server on the default port, as `packaging/remote-bridge.sh` shares it.
fn adb_server(port_path: &str) -> Option<String> {
    if let Ok(server) = std::env::var("ADB_SERVER_SOCKET")
        && !server.is_empty()
    {
        return Some(server);
    }
    let addr = port_path.strip_prefix("tcp://")?;
    let host = addr.rsplit_once(':').map_or(addr, |(host, _)| host);
    Some(format!("tcp:{host}:5037"))
}

/// `adb`, aimed at the server [`adb_server`] picks for `port_path`.
pub fn adb_command(port_path: &str) -> Command {
    let mut cmd = Command::new("adb");
    if let Some(server) = adb_server(port_path) {
        cmd.env("ADB_SERVER_SOCKET", server);
    }
    cmd
}

/// Run `cmd` to completion, calling `tick` every [`KEEPALIVE_EVERY`] while it
/// runs. Kills it and fails after `timeout`.
fn run_ticking(
    cmd: &mut Command,
    what: &str,
    timeout: Duration,
    tick: &mut dyn FnMut() -> Result<()>,
) -> Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to run {what} (is adb installed?)"))?;
    let start = Instant::now();
    let mut last_tick = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().with_context(|| format!("{what} output"));
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{what} did not finish within {}s", timeout.as_secs());
        }
        if last_tick.elapsed() >= KEEPALIVE_EVERY {
            tick()?;
            last_tick = Instant::now();
        }
        thread::sleep(Duration::from_millis(50));
    }
}

use crate::data::{send_command, send_state_command};
use crate::sysinfo::SysInfo;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenConfig {
    pub id: String,
    pub screen_mode: String,
    pub play_mode: String,
    pub ratio: String,
    pub color: String,
    pub align: String,
    /// Built-in animation overlay (device plays /system/media/anim/<name>.webp).
    /// Observed values: "Rain", "Smoke". None = no filter.
    pub filter_value: Option<String>,
    pub filter_opacity: u8,
    pub badges: Vec<String>,
    pub sysinfo_display: Vec<String>,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            id: "Customization".to_string(),
            screen_mode: "Full Screen".to_string(),
            play_mode: "Single".to_string(),
            ratio: "2:1".to_string(),
            color: "#dcdcdc".to_string(),
            align: "Left".to_string(),
            filter_value: None,
            filter_opacity: 100,
            badges: vec!["GPU Badge".to_string(), "CPU Badge".to_string()],
            sysinfo_display: vec!["CPU Temperature".to_string(), "GPU Temperature".to_string()],
        }
    }
}

impl ScreenConfig {
    /// The waterBlockScreenId payload for a media playlist (Full Screen form;
    /// "Screen Splitting" uses arrays for settings/sysinfoDisplay and no ratio
    /// — not implemented yet).
    pub fn to_water_block_json(&self, media: &[String]) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "screenMode": self.screen_mode,
            "playMode": self.play_mode,
            "ratio": self.ratio,
            "media": media,
            "settings": {
                "color": self.color,
                "align": self.align,
                "filter": {
                    "value": self.filter_value,
                    "opacity": self.filter_opacity
                },
                "badges": self.badges
            },
            "sysinfoDisplay": self.sysinfo_display
        })
    }
}

pub struct AioCoolerController {
    serial_device: String,
}

impl AioCoolerController {
    pub fn new(serial_device: &str) -> Self {
        Self {
            serial_device: serial_device.to_string(),
        }
    }

    /// Push `local_path` to the device's media dir as `remote_name`, calling
    /// `tick` every couple of seconds meanwhile so the serial link stays fed.
    pub fn adb_push(
        &self,
        local_path: &PathBuf,
        remote_name: &str,
        tick: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        log::info!("Pushing image to device through ADB");

        let wait = run_ticking(
            adb_command(&self.serial_device).arg("wait-for-device"),
            "adb wait-for-device",
            ADB_WAIT_TIMEOUT,
            tick,
        )
        .with_context(|| {
            format!(
                "No ADB device appeared — the image bytes travel over ADB, not serial. \
                 Check `adb devices`. If the cooler is attached to another machine, \
                 run packaging/remote-bridge.sh there and export \
                 ADB_SERVER_SOCKET=tcp:<that-host>:5037 (a tcp:// device sets this \
                 for you). Using: {}",
                adb_server(&self.serial_device).unwrap_or_else(|| "the local adb server".into())
            )
        })?;
        if !wait.status.success() {
            anyhow::bail!(
                "adb wait-for-device failed: {}",
                String::from_utf8_lossy(&wait.stderr).trim()
            );
        }

        let remote_path = format!("/sdcard/pcMedia/{}", remote_name);
        log::info!("Pushing {} to {}", local_path.display(), remote_path);

        let output = run_ticking(
            adb_command(&self.serial_device).args(["push", &local_path.to_string_lossy(), &remote_path]),
            "adb push",
            ADB_PUSH_TIMEOUT,
            tick,
        )?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("ADB push failed: {}", stderr);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        log::info!("ADB push output: {}", stdout.trim());

        // Verify file exists and has correct size
        let expected_size = std::fs::metadata(local_path)?.len();
        let size_check = run_ticking(
            adb_command(&self.serial_device).args(["shell", "stat", "-c", "%s", &remote_path]),
            "adb shell stat",
            ADB_WAIT_TIMEOUT,
            tick,
        )?;

        if size_check.status.success() {
            let remote_size: u64 = String::from_utf8_lossy(&size_check.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
            
            if remote_size != expected_size {
                anyhow::bail!(
                    "File size mismatch: local={}, remote={}",
                    expected_size,
                    remote_size
                );
            }
            log::info!("Verified file size: {} bytes", remote_size);
        }

        // Small delay to ensure device has processed the file
        thread::sleep(Duration::from_millis(500));

        log::info!("ADB push successful");
        Ok(())
    }

    fn open_port(&self) -> Result<Box<dyn serialport::SerialPort>> {
        log::info!("Opening device transport: {}", self.serial_device);

        // Delegate to the shared opener so a `tcp://host:port` device string
        // transparently uses the network bridge (remote GUI/CLI over the LAN).
        let mut port = crate::commands::open_port(&self.serial_device)?;
        port.set_timeout(Duration::from_secs(2))
            .context("Failed to set serial timeout")?;

        // Clear buffers
        thread::sleep(Duration::from_millis(100));
        let _ = port.clear(serialport::ClearBuffer::All);
        Ok(port)
    }

    /// Configure the display for already-present media (no upload, no cleanup).
    pub fn send_screen_config(&self, media: &[String], config: &ScreenConfig) -> Result<()> {
        let mut port = self.open_port()?;

        log::info!("Sending initial sysinfo...");
        self.send_sysinfo(&mut port)?;
        thread::sleep(Duration::from_millis(200));

        self.send_screen_config_on(&mut port, media, config)?;
        self.keepalive(&mut port, 5)?;
        Ok(())
    }

    fn send_screen_config_on(
        &self,
        port: &mut Box<dyn serialport::SerialPort>,
        media: &[String],
        config: &ScreenConfig,
    ) -> Result<()> {
        log::info!("Sending screen configuration for: {media:?}");
        send_command(port, "waterBlockScreenId", &config.to_water_block_json(media))
    }

    /// Send several sysinfo updates to keep the connection alive and populate temps
    fn keepalive(&self, port: &mut Box<dyn serialport::SerialPort>, updates: u32) -> Result<()> {
        log::info!("Sending sysinfo updates to keep connection alive...");
        for i in 0..updates {
            thread::sleep(Duration::from_millis(800));
            self.send_sysinfo(port)?;
            log::debug!("Sysinfo update {}/{updates}", i + 1);
        }
        Ok(())
    }

    /// Send current system info (CPU/GPU temps, etc)
    fn send_sysinfo(&self, port: &mut Box<dyn serialport::SerialPort>) -> Result<()> {
        let info = SysInfo::get_sysinfo();
        let json = serde_json::to_value(&info)?;
        send_state_command(port, "all", &json)?;
        log::debug!("Sysinfo: CPU {}°C, GPU {}°C", info.cpu.temperature, info.gpu.temperature);
        Ok(())
    }

    pub fn calculate_md5(path: &PathBuf) -> Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;
        Ok(format!("{:x}", md5::compute(&buffer)))
    }

    pub fn generate_filename(extension: &str) -> String {
        let now = chrono::Local::now();
        now.format(&format!("%Y-%m-%d_%H-%M-%S-%3f.{}", extension))
            .to_string()
    }
}
