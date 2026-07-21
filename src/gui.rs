// ============================================================================
// Desktop GUI (egui) — built only with `--features gui`
// ============================================================================

use eframe::egui;

use crate::app_state;

impl eframe::App for app_state::AioCoolerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_messages();

        if self.is_processing {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("Tryx Panorama Display Controller");
            });
            ui.add_space(4.0);
        });

        // Bottom panel - Status and progress
        egui::Panel::bottom("status").show(ui, |ui| {
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
        egui::Panel::left("log_panel")
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                ui.heading("📋 Logs");
                ui.separator();

                egui_logger::logger_ui().show(ui);
            });

        egui::CentralPanel::default().show(ui, |ui| {
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

/// Launch the desktop GUI.
pub fn run() -> anyhow::Result<()> {
    egui_logger::builder().max_level(log::LevelFilter::Info).init().unwrap();

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
            Ok(Box::new(app_state::AioCoolerApp::default()))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}
