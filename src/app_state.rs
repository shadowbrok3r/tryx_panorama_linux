// ============================================================================
// GUI application state + the background-work plumbing every control uses.
// ============================================================================

use std::path::PathBuf;

use crossbeam::channel::{Receiver, Sender};

use crate::gallery::{self, Gallery};

#[derive(Debug)]
pub enum AppMessage {
    Success(String),
    Error(String),
    /// A worker fetched the device's media list.
    DeviceMedia(Vec<String>),
    /// A gallery-mutating action finished; reload the working copy from disk.
    GalleryChanged,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Tab {
    Gallery,
    Display,
    FanPump,
    System,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum FanMode {
    Smart,
    Fixed,
}

/// Main App Structure
pub struct AioCoolerApp {
    // Connection / persistence
    pub serial_device: String,
    pub gallery_path: PathBuf,

    // Working copies (the GUI's source of truth; pushed to the device on Apply)
    pub gallery: Gallery, // media order + play_mode + display config
    pub device_media: Vec<String>, // last adb-listed files (Gallery tab)

    // Gallery tab
    pub selected_image: Option<PathBuf>,
    pub replace_on_upload: bool,

    // Fan & Pump tab
    pub fan_mode: FanMode,
    pub fan_curve: Vec<(i32, i32)>,
    pub fan_raw: bool,
    pub fan_fixed_duty: u8,
    pub pump_enable: bool,
    pub pump_value: u32,

    // System tab
    pub brightness: u8,
    pub rotate_degree: i32,
    pub temp_fahrenheit: bool,
    pub cpu_name: String,
    pub gpu_name: String,
    pub power_event: String,
    pub display_in_sleep: bool,

    // UI
    pub current_tab: Tab,
    pub is_processing: bool,
    pub progress: f32,
    pub status_message: String,

    pub message_sender: Option<Sender<AppMessage>>,
    pub message_receiver: Receiver<AppMessage>,
}

impl Default for AioCoolerApp {
    fn default() -> Self {
        let (tx, rx) = crossbeam::channel::unbounded();
        let gallery_path = Gallery::resolve_path(None);
        let gallery = Gallery::load(&gallery_path).unwrap_or_default();
        Self {
            serial_device: "/dev/ttyACM0".to_string(),
            gallery_path,
            gallery,
            device_media: Vec::new(),
            selected_image: None,
            replace_on_upload: false,
            fan_mode: FanMode::Smart,
            fan_curve: vec![(30, 30), (50, 40), (65, 55), (80, 70), (90, 100)],
            fan_raw: false,
            fan_fixed_duty: 45,
            pump_enable: false,
            pump_value: 65,
            brightness: 100,
            rotate_degree: 0,
            temp_fahrenheit: false,
            cpu_name: crate::commands::detect_cpu_name(),
            gpu_name: crate::commands::detect_gpu_name(),
            power_event: "suspend".to_string(),
            display_in_sleep: false,
            current_tab: Tab::Gallery,
            is_processing: false,
            progress: 0.0,
            status_message: "Ready".to_string(),
            message_sender: Some(tx),
            message_receiver: rx,
        }
    }
}

impl AioCoolerApp {
    pub fn process_messages(&mut self) {
        while let Ok(msg) = self.message_receiver.try_recv() {
            match msg {
                AppMessage::Success(msg) => {
                    self.is_processing = false;
                    self.progress = 1.0;
                    self.status_message = msg;
                }
                AppMessage::Error(msg) => {
                    self.is_processing = false;
                    self.progress = 0.0;
                    self.status_message = format!("Error: {msg}");
                    log::error!("{msg}");
                }
                AppMessage::DeviceMedia(media) => {
                    self.device_media = media;
                }
                AppMessage::GalleryChanged => {
                    // Reload media/play_mode from disk; display config is edited
                    // live in self.gallery.config and was already persisted by
                    // the action that fired this, so a reload stays consistent.
                    if let Ok(g) = Gallery::load(&self.gallery_path) {
                        self.gallery = g;
                    }
                }
            }
        }
    }

    /// Spawn a background action, serialized by `is_processing` (the serial port
    /// is exclusive). The closure gets a `Sender` so it can emit progress /
    /// device-media / gallery-changed messages; its `Result` becomes Success/Error.
    pub fn spawn_action<F>(&mut self, status: impl Into<String>, work: F)
    where
        F: FnOnce(Sender<AppMessage>) -> anyhow::Result<()> + Send + 'static,
    {
        if self.is_processing {
            return;
        }
        self.is_processing = true;
        self.progress = 0.0;
        self.status_message = status.into();
        let tx = self.message_sender.clone().unwrap();
        let tx_work = tx.clone();
        std::thread::spawn(move || match work(tx_work) {
            Ok(()) => {
                let _ = tx.send(AppMessage::Success("Done".to_string()));
            }
            Err(e) => {
                let _ = tx.send(AppMessage::Error(format!("{e:#}")));
            }
        });
    }

    /// Refresh the device media list (adb) in the background.
    pub fn refresh_device_media(&mut self) {
        self.spawn_action("Listing device media…", move |tx| {
            let media = gallery::list_device_media()?;
            let _ = tx.send(AppMessage::DeviceMedia(media));
            Ok(())
        });
    }
}
