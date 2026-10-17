// ============================================================================
// GUI application state + the background-work plumbing every control uses.
// ============================================================================

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam::channel::{Receiver, Sender};

use crate::gallery::{self, Gallery};
use crate::hub;
use crate::theme::{self, SavedTheme};

/// eframe storage key for the last device path.
pub const DEVICE_KEY: &str = "serial_device";
/// How often the route indicator re-checks for a daemon.
const PROBE_EVERY: Duration = Duration::from_secs(3);

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

/// How the last action ended, for coloring the status line.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum StatusKind {
    Idle,
    Busy,
    Ok,
    Error,
}

/// Where commands for the current device field end up.
#[derive(Clone, PartialEq, Eq)]
pub enum Route {
    /// A local daemon owns the port; commands go through its socket.
    Daemon(PathBuf),
    /// No daemon: commands open the serial device themselves.
    Direct,
    /// `tcp://` — a bridge on another machine.
    Remote,
    /// `unix://` — a daemon socket named explicitly.
    Socket,
}

impl Route {
    pub fn probe(device: &str) -> Self {
        if device.starts_with("tcp://") {
            Route::Remote
        } else if device.starts_with("unix://") {
            Route::Socket
        } else {
            hub::running_daemon().map_or(Route::Direct, Route::Daemon)
        }
    }
}

/// Main App Structure
pub struct AioCoolerApp {
    // Connection / persistence
    pub serial_device: String,
    pub gallery_path: PathBuf,
    pub route: Route,
    route_probed: (String, Instant),

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

    // Appearance: the installed theme (its glass material rides along on save)
    pub theme: SavedTheme,

    // UI
    pub current_tab: Tab,
    pub is_processing: bool,
    pub progress: f32,
    pub status_message: String,
    pub status_kind: StatusKind,

    pub message_sender: Option<Sender<AppMessage>>,
    pub message_receiver: Receiver<AppMessage>,
}

impl AioCoolerApp {
    pub fn new(serial_device: String, gallery_path: PathBuf, theme: SavedTheme) -> Self {
        let (tx, rx) = crossbeam::channel::unbounded();
        let gallery = Gallery::load(&gallery_path).unwrap_or_default();
        let route = Route::probe(&serial_device);
        Self {
            route_probed: (serial_device.clone(), Instant::now()),
            route,
            serial_device,
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
            theme,
            current_tab: Tab::Gallery,
            is_processing: false,
            progress: 0.0,
            status_message: "Ready".to_string(),
            status_kind: StatusKind::Idle,
            message_sender: Some(tx),
            message_receiver: rx,
        }
    }

    pub fn process_messages(&mut self) {
        while let Ok(msg) = self.message_receiver.try_recv() {
            match msg {
                AppMessage::Success(msg) => {
                    self.is_processing = false;
                    self.progress = 1.0;
                    self.status_message = msg;
                    self.status_kind = StatusKind::Ok;
                }
                AppMessage::Error(msg) => {
                    self.is_processing = false;
                    self.progress = 0.0;
                    self.status_message = format!("Error: {msg}");
                    self.status_kind = StatusKind::Error;
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

    /// Re-check where commands go when the device field changed, or every few seconds.
    pub fn refresh_route(&mut self) {
        let (probed_for, at) = &self.route_probed;
        if *probed_for != self.serial_device || at.elapsed() >= PROBE_EVERY {
            self.route = Route::probe(&self.serial_device);
            self.route_probed = (self.serial_device.clone(), Instant::now());
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
        self.status_kind = StatusKind::Busy;
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
        let dev = self.serial_device.clone();
        self.spawn_action("Listing device media…", move |tx| {
            let media = gallery::list_device_media(&dev)?;
            let _ = tx.send(AppMessage::DeviceMedia(media));
            Ok(())
        });
    }

    /// Install `theme` live and remember it as the user's theme.
    pub fn set_theme(&mut self, ctx: &eframe::egui::Context, theme: SavedTheme) {
        theme::apply(ctx, &theme);
        let path = theme::user_theme_path();
        match theme::save(&theme, &path) {
            Ok(()) => log::info!("Theme saved to {}", path.display()),
            Err(e) => log::warn!("Theme applied but not saved: {e:#}"),
        }
        self.theme = theme;
    }

    /// Back to the bundled Mastertech theme, forgetting the user's file.
    pub fn reset_theme(&mut self, ctx: &eframe::egui::Context) {
        let path = theme::user_theme_path();
        match std::fs::remove_file(&path) {
            Ok(()) => log::info!("Removed {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("Could not remove {}: {e}", path.display()),
        }
        self.theme = theme::bundled();
        theme::apply(ctx, &self.theme);
    }
}
