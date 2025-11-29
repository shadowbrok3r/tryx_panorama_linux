use std::{path::PathBuf, process::Command, sync::mpsc::{self, Receiver, Sender}, thread, time::{Duration, SystemTime, UNIX_EPOCH}};
use crate::screen_setup::{AioCoolerController, ScreenConfig};
use serde::{Deserialize, Serialize};
use eframe::egui::{self, Color32};
use anyhow::{Context, Result};
use std::io::{Read, Write};
use egui_logger::logger_ui;

pub mod screen_setup;
pub mod data;

// ============================================================================
// App Messages
// ============================================================================

#[derive(Debug)]
enum AppMessage {
    Log(String),
    Progress(f32, String),
    Success(String),
    Error(String),
}

/// Main App Structure
struct AioCoolerApp {

    serial_device: String,
    selected_image: Option<PathBuf>,
    screen_config: ScreenConfig,


    is_processing: bool,
    progress: f32,
    status_message: String,
    log_messages: Vec<String>,


    message_sender: Option<Sender<AppMessage>>,
    message_receiver: Receiver<AppMessage>,
}

impl Default for AioCoolerApp {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            serial_device: "/dev/ttyACM0".to_string(),
            selected_image: None,
            screen_config: ScreenConfig::default(),
            is_processing: false,
            progress: 0.0,
            status_message: "Ready".to_string(),
            log_messages: Vec::new(),
            message_sender: Some(tx),
            message_receiver: rx,
        }
    }
}

impl AioCoolerApp {
    fn process_messages(&mut self) {
        while let Ok(msg) = self.message_receiver.try_recv() {
            match msg {
                AppMessage::Log(text) => {
                    self.log_messages.push(text);
                    if self.log_messages.len() > 100 {
                        self.log_messages.remove(0);
                    }
                }
                AppMessage::Progress(progress, status) => {
                    self.progress = progress;
                    self.status_message = status;
                }
                AppMessage::Success(msg) => {
                    self.is_processing = false;
                    self.progress = 1.0;
                    self.status_message = msg;
                }
                AppMessage::Error(msg) => {
                    self.is_processing = false;
                    self.progress = 0.0;
                    self.status_message = format!("Error: {}", msg);
                }
            }
        }
    }

    fn start_transfer(&mut self) {
        if self.is_processing {
            return;
        }

        let Some(image_path) = self.selected_image.clone() else {
            self.status_message = "No image selected".to_string();
            return;
        };

        self.is_processing = true;
        self.progress = 0.0;
        self.status_message = "Starting transfer...".to_string();

        let serial_device = self.serial_device.clone();
        let config = self.screen_config.clone();
        let tx = self.message_sender.clone().unwrap();

        thread::spawn(move || {
            let result = (|| -> Result<()> {
                let _ = tx.send(AppMessage::Progress(0.1, "Calculating MD5...".to_string()));
                let _ = tx.send(AppMessage::Log("Calculating file MD5...".to_string()));

                let file_md5 = AioCoolerController::calculate_md5(&image_path)?;
                let file_size = std::fs::metadata(&image_path)?.len();

                let extension = image_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("png");
                let remote_name = AioCoolerController::generate_filename(extension);

                let _ = tx.send(AppMessage::Log(format!(
                    "File: {} ({} bytes, MD5: {})",
                    image_path.display(),
                    file_size,
                    file_md5
                )));

                let _ = tx.send(AppMessage::Progress(0.2, "Pushing to device via ADB...".to_string()));
                let _ = tx.send(AppMessage::Log("Starting ADB push...".to_string()));

                let controller = AioCoolerController::new(&serial_device);
                controller.adb_push(&image_path, &remote_name)?;

                let _ = tx.send(AppMessage::Progress(0.5, "Sending serial commands...".to_string()));
                let _ = tx.send(AppMessage::Log("Sending serial commands...".to_string()));

                controller.send_image_commands(&remote_name, file_size, &file_md5, &config)?;

                let _ = tx.send(AppMessage::Log("Transfer complete!".to_string()));
                Ok(())
            })();

            match result {
                Ok(()) => {
                    let _ = tx.send(AppMessage::Success("Transfer complete!".to_string()));
                }
                Err(e) => {
                    let _ = tx.send(AppMessage::Error(format!("{:#}", e)));
                }
            }
        });
    }
}

impl eframe::App for AioCoolerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_messages();

        if self.is_processing {
            ctx.request_repaint();
        }

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("Tryx Panorama Display Controller");
            });
            ui.add_space(4.0);
        });

        // Bottom panel - Status and progress
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(&self.status_message);
                if self.is_processing {
                    ui.spinner();
                }
            });
            if self.is_processing || self.progress > 0.0 {
                ui.add(egui::ProgressBar::new(self.progress).show_percentage());
            }
            ui.add_space(4.0);
        });

        // Left panel - Log
        egui::SidePanel::left("log_panel")
            .resizable(true)
            .default_width(300.0)
            .show(ctx, |ui| {
                ui.heading("📋 Logs");
                ui.separator();

                egui_logger::logger_ui()
                .warn_color(Color32::from_rgb(94, 215, 221)) 
                .error_color(Color32::from_rgb(255, 55, 102)) 
                .log_levels([true, true, true, false, false])
                .show(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.group(|ui| {
                    ui.heading("⚙️ Device Settings");
                    ui.separator();

                    ui.horizontal(|ui| {
                        ui.label("Serial Device:");
                        ui.text_edit_singleline(&mut self.serial_device);
                    });
                });

                ui.add_space(10.0);

                ui.group(|ui| {
                    ui.heading("Image Selection");
                    ui.separator();

                    ui.horizontal(|ui| {
                        if ui.button("Browse...").clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("Images", &["png", "jpg", "jpeg", "gif", "bmp"])
                                .pick_file()
                            {
                                self.selected_image = Some(path);
                            }
                        }

                        if let Some(path) = &self.selected_image {
                            ui.label(format!("Selected: {}", path.display()));
                        } else {
                            ui.label("No image selected");
                        }
                    });
                });

                ui.add_space(10.0);

                ui.group(|ui| {
                    ui.heading("Screen Configuration");
                    ui.separator();

                    egui::Grid::new("screen_config_grid")
                        .num_columns(2)
                        .spacing([20.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Screen Mode:");
                            egui::ComboBox::from_id_salt("screen_mode")
                                .selected_text(&self.screen_config.screen_mode)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.screen_config.screen_mode,
                                        "Full Screen".to_string(),
                                        "Full Screen",
                                    );
                                    ui.selectable_value(
                                        &mut self.screen_config.screen_mode,
                                        "Window".to_string(),
                                        "Window",
                                    );
                                });
                            ui.end_row();

                            ui.label("Play Mode:");
                            egui::ComboBox::from_id_salt("play_mode")
                                .selected_text(&self.screen_config.play_mode)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.screen_config.play_mode,
                                        "Single".to_string(),
                                        "Single",
                                    );
                                    ui.selectable_value(
                                        &mut self.screen_config.play_mode,
                                        "Loop".to_string(),
                                        "Loop",
                                    );
                                    ui.selectable_value(
                                        &mut self.screen_config.play_mode,
                                        "Slideshow".to_string(),
                                        "Slideshow",
                                    );
                                });
                            ui.end_row();

                            ui.label("Ratio:");
                            egui::ComboBox::from_id_salt("ratio")
                                .selected_text(&self.screen_config.ratio)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.screen_config.ratio,
                                        "2:1".to_string(),
                                        "2:1",
                                    );
                                    ui.selectable_value(
                                        &mut self.screen_config.ratio,
                                        "16:9".to_string(),
                                        "16:9",
                                    );
                                    ui.selectable_value(
                                        &mut self.screen_config.ratio,
                                        "4:3".to_string(),
                                        "4:3",
                                    );
                                    ui.selectable_value(
                                        &mut self.screen_config.ratio,
                                        "1:1".to_string(),
                                        "1:1",
                                    );
                                });
                            ui.end_row();

                            ui.label("Alignment:");
                            egui::ComboBox::from_id_salt("align")
                                .selected_text(&self.screen_config.align)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.screen_config.align,
                                        "Left".to_string(),
                                        "Left",
                                    );
                                    ui.selectable_value(
                                        &mut self.screen_config.align,
                                        "Center".to_string(),
                                        "Center",
                                    );
                                    ui.selectable_value(
                                        &mut self.screen_config.align,
                                        "Right".to_string(),
                                        "Right",
                                    );
                                });
                            ui.end_row();

                            ui.label("Color:");
                            ui.text_edit_singleline(&mut self.screen_config.color);
                            ui.end_row();

                            ui.label("Filter Opacity:");
                            ui.add(egui::Slider::new(&mut self.screen_config.filter_opacity, 0..=100).suffix("%"));
                            ui.end_row();
                        });
                });

                ui.add_space(10.0);

                ui.group(|ui| {
                    ui.heading("🏷️ Overlays");
                    ui.separator();

                    ui.horizontal(|ui| {
                        ui.label("Badges:");
                    });

                    let badges = ["CPU Badge", "GPU Badge", "RAM Badge", "FPS Badge"];
                    ui.horizontal_wrapped(|ui| {
                        for badge in badges {
                            let mut enabled = self.screen_config.badges.contains(&badge.to_string());
                            if ui.checkbox(&mut enabled, badge).changed() {
                                if enabled {
                                    self.screen_config.badges.push(badge.to_string());
                                } else {
                                    self.screen_config.badges.retain(|b| b != badge);
                                }
                            }
                        }
                    });

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.label("System Info:");
                    });

                    let sysinfo_options = [
                        "CPU Temperature",
                        "GPU Temperature",
                        "CPU Usage",
                        "GPU Usage",
                        "RAM Usage",
                        "Fan Speed",
                    ];
                    ui.horizontal_wrapped(|ui| {
                        for info in sysinfo_options {
                            let mut enabled = self.screen_config.sysinfo_display.contains(&info.to_string());
                            if ui.checkbox(&mut enabled, info).changed() {
                                if enabled {
                                    self.screen_config.sysinfo_display.push(info.to_string());
                                } else {
                                    self.screen_config.sysinfo_display.retain(|i| i != info);
                                }
                            }
                        }
                    });
                });

                ui.add_space(20.0);

                // Transfer Button
                ui.horizontal(|ui| {
                    let button = egui::Button::new("🚀 Transfer Image to Cooler")
                        .min_size(egui::vec2(200.0, 40.0));

                    let enabled = !self.is_processing && self.selected_image.is_some();

                    if ui.add_enabled(enabled, button).clicked() {
                        self.start_transfer();
                    }
                });
            });
        });
    }
}

// ============================================================================
// Main Entry Point
// ============================================================================

fn main() -> eframe::Result {
    env_logger::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 700.0])
            .with_min_inner_size([600.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Tryx Panorama Display Controller",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(AioCoolerApp::default()))
        }),
    )
}
