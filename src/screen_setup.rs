use std::{path::PathBuf, process::Command, thread, time::Duration};
use serde::{Deserialize, Serialize};
use anyhow::{Context, Result};
use std::io::Read;

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
            filter_opacity: 100,
            badges: vec!["GPU Badge".to_string(), "CPU Badge".to_string()],
            sysinfo_display: vec!["CPU Temperature".to_string(), "GPU Temperature".to_string()],
        }
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

    pub fn adb_push(&self, local_path: &PathBuf, remote_name: &str) -> Result<()> {
        log::info!("Pushing image to device through ADB");
        
        let status = Command::new("adb")
            .args(["wait-for-device"])
            .status()
            .context("Failed to execute adb wait-for-device")?;

        if !status.success() {
            anyhow::bail!("ADB wait-for-device failed");
        }

        let remote_path = format!("/sdcard/pcMedia/{}", remote_name);
        log::info!("Pushing {} to {}", local_path.display(), remote_path);

        let output = Command::new("adb")
            .args(["push", &local_path.to_string_lossy(), &remote_path])
            .output()
            .context("Failed to execute adb push")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("ADB push failed: {}", stderr);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        log::info!("ADB push output: {}", stdout.trim());

        // Verify file exists and has correct size
        let expected_size = std::fs::metadata(local_path)?.len();
        let size_check = Command::new("adb")
            .args(["shell", "stat", "-c", "%s", &remote_path])
            .output()?;
        
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

    /// Send screen configuration command with sysinfo to keep connection alive.
    /// Skip transport/transported commands for nowbecause those expect file data over serial.
    pub fn send_image_commands(
        &self,
        file_name: &str,
        _file_size: u64,
        _file_md5: &str,
        config: &ScreenConfig,
    ) -> Result<()> {
        log::info!("Opening serial port: {}", self.serial_device);

        let mut port = serialport::new(&self.serial_device, 115200)
            .timeout(Duration::from_secs(2))
            .open()
            .context("Failed to open serial port")?;

        // Clear buffers
        thread::sleep(Duration::from_millis(100));
        let _ = port.clear(serialport::ClearBuffer::All);

        // Send initial sysinfo to establish connection
        log::info!("Sending initial sysinfo...");
        self.send_sysinfo(&mut port)?;
        thread::sleep(Duration::from_millis(200));

        // Clean up old media files FIRST to avoid playlist fuckery
        log::info!("Cleaning up old media files (keeping: {})", file_name);
        send_command(
            &mut port,
            "mediaDelete",
            &serde_json::json!({
                "exclude": [file_name]
            }),
        )?;
        thread::sleep(Duration::from_millis(300));

        // Keepalive
        self.send_sysinfo(&mut port)?;
        thread::sleep(Duration::from_millis(200));

        // Send screen config with new file
        log::info!("Sending screen configuration for: {}", file_name);
        send_command(
            &mut port,
            "waterBlockScreenId",
            &serde_json::json!({
                "id": config.id,
                "screenMode": config.screen_mode,
                "playMode": config.play_mode,
                "ratio": config.ratio,
                "media": [file_name],
                "settings": {
                    "color": config.color,
                    "align": config.align,
                    "filter": {
                        "value": null,
                        "opacity": config.filter_opacity
                    },
                    "badges": config.badges
                },
                "sysinfoDisplay": config.sysinfo_display
            }),
        )?;

        // Send several sysinfo updates to keep connection alive and display temps
        log::info!("Sending sysinfo updates to keep connection alive...");
        for i in 0..5 {
            thread::sleep(Duration::from_millis(800));
            self.send_sysinfo(&mut port)?;
            log::debug!("Sysinfo update {}/5", i + 1);
        }

        log::info!("Screen configuration sent successfully!");
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
