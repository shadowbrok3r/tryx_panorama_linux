

#[derive(Debug)]
pub enum AppMessage {
    Log(String),
    Progress(f32, String),
    Success(String),
    Error(String),
}

/// Main App Structure
pub struct AioCoolerApp {

    pub serial_device: String,
    pub selected_image: Option<std::path::PathBuf>,
    pub screen_config: crate::screen_setup::ScreenConfig,


    pub is_processing: bool,
    pub progress: f32,
    pub status_message: String,
    pub log_messages: Vec<String>,


    pub message_sender: Option<crossbeam::channel::Sender<AppMessage>>,
    pub message_receiver: crossbeam::channel::Receiver<AppMessage>,
}

impl Default for AioCoolerApp {
    fn default() -> Self {
        let (tx, rx) = crossbeam::channel::unbounded();
        Self {
            serial_device: "/dev/ttyACM0".to_string(),
            selected_image: None,
            screen_config: crate::screen_setup::ScreenConfig::default(),
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
    pub fn process_messages(&mut self) {
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

    pub fn start_transfer(&mut self) {
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

        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<(), anyhow::Error> {
                let _ = tx.send(AppMessage::Progress(0.1, "Calculating MD5...".to_string()));
                let _ = tx.send(AppMessage::Log("Calculating file MD5...".to_string()));

                let file_md5 = crate::AioCoolerController::calculate_md5(&image_path)?;
                let file_size = std::fs::metadata(&image_path)?.len();

                let extension = image_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("png");
                let remote_name = crate::AioCoolerController::generate_filename(extension);

                let _ = tx.send(AppMessage::Log(format!(
                    "File: {} ({} bytes, MD5: {})",
                    image_path.display(),
                    file_size,
                    file_md5
                )));

                let _ = tx.send(AppMessage::Progress(0.2, "Pushing to device via ADB...".to_string()));
                let _ = tx.send(AppMessage::Log("Starting ADB push...".to_string()));

                let controller = crate::AioCoolerController::new(&serial_device);
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
