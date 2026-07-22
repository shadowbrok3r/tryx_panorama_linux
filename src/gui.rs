// ============================================================================
// Desktop GUI (egui) — built only with `--features gui`.
//
// Four tabs surface the full control set. Every action button clones the state
// it needs and calls a `commands::*` fn on a background thread via
// `spawn_action` (each command opens/closes its own port; `is_processing`
// serializes them because the serial port is exclusive). Works locally or over
// the LAN by setting the Serial Device to `tcp://host:9600` (see `bridge`).
// ============================================================================

use eframe::egui;

use crate::app_state::{self, AioCoolerApp, AppMessage, FanMode, Tab};
use crate::commands;

impl eframe::App for AioCoolerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_messages();
        if self.is_processing {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading("Tryx Panorama Controller");
            ui.horizontal(|ui| {
                ui.label("Device:");
                ui.text_edit_singleline(&mut self.serial_device);
                ui.label("(tcp://host:9600 for a remote bridge)");
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.current_tab, Tab::Gallery, "🖼 Gallery");
                ui.selectable_value(&mut self.current_tab, Tab::Display, "🎨 Display");
                ui.selectable_value(&mut self.current_tab, Tab::FanPump, "🌀 Fan & Pump");
                ui.selectable_value(&mut self.current_tab, Tab::System, "⚙ System");
            });
            ui.add_space(4.0);
        });

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

        egui::Panel::left("log_panel")
            .resizable(true)
            .default_size(280.0)
            .show(ui, |ui| {
                ui.heading("📋 Logs");
                ui.separator();
                egui_logger::logger_ui().show(ui);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.current_tab {
                Tab::Gallery => self.gallery_tab(ui),
                Tab::Display => self.display_tab(ui),
                Tab::FanPump => self.fan_pump_tab(ui),
                Tab::System => self.system_tab(ui),
            });
        });
    }
}

impl AioCoolerApp {
    // ---- Gallery -----------------------------------------------------------
    fn gallery_tab(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.heading("Add image");
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Browse…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "gif", "bmp"])
                        .pick_file()
                    {
                        self.selected_image = Some(path);
                    }
                }
                match &self.selected_image {
                    Some(p) => ui.label(format!("{}", p.display())),
                    None => ui.label("No image selected"),
                };
            });
            ui.checkbox(&mut self.replace_on_upload, "Replace (wipe every other file)");
            let can_upload = !self.is_processing && self.selected_image.is_some();
            if ui
                .add_enabled(can_upload, egui::Button::new("🚀 Upload & add to gallery"))
                .clicked()
            {
                let dev = self.serial_device.clone();
                let path = self.selected_image.clone().unwrap();
                let cfg = self.gallery.config.clone();
                let gp = self.gallery_path.clone();
                let replace = self.replace_on_upload;
                self.spawn_action("Uploading image…", move |tx| {
                    commands::image(&dev, &path, &cfg, &gp, replace)?;
                    let _ = tx.send(AppMessage::GalleryChanged);
                    Ok(())
                });
            }
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("Playlist");
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Play mode:");
                egui::ComboBox::from_id_salt("play_mode")
                    .selected_text(&self.gallery.play_mode)
                    .show_ui(ui, |ui| {
                        for m in ["Single", "Loop", "Shuffle"] {
                            ui.selectable_value(&mut self.gallery.play_mode, m.to_string(), m);
                        }
                    });
            });

            if self.gallery.media.is_empty() {
                ui.label("(playlist empty — upload an image above)");
            }

            let mut move_up: Option<usize> = None;
            let mut move_down: Option<usize> = None;
            let mut remove: Option<String> = None;
            let len = self.gallery.media.len();
            for (i, name) in self.gallery.media.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("{}.", i + 1));
                    if ui.add_enabled(i > 0, egui::Button::new("↑")).clicked() {
                        move_up = Some(i);
                    }
                    if ui.add_enabled(i + 1 < len, egui::Button::new("↓")).clicked() {
                        move_down = Some(i);
                    }
                    if ui.button("🗑").clicked() {
                        remove = Some(name.clone());
                    }
                    let on_device = self.device_media.is_empty()
                        || self.device_media.iter().any(|d| d == name);
                    ui.label(if on_device {
                        name.clone()
                    } else {
                        format!("{name}  [missing on device]")
                    });
                });
            }
            if let Some(i) = move_up {
                self.gallery.media.swap(i - 1, i);
            }
            if let Some(i) = move_down {
                self.gallery.media.swap(i, i + 1);
            }
            if let Some(name) = remove {
                let dev = self.serial_device.clone();
                let gp = self.gallery_path.clone();
                self.spawn_action(format!("Removing {name}…"), move |tx| {
                    commands::gallery_rm(&dev, &name, &gp)?;
                    let _ = tx.send(AppMessage::GalleryChanged);
                    Ok(())
                });
            }
        });

        ui.add_space(8.0);

        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.is_processing, egui::Button::new("✔ Apply to device"))
                .clicked()
            {
                self.apply_gallery();
            }
            if ui
                .add_enabled(!self.is_processing, egui::Button::new("⟳ Refresh device list"))
                .clicked()
            {
                self.refresh_device_media();
            }
            if ui
                .add_enabled(!self.is_processing, egui::Button::new("🧹 Clear gallery"))
                .clicked()
            {
                let dev = self.serial_device.clone();
                let gp = self.gallery_path.clone();
                self.spawn_action("Clearing gallery…", move |tx| {
                    commands::gallery_clear(&dev, &gp)?;
                    let _ = tx.send(AppMessage::GalleryChanged);
                    Ok(())
                });
            }
        });
    }

    // ---- Display -----------------------------------------------------------
    fn display_tab(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.heading("Layout");
            ui.separator();
            egui::Grid::new("display_grid").num_columns(2).spacing([20.0, 8.0]).show(ui, |ui| {
                ui.label("Ratio:");
                egui::ComboBox::from_id_salt("ratio")
                    .selected_text(&self.gallery.config.ratio)
                    .show_ui(ui, |ui| {
                        for r in ["1:1", "2:1", "3:2", "4:3", "16:9"] {
                            ui.selectable_value(&mut self.gallery.config.ratio, r.to_string(), r);
                        }
                    });
                ui.end_row();

                ui.label("Alignment:");
                egui::ComboBox::from_id_salt("align")
                    .selected_text(&self.gallery.config.align)
                    .show_ui(ui, |ui| {
                        for a in ["Left", "Center", "Right"] {
                            ui.selectable_value(&mut self.gallery.config.align, a.to_string(), a);
                        }
                    });
                ui.end_row();

                ui.label("Color (hex):");
                ui.text_edit_singleline(&mut self.gallery.config.color);
                ui.end_row();

                ui.label("Filter:");
                let mut filter =
                    self.gallery.config.filter_value.clone().unwrap_or_else(|| "None".to_string());
                egui::ComboBox::from_id_salt("filter")
                    .selected_text(&filter)
                    .show_ui(ui, |ui| {
                        for f in ["None", "Rain", "Smoke"] {
                            ui.selectable_value(&mut filter, f.to_string(), f);
                        }
                    });
                self.gallery.config.filter_value =
                    if filter == "None" { None } else { Some(filter) };
                ui.end_row();

                ui.label("Filter opacity:");
                ui.add(
                    egui::Slider::new(&mut self.gallery.config.filter_opacity, 0..=100).suffix("%"),
                );
                ui.end_row();
            });
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("Badges");
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                for badge in ["CPU Badge", "GPU Badge"] {
                    let mut on = self.gallery.config.badges.iter().any(|b| b == badge);
                    if ui.checkbox(&mut on, badge).changed() {
                        if on {
                            self.gallery.config.badges.push(badge.to_string());
                        } else {
                            self.gallery.config.badges.retain(|b| b != badge);
                        }
                    }
                }
            });
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("Sysinfo overlays");
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                for &metric in commands::SYSINFO_METRICS {
                    let mut on = self.gallery.config.sysinfo_display.iter().any(|m| m == metric);
                    if ui.checkbox(&mut on, metric).changed() {
                        if on {
                            self.gallery.config.sysinfo_display.push(metric.to_string());
                        } else {
                            self.gallery.config.sysinfo_display.retain(|m| m != metric);
                        }
                    }
                }
            });
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.is_processing, egui::Button::new("✔ Apply display"))
                .clicked()
            {
                self.apply_gallery();
            }
            if ui
                .add_enabled(!self.is_processing, egui::Button::new("Push overlays only"))
                .clicked()
            {
                let dev = self.serial_device.clone();
                let items = self.gallery.config.sysinfo_display.clone();
                self.spawn_action("Pushing overlays…", move |_tx| {
                    commands::sysinfo_display(&dev, &items, 2)
                });
            }
        });
    }

    // ---- Fan & Pump --------------------------------------------------------
    fn fan_pump_tab(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.heading("LCD fan");
            ui.separator();
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.fan_mode, FanMode::Smart, "Smart curve");
                ui.selectable_value(&mut self.fan_mode, FanMode::Fixed, "Fixed duty");
            });

            match self.fan_mode {
                FanMode::Smart => {
                    ui.label("Temperature (°C) → duty (%) points:");
                    let mut remove: Option<usize> = None;
                    for i in 0..self.fan_curve.len() {
                        ui.horizontal(|ui| {
                            ui.add(egui::DragValue::new(&mut self.fan_curve[i].0).prefix("T "));
                            ui.add(egui::DragValue::new(&mut self.fan_curve[i].1).suffix("%"));
                            if ui.button("🗑").clicked() {
                                remove = Some(i);
                            }
                        });
                    }
                    if let Some(i) = remove {
                        self.fan_curve.remove(i);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("+ Add point").clicked() {
                            self.fan_curve.push((60, 50));
                        }
                        ui.checkbox(&mut self.fan_raw, "raw (no ceiling sentinel)");
                    });
                    if ui
                        .add_enabled(!self.is_processing, egui::Button::new("✔ Apply curve"))
                        .clicked()
                    {
                        let dev = self.serial_device.clone();
                        let curve = self.fan_curve.clone();
                        let raw = self.fan_raw;
                        self.spawn_action("Applying fan curve…", move |_tx| {
                            commands::fan_smart(&dev, curve, raw, 2)
                        });
                    }
                }
                FanMode::Fixed => {
                    ui.add(egui::Slider::new(&mut self.fan_fixed_duty, 0..=100).text("duty %"));
                    if ui
                        .add_enabled(!self.is_processing, egui::Button::new("✔ Apply fixed duty"))
                        .clicked()
                    {
                        let dev = self.serial_device.clone();
                        let duty = self.fan_fixed_duty;
                        self.spawn_action("Applying fixed duty…", move |_tx| {
                            commands::fan_fixed(&dev, duty, 2)
                        });
                    }
                }
            }
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("Turbo pump");
            ui.separator();
            ui.checkbox(&mut self.pump_enable, "Enable turbo");
            ui.add(egui::Slider::new(&mut self.pump_value, 0..=255).text("PWM"));
            if ui
                .add_enabled(!self.is_processing, egui::Button::new("✔ Apply pump"))
                .clicked()
            {
                let dev = self.serial_device.clone();
                let (en, val) = (self.pump_enable, self.pump_value);
                self.spawn_action("Applying pump…", move |_tx| commands::pump(&dev, en, val, 2));
            }
        });
    }

    // ---- System ------------------------------------------------------------
    fn system_tab(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.heading("Display panel");
            ui.separator();
            ui.horizontal(|ui| {
                ui.add(egui::Slider::new(&mut self.brightness, 0..=100).text("brightness %"));
                if ui.add_enabled(!self.is_processing, egui::Button::new("Set")).clicked() {
                    let dev = self.serial_device.clone();
                    let v = self.brightness;
                    self.spawn_action("Setting brightness…", move |_tx| {
                        commands::brightness(&dev, v, 2)
                    });
                }
            });
            ui.horizontal(|ui| {
                ui.label("Panel power:");
                if ui.add_enabled(!self.is_processing, egui::Button::new("On")).clicked() {
                    let dev = self.serial_device.clone();
                    self.spawn_action("Panel on…", move |_tx| commands::screen_power(&dev, true, 2));
                }
                if ui.add_enabled(!self.is_processing, egui::Button::new("Off")).clicked() {
                    let dev = self.serial_device.clone();
                    self.spawn_action("Panel off…", move |_tx| {
                        commands::screen_power(&dev, false, 2)
                    });
                }
            });
            ui.horizontal(|ui| {
                if ui.checkbox(&mut self.display_in_sleep, "Show while PC sleeps").changed() {
                    let dev = self.serial_device.clone();
                    let on = self.display_in_sleep;
                    self.spawn_action("Setting display-in-sleep…", move |_tx| {
                        commands::display_in_sleep(&dev, on, 2)
                    });
                }
            });
            ui.horizontal(|ui| {
                ui.label("Rotation:");
                egui::ComboBox::from_id_salt("rotate")
                    .selected_text(format!("{}°", self.rotate_degree))
                    .show_ui(ui, |ui| {
                        for d in [0, 90, 180, 270] {
                            ui.selectable_value(&mut self.rotate_degree, d, format!("{d}°"));
                        }
                    });
                if ui.add_enabled(!self.is_processing, egui::Button::new("Apply")).clicked() {
                    let dev = self.serial_device.clone();
                    let d = self.rotate_degree;
                    self.spawn_action("Rotating…", move |_tx| commands::rotate(&dev, d, 2));
                }
            });
            ui.horizontal(|ui| {
                if ui.checkbox(&mut self.temp_fahrenheit, "Fahrenheit").changed() {
                    let dev = self.serial_device.clone();
                    let unit = if self.temp_fahrenheit { "Fahrenheit" } else { "Celsius" };
                    self.spawn_action("Setting temp unit…", move |_tx| {
                        commands::temperature(&dev, unit, 2)
                    });
                }
            });
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("Badges (CPU/GPU names)");
            ui.separator();
            egui::Grid::new("spec_grid").num_columns(2).show(ui, |ui| {
                ui.label("CPU:");
                ui.text_edit_singleline(&mut self.cpu_name);
                ui.end_row();
                ui.label("GPU:");
                ui.text_edit_singleline(&mut self.gpu_name);
                ui.end_row();
            });
            ui.horizontal(|ui| {
                if ui.button("Auto-detect").clicked() {
                    self.cpu_name = commands::detect_cpu_name();
                    self.gpu_name = commands::detect_gpu_name();
                }
                if ui.add_enabled(!self.is_processing, egui::Button::new("✔ Apply names")).clicked()
                {
                    let dev = self.serial_device.clone();
                    let (cpu, gpu) = (self.cpu_name.clone(), self.gpu_name.clone());
                    self.spawn_action("Setting badge names…", move |_tx| {
                        commands::spec(&dev, Some(cpu), Some(gpu), 2)
                    });
                }
            });
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("Power event / disconnect");
            ui.separator();
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("power_event")
                    .selected_text(&self.power_event)
                    .show_ui(ui, |ui| {
                        for e in ["suspend", "shutdown", "lock-screen", "resume", "unlock-screen"] {
                            ui.selectable_value(&mut self.power_event, e.to_string(), e);
                        }
                    });
                if ui.add_enabled(!self.is_processing, egui::Button::new("Send event")).clicked() {
                    let dev = self.serial_device.clone();
                    let ev = self.power_event.clone();
                    self.spawn_action("Sending power event…", move |_tx| {
                        commands::power(&dev, &ev, 2)
                    });
                }
                if ui
                    .add_enabled(!self.is_processing, egui::Button::new("Disconn (screen off)"))
                    .clicked()
                {
                    let dev = self.serial_device.clone();
                    self.spawn_action("Disconnecting…", move |_tx| commands::disconn(&dev, 2));
                }
            });
        });
    }

    /// Persist + push the current in-memory gallery (media + play_mode + config).
    fn apply_gallery(&mut self) {
        let dev = self.serial_device.clone();
        let g = self.gallery.clone();
        let gp = self.gallery_path.clone();
        self.spawn_action("Applying gallery…", move |tx| {
            commands::gallery_write_apply(&dev, &g, &gp)?;
            let _ = tx.send(AppMessage::GalleryChanged);
            Ok(())
        });
    }
}

/// Launch the desktop GUI.
pub fn run() -> anyhow::Result<()> {
    egui_logger::builder().max_level(log::LevelFilter::Info).init().unwrap();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 720.0])
            .with_min_inner_size([700.0, 460.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Tryx Panorama Controller",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(app_state::AioCoolerApp::default()))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}
