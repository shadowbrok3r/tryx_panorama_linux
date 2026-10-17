// ============================================================================
// Desktop GUI (egui) — built only with `--features gui`.
//
// Four tabs surface the full control set. Every action button clones the state
// it needs and calls a `commands::*` fn on a background thread via
// `spawn_action` (each command opens/closes its own link; `is_processing`
// serializes them). While the daemon runs, those links go through its socket:
// it owns the serial port and keeps the display alive between actions. Without
// it the GUI opens the port itself, and the panel blanks ~60 s after the last
// command — the header says which. Works over the LAN by setting the Device to
// `tcp://host:9600` (see `bridge`). "Now showing" mirrors the panel through adb
// screencaps. Styled with the bundled Mastertech theme (see `theme`).
// ============================================================================

use std::path::PathBuf;

use eframe::egui::{self, RichText};

use crate::app_state::{self, AioCoolerApp, AppMessage, FanMode, Route, StatusKind, Tab};
use crate::commands;
use crate::theme::{self, colors, fonts::TextScale, icons::p, metrics::Density, tokens::Tokens};

/// Tallest the "Now showing" preview is drawn, in points.
const PREVIEW_MAX_H: f32 = 200.0;

impl eframe::App for AioCoolerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_messages(ctx);
        self.refresh_route();
        let next_capture = self.schedule_preview(ctx);
        if self.is_processing {
            ctx.request_repaint();
        } else {
            // Keeps the route badge current (a daemon started/stopped meanwhile)
            // and wakes up for the next preview capture.
            let badge = std::time::Duration::from_secs(3);
            ctx.request_repaint_after(next_capture.map_or(badge, |t| t.min(badge)));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(app_state::DEVICE_KEY, self.serial_device.clone());
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading("Tryx Panorama Controller");
            ui.horizontal(|ui| {
                ui.label("Device:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.serial_device)
                        .hint_text("/dev/ttyACM0 or tcp://host:9600")
                        .desired_width(240.0),
                );
                self.route_badge(ui);
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.current_tab, Tab::Gallery, format!("{} Gallery", p::IMAGE));
                ui.selectable_value(&mut self.current_tab, Tab::Display, format!("{} Display", p::PALETTE));
                ui.selectable_value(&mut self.current_tab, Tab::FanPump, format!("{} Fan & Pump", p::FAN));
                ui.selectable_value(&mut self.current_tab, Tab::System, format!("{} System", p::GEAR));
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let t = Tokens::get(ui.ctx());
                let (icon, color) = match self.status_kind {
                    StatusKind::Idle => (p::INFO, t.text_dim),
                    StatusKind::Busy => (p::HOURGLASS_SIMPLE, t.text),
                    StatusKind::Ok => (p::CHECK_CIRCLE, t.success),
                    StatusKind::Error => (p::WARNING_CIRCLE, t.error),
                };
                ui.label(RichText::new(format!("{icon} {}", self.status_message)).color(color));
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
                ui.heading(format!("{} Logs", p::SCROLL));
                ui.separator();
                let t = Tokens::get(ui.ctx());
                egui_logger::logger_ui()
                    .warn_color(t.warn)
                    .error_color(t.error)
                    .show(ui);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            self.preview_section(ui);
            ui.add_space(4.0);
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
    /// Where commands for the Device field go, and what that means for the display.
    fn route_badge(&self, ui: &mut egui::Ui) {
        let t = Tokens::get(ui.ctx());
        let (icon, color, text, tip) = match &self.route {
            Route::Daemon(sock) => (
                p::PLUGS_CONNECTED,
                t.success,
                "via daemon",
                format!(
                    "The tryx daemon at {} owns the serial port. Commands go through it, \
                     and it keeps the display alive between them.",
                    sock.display()
                ),
            ),
            Route::Direct => (
                p::WARNING,
                t.warn,
                "direct — no daemon",
                "No daemon is running, so each command opens the port itself and nothing \
                 streams sysinfo afterwards: the display blanks about 60 s after the last \
                 command. Start the daemon (systemctl start tryx-panorama) to keep it alive."
                    .to_string(),
            ),
            Route::Remote => (
                p::GLOBE,
                t.info,
                "remote bridge",
                "Commands go to a bridge on another machine — through its daemon, if one \
                 runs there. Images go over that host's shared adb server \
                 (ADB_SERVER_SOCKET defaults to tcp:<host>:5037)."
                    .to_string(),
            ),
            Route::Socket => (
                p::PLUGS_CONNECTED,
                t.info,
                "daemon socket",
                "Commands go to the daemon socket named in the Device field.".to_string(),
            ),
        };
        ui.label(RichText::new(format!("{icon} {text}")).color(color))
            .on_hover_text(tip);
    }

    /// What the panel shows right now: its framebuffer over adb, refreshed on a
    /// timer and shortly after every action. Above the tabs, so it stays in view.
    fn preview_section(&mut self, ui: &mut egui::Ui) {
        let t = Tokens::get(ui.ctx());
        let body = egui::CollapsingHeader::new(format!("{} Now showing", p::MONITOR))
            .id_salt("now_showing")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let capturing = self.preview.is_capturing();
                    if ui
                        .add_enabled(
                            !capturing,
                            egui::Button::new(format!("{} Refresh", p::ARROW_CLOCKWISE)),
                        )
                        .clicked()
                    {
                        self.capture_preview(ui.ctx());
                    }
                    ui.checkbox(
                        &mut self.preview.auto,
                        format!("every {} s", app_state::PREVIEW_EVERY.as_secs()),
                    );
                    if capturing {
                        ui.spinner();
                    }
                    if let Some(at) = self.preview.captured_at {
                        let [w, h] = self.preview.native;
                        ui.label(
                            RichText::new(format!("{w}×{h} · captured {}", at.format("%H:%M:%S")))
                                .color(t.text_dim),
                        );
                    }
                });
                if let Some(e) = &self.preview.error {
                    ui.label(RichText::new(format!("{} {e}", p::WARNING)).color(t.warn))
                        .on_hover_text(
                            "The preview reads the panel over adb (screencap), the same \
                             channel image uploads use.",
                        );
                }
                match &self.preview.texture {
                    Some(texture) => {
                        let size = texture.size_vec2();
                        let scale = (ui.available_width() / size.x).min(PREVIEW_MAX_H / size.y).min(1.0);
                        ui.add(
                            egui::Image::from_texture((texture.id(), size * scale))
                                .corner_radius(t.radius_md),
                        );
                    }
                    None if self.preview.error.is_none() => {
                        ui.label(RichText::new("Capturing the panel…").color(t.text_dim));
                    }
                    None => {}
                }
            });
        self.preview.visible = body.body_returned.is_some();
    }

    // ---- Gallery -----------------------------------------------------------
    fn gallery_tab(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.heading("Add image");
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button(format!("{} Browse…", p::FOLDER_OPEN)).clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "gif", "bmp"])
                        .pick_file()
                {
                    self.selected_image = Some(path);
                }
                match &self.selected_image {
                    Some(p) => ui.label(format!("{}", p.display())),
                    None => ui.label(RichText::new("No image selected").color(colors::weak_text(ui))),
                };
            });
            ui.checkbox(&mut self.replace_on_upload, "Replace (wipe every other file)");
            let can_upload = !self.is_processing && self.selected_image.is_some();
            if ui
                .add_enabled(
                    can_upload,
                    egui::Button::new(format!("{} Upload & add to gallery", p::UPLOAD_SIMPLE)),
                )
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
                ui.label(
                    RichText::new("(playlist empty — upload an image above)")
                        .color(colors::weak_text(ui)),
                );
            }

            let mut move_up: Option<usize> = None;
            let mut move_down: Option<usize> = None;
            let mut remove: Option<String> = None;
            let len = self.gallery.media.len();
            for (i, name) in self.gallery.media.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("{}.", i + 1));
                    if ui.add_enabled(i > 0, egui::Button::new(p::ARROW_UP)).clicked() {
                        move_up = Some(i);
                    }
                    if ui.add_enabled(i + 1 < len, egui::Button::new(p::ARROW_DOWN)).clicked() {
                        move_down = Some(i);
                    }
                    if ui.button(p::TRASH).on_hover_text("Delete from the device").clicked() {
                        remove = Some(name.clone());
                    }
                    let on_device = self.device_media.is_empty()
                        || self.device_media.iter().any(|d| d == name);
                    if on_device {
                        ui.label(name);
                    } else {
                        ui.label(
                            RichText::new(format!("{name}  [missing on device]"))
                                .color(colors::warn(ui)),
                        )
                        .on_hover_text("Shows as a black frame; Apply drops it from the playlist");
                    }
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
                .add_enabled(
                    !self.is_processing,
                    egui::Button::new(format!("{} Apply to device", p::CHECK)),
                )
                .clicked()
            {
                self.apply_gallery();
            }
            if ui
                .add_enabled(
                    !self.is_processing,
                    egui::Button::new(format!("{} Refresh device list", p::ARROW_CLOCKWISE)),
                )
                .clicked()
            {
                self.refresh_device_media();
            }
            if ui
                .add_enabled(
                    !self.is_processing,
                    egui::Button::new(format!("{} Clear gallery", p::BROOM)),
                )
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
                .add_enabled(
                    !self.is_processing,
                    egui::Button::new(format!("{} Apply display", p::CHECK)),
                )
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
                            if ui.button(p::TRASH).clicked() {
                                remove = Some(i);
                            }
                        });
                    }
                    if let Some(i) = remove {
                        self.fan_curve.remove(i);
                    }
                    ui.horizontal(|ui| {
                        if ui.button(format!("{} Add point", p::PLUS)).clicked() {
                            self.fan_curve.push((60, 50));
                        }
                        ui.checkbox(&mut self.fan_raw, "raw (no ceiling sentinel)");
                    });
                    if ui
                        .add_enabled(
                            !self.is_processing,
                            egui::Button::new(format!("{} Apply curve", p::CHECK)),
                        )
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
                        .add_enabled(
                            !self.is_processing,
                            egui::Button::new(format!("{} Apply fixed duty", p::CHECK)),
                        )
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
                .add_enabled(
                    !self.is_processing,
                    egui::Button::new(format!("{} Apply pump", p::CHECK)),
                )
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
                if ui
                    .add_enabled(!self.is_processing, egui::Button::new(format!("{} On", p::POWER)))
                    .clicked()
                {
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
                if ui
                    .add_enabled(
                        !self.is_processing,
                        egui::Button::new(format!("{} Apply names", p::CHECK)),
                    )
                    .clicked()
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

        ui.add_space(8.0);
        self.appearance_group(ui);
    }

    /// Density, text size and theme file — the same knobs as Mastertech's.
    fn appearance_group(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.heading(format!("{} Appearance", p::PAINT_BRUSH));
            ui.separator();
            let mut theme = self.theme.clone();
            let mut changed = false;
            egui::Grid::new("appearance_grid").num_columns(2).spacing([20.0, 8.0]).show(ui, |ui| {
                ui.label("Density:");
                egui::ComboBox::from_id_salt("density")
                    .selected_text(theme.density.label())
                    .show_ui(ui, |ui| {
                        for d in Density::ALL {
                            changed |= ui.selectable_value(&mut theme.density, d, d.label()).changed();
                        }
                    });
                ui.end_row();

                ui.label("Text size:");
                egui::ComboBox::from_id_salt("text_scale")
                    .selected_text(theme.text_scale.label())
                    .show_ui(ui, |ui| {
                        for s in TextScale::ALL {
                            changed |= ui.selectable_value(&mut theme.text_scale, s, s.label()).changed();
                        }
                    });
                ui.end_row();
            });
            ui.horizontal(|ui| {
                if ui
                    .button(format!("{} Load theme…", p::FOLDER_OPEN))
                    .on_hover_text("A Mastertech theme export (JSON), or a bare egui Style")
                    .clicked()
                    && let Some(path) =
                        rfd::FileDialog::new().add_filter("Theme", &["json"]).pick_file()
                {
                    match std::fs::read(&path)
                        .map_err(anyhow::Error::from)
                        .and_then(|bytes| theme::theme_from_json(&bytes))
                    {
                        Ok(loaded) => {
                            theme = loaded;
                            changed = true;
                        }
                        Err(e) => log::error!("Can't load theme {}: {e:#}", path.display()),
                    }
                }
                if ui
                    .button(format!("{} Mastertech default", p::ARROW_COUNTER_CLOCKWISE))
                    .clicked()
                {
                    self.reset_theme(ui.ctx());
                    changed = false;
                }
            });
            if changed {
                self.set_theme(ui.ctx(), theme);
            }
            ui.label(
                RichText::new(format!("Custom theme file: {}", theme::user_theme_path().display()))
                    .small()
                    .color(colors::weak_text(ui)),
            );
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

/// Launch the desktop GUI. `port` (an explicit `--port`) wins over the device
/// remembered from the last session.
pub fn run(port: Option<String>, gallery_path: PathBuf) -> anyhow::Result<()> {
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
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let saved = theme::load();
            theme::install(&cc.egui_ctx, &saved);
            let device = port
                .or_else(|| cc.storage.and_then(|s| s.get_string(app_state::DEVICE_KEY)))
                .unwrap_or_else(|| "/dev/ttyACM0".to_string());
            Ok(Box::new(AioCoolerApp::new(device, gallery_path, saved)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}
