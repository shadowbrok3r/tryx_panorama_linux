// ============================================================================
// GUI application state + the background-work plumbing every control uses.
// ============================================================================

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam::channel::{Receiver, Sender};
use eframe::egui::{ColorImage, Context, TextureHandle, TextureOptions};

use crate::gallery::{self, Gallery};
use crate::hub;
use crate::screen_setup;
use crate::theme::{self, SavedTheme};

/// eframe storage key for the last device path.
pub const DEVICE_KEY: &str = "serial_device";
/// How often the route indicator re-checks for a daemon.
const PROBE_EVERY: Duration = Duration::from_secs(3);
/// How often the "Now showing" preview re-captures while auto-refresh is on.
pub const PREVIEW_EVERY: Duration = Duration::from_secs(5);
/// How long after an action the preview re-captures: the panel needs a moment
/// to load and draw the new layout.
const PREVIEW_SETTLE: Duration = Duration::from_millis(1500);
/// Screenshots are scaled down to this width before becoming a texture.
const PREVIEW_MAX_W: u32 = 1280;

pub enum AppMessage {
    Success(String),
    Error(String),
    /// A worker fetched the device's media list.
    DeviceMedia(Vec<String>),
    /// A gallery-mutating action finished; reload the working copy from disk.
    GalleryChanged,
    /// A screen capture finished: the frame and the panel's native size.
    Screen(Result<(ColorImage, [usize; 2]), String>),
}

/// The "Now showing" preview: the latest capture of the cooler's panel.
pub struct Preview {
    pub texture: Option<TextureHandle>,
    /// The panel's native resolution, from the latest capture.
    pub native: [usize; 2],
    pub captured_at: Option<chrono::DateTime<chrono::Local>>,
    pub error: Option<String>,
    pub auto: bool,
    /// The section is expanded, so captures are worth taking.
    pub visible: bool,
    in_flight: bool,
    last_attempt: Option<Instant>,
    /// An action just changed the panel: capture once this passes.
    refresh_at: Option<Instant>,
}

impl Preview {
    pub fn is_capturing(&self) -> bool {
        self.in_flight
    }
}

impl Default for Preview {
    fn default() -> Self {
        Self {
            texture: None,
            native: [0, 0],
            captured_at: None,
            error: None,
            auto: true,
            visible: true,
            in_flight: false,
            last_attempt: None,
            refresh_at: None,
        }
    }
}

/// Decode a screenshot, scaled down to at most [`PREVIEW_MAX_W`] wide.
fn decode_preview(png: &[u8]) -> anyhow::Result<(ColorImage, [usize; 2])> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)?;
    let native = [img.width() as usize, img.height() as usize];
    let img = if img.width() > PREVIEW_MAX_W {
        img.resize(PREVIEW_MAX_W, img.height(), image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok((ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()), native))
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

    /// What the panel shows right now.
    pub preview: Preview,

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
            preview: Preview::default(),
            current_tab: Tab::Gallery,
            is_processing: false,
            progress: 0.0,
            status_message: "Ready".to_string(),
            status_kind: StatusKind::Idle,
            message_sender: Some(tx),
            message_receiver: rx,
        }
    }

    pub fn process_messages(&mut self, ctx: &Context) {
        while let Ok(msg) = self.message_receiver.try_recv() {
            match msg {
                AppMessage::Success(msg) => {
                    self.is_processing = false;
                    self.progress = 1.0;
                    self.status_message = msg;
                    self.status_kind = StatusKind::Ok;
                    self.preview.refresh_at = Some(Instant::now() + PREVIEW_SETTLE);
                }
                AppMessage::Error(msg) => {
                    self.is_processing = false;
                    self.progress = 0.0;
                    self.status_message = format!("Error: {msg}");
                    self.status_kind = StatusKind::Error;
                    log::error!("{msg}");
                    self.preview.refresh_at = Some(Instant::now() + PREVIEW_SETTLE);
                }
                AppMessage::Screen(shot) => {
                    self.preview.in_flight = false;
                    match shot {
                        Ok((image, native)) => {
                            match &mut self.preview.texture {
                                Some(texture) => texture.set(image, TextureOptions::LINEAR),
                                None => {
                                    self.preview.texture = Some(ctx.load_texture(
                                        "tryx-screen",
                                        image,
                                        TextureOptions::LINEAR,
                                    ))
                                }
                            }
                            self.preview.native = native;
                            self.preview.captured_at = Some(chrono::Local::now());
                            if self.preview.error.take().is_some() {
                                log::info!("Screen preview is back");
                            }
                        }
                        Err(e) => {
                            // Auto-refresh retries quietly; log only when the reason changes.
                            if self.preview.error.as_deref() != Some(e.as_str()) {
                                log::warn!("Screen preview: {e}");
                            }
                            self.preview.error = Some(e);
                        }
                    }
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

    /// Capture the panel in the background, unless a capture is running. Runs
    /// beside `spawn_action` work: it only reads the framebuffer over adb.
    pub fn capture_preview(&mut self, ctx: &Context) {
        if self.preview.in_flight {
            return;
        }
        self.preview.in_flight = true;
        self.preview.last_attempt = Some(Instant::now());
        let dev = self.serial_device.clone();
        let tx = self.message_sender.clone().unwrap();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let shot = screen_setup::capture_screen(&dev)
                .and_then(|png| decode_preview(&png))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppMessage::Screen(shot));
            ctx.request_repaint();
        });
    }

    /// Start a capture if auto-refresh or a finished action calls for one.
    /// Returns how long until the next one is due, for scheduling a repaint.
    pub fn schedule_preview(&mut self, ctx: &Context) -> Option<Duration> {
        let p = &self.preview;
        if !p.visible || p.in_flight {
            return None;
        }
        let now = Instant::now();
        let auto = p.auto.then(|| p.last_attempt.map_or(now, |t| t + PREVIEW_EVERY));
        let due = [auto, p.refresh_at].into_iter().flatten().min()?;
        if due > now {
            return Some(due - now);
        }
        self.preview.refresh_at = None;
        self.capture_preview(ctx);
        None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panel_screenshot_decodes_scaled_to_the_preview_width() {
        let shot = image::RgbaImage::from_pixel(2160, 1080, image::Rgba([200, 30, 90, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(shot)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let (frame, native) = decode_preview(&png).unwrap();
        assert_eq!(native, [2160, 1080]);
        assert_eq!(frame.size, [1280, 640]);
        assert_eq!(frame.pixels[0].to_srgba_unmultiplied(), [200, 30, 90, 255]);
        assert!(decode_preview(b"not a png").is_err());
    }
}
