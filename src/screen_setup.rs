use std::{path::PathBuf, process::Command, sync::mpsc::{self, Receiver, Sender}, thread, time::{Duration, SystemTime, UNIX_EPOCH}};
use serde::{Deserialize, Serialize};
use eframe::egui::{self, Color32};
use anyhow::{Context, Result};
use std::io::{Read, Write};
use egui_logger::logger_ui;

use crate::data::send_command;

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

        log::info!("ADB push successful");
        Ok(())
    }

    pub fn send_image_commands(
        &self,
        file_name: &str,
        file_size: u64,
        file_md5: &str,
        config: &ScreenConfig,
    ) -> Result<()> {
        log::info!("Opening serial port: {}", self.serial_device);

        let mut port = serialport::new(&self.serial_device, 115200)
            .timeout(Duration::from_secs(2))
            .open()
            .context("Failed to open serial port")?;

        // Clear buffers
        thread::sleep(Duration::from_millis(500));
        let _ = port.clear(serialport::ClearBuffer::All);

        // Send transport command
        send_command(
            &mut port,
            "transport",
            &serde_json::json!({
                "type": "media",
                "fileSize": file_size,
                "fileName": file_name
            }),
        )?;

        thread::sleep(Duration::from_millis(300));

        // Send transported command
        send_command(
            &mut port,
            "transported",
            &serde_json::json!({
                "md5": file_md5,
                "fileName": file_name
            }),
        )?;

        thread::sleep(Duration::from_millis(300));

        // Send screen config
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

        thread::sleep(Duration::from_millis(500));
        log::info!("All commands sent successfully!");

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
