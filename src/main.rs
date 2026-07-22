use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[cfg(feature = "gui")]
mod app_state;
mod commands;
mod data;
mod gallery;
#[cfg(feature = "gui")]
mod gui;
mod screen_setup;
mod sysinfo;

use screen_setup::ScreenConfig;

#[derive(Parser)]
#[command(
    name = "tryx_panorama_linux",
    version,
    about = "Tryx Panorama AIO display controller for Linux",
    long_about = "Controls the Tryx Panorama AIO cooler display over USB serial (CDC-ACM) and ADB.\n\
                  Run `detect` first to find the device and diagnose permissions."
)]
struct Cli {
    /// Serial device (run `detect` to find it)
    #[arg(short, long, global = true, default_value = "/dev/ttyACM0")]
    port: String,

    /// Increase log verbosity (-v debug, -vv trace)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Gallery state file (default: $XDG_CONFIG_HOME/tryx-panorama/gallery.json,
    /// or $TRYX_GALLERY). CLI, GUI, and daemon must share the same path.
    #[arg(long, global = true)]
    gallery_file: Option<String>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Find the device, check serial permissions and ADB availability
    Detect,
    /// Handshake: query device identity and capabilities (POST conn)
    Conn {
        /// Retry attempts (device ignores input for ~5s after boot)
        #[arg(long, default_value_t = 5)]
        retries: u32,
    },
    /// Decode and print incoming frames from the device
    Listen {
        /// Also dump raw RX bytes as hex
        #[arg(long)]
        hex: bool,
        /// Stop after this many seconds (default: run until Ctrl-C)
        #[arg(long)]
        timeout_secs: Option<u64>,
        /// Send sysinfo every 5s so the device doesn't reset its port
        #[arg(long)]
        keepalive: bool,
    },
    /// Send a raw protocol command, then print replies
    Send {
        /// Command type (conn, all, config, waterBlockScreenId, turboPump, …)
        cmd_type: String,
        /// JSON body
        #[arg(long, default_value = "{}")]
        json: String,
        /// Request method token (POST or STATE)
        #[arg(long, default_value = "POST")]
        method: String,
        /// How long to listen for replies afterwards
        #[arg(long, default_value_t = 3)]
        wait_secs: u64,
    },
    /// Stream system info (STATE all) to the display
    Sysinfo {
        /// Milliseconds between updates
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        /// Number of updates to send (0 = until Ctrl-C)
        #[arg(long, default_value_t = 0)]
        count: u64,
        /// Print the JSON payload instead of sending it
        #[arg(long)]
        dry_run: bool,
    },
    /// Run continuously: stream sysinfo at 1Hz with auto-reconnect (for systemd)
    Daemon {
        /// Milliseconds between updates
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        /// Send a conn handshake and log device identity on each (re)connect
        #[arg(long)]
        conn: bool,
        /// Suppress the periodic status line (logs still emit)
        #[arg(long)]
        quiet: bool,
        /// Print a status line every N ticks (0 = never)
        #[arg(long, default_value_t = 10)]
        status_every: u64,
        /// Don't re-apply the saved gallery on (re)connect
        #[arg(long)]
        no_gallery: bool,
    },
    /// Expose this machine's cooler over TCP so a remote GUI/CLI can control it.
    /// Run on the box wired to the cooler; the remote side uses --port tcp://host:port.
    /// One client at a time. No auth — bind to a trusted LAN interface / firewall it.
    Bridge {
        /// Address to listen on (host:port). Use 0.0.0.0 for any interface.
        #[arg(long, default_value = "0.0.0.0:9600")]
        listen: String,
    },
    /// Upload an image and add it to the persistent gallery (accumulates by
    /// default — nothing is deleted; the whole playlist is re-displayed)
    Image {
        /// Image file (png/jpg/gif/…)
        path: PathBuf,
        /// Replace: wipe every other file and show only this one (old behavior)
        #[arg(long)]
        replace: bool,
        /// Deprecated no-op: keeping media is now the default (accepted for compat)
        #[arg(long, hide = true)]
        keep_media: bool,
        #[command(flatten)]
        config: ScreenConfigArgs,
    },
    /// Manage the persistent image gallery (accumulating playlist)
    Gallery {
        #[command(subcommand)]
        action: GalleryAction,
    },
    /// Configure the display for media already on the device (no upload)
    Screen {
        /// Media file name(s) on the device (in /sdcard/pcMedia)
        #[arg(required = true)]
        media: Vec<String>,
        #[command(flatten)]
        config: ScreenConfigArgs,
    },
    /// Control the turbo pump (POST turboPump)
    Pump {
        /// Enable turbo mode
        #[arg(long)]
        enable: bool,
        /// PWM value (device init default: 65)
        #[arg(long, default_value_t = 65)]
        value: u32,
        /// How long to listen for the ACK afterwards
        #[arg(long, default_value_t = 2)]
        wait_secs: u64,
    },
    /// Set display brightness (0-100%)
    Brightness {
        value: u8,
    },
    /// Turn the display panel on or off
    ScreenPower {
        #[arg(value_enum)]
        state: Switch,
    },
    /// Keep the panel on (or not) while the PC sleeps
    DisplayInSleep {
        #[arg(value_enum)]
        state: Switch,
    },
    /// Screen-off/on event (never powers Android off, just the panel)
    Power {
        #[arg(value_enum)]
        event: PowerEvent,
    },
    /// Set the display temperature unit
    Temperature {
        #[arg(value_enum)]
        unit: TempUnit,
    },
    /// Set display rotation (persist.vendor.orientation; may need a device reboot)
    Rotate {
        degree: i32,
    },
    /// Set the CPU/GPU badge names (auto-detected from this machine if omitted)
    Spec {
        #[arg(long)]
        cpu: Option<String>,
        #[arg(long)]
        gpu: Option<String>,
    },
    /// Choose which system-info metrics the overlay shows
    SysinfoDisplay {
        /// Metric names, e.g. "CPU Temperature" "GPU Usage" "Date&Time"
        #[arg(required = true, num_args = 1..)]
        items: Vec<String>,
    },
    /// Graceful screen-off (serial link stays up; any command restores it)
    Disconn,
    /// Control the LCD/pump fan curve (Smart temperature curve or Fixed duty)
    Fan {
        #[command(subcommand)]
        action: FanAction,
    },
    /// Launch the desktop GUI (requires building with --features gui)
    Gui,
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum Switch {
    On,
    Off,
}

impl From<Switch> for bool {
    fn from(s: Switch) -> bool {
        matches!(s, Switch::On)
    }
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum TempUnit {
    Celsius,
    Fahrenheit,
}

impl TempUnit {
    fn as_str(self) -> &'static str {
        match self {
            TempUnit::Celsius => "Celsius",
            TempUnit::Fahrenheit => "Fahrenheit",
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum PowerEvent {
    Suspend,
    Shutdown,
    LockScreen,
    Resume,
    UnlockScreen,
}

impl PowerEvent {
    fn as_str(self) -> &'static str {
        match self {
            PowerEvent::Suspend => "suspend",
            PowerEvent::Shutdown => "shutdown",
            PowerEvent::LockScreen => "lock-screen",
            PowerEvent::Resume => "resume",
            PowerEvent::UnlockScreen => "unlock-screen",
        }
    }
}

#[derive(clap::Subcommand)]
enum FanAction {
    /// Smart temperature→duty curve, e.g. --curve "30:30,50:40,65:55,80:70,90:100"
    Smart {
        /// Comma-separated tempC:duty% points (duty 0-100)
        #[arg(long)]
        curve: String,
        /// Don't append a ceiling sentinel (send the curve exactly as given)
        #[arg(long)]
        raw: bool,
    },
    /// Fixed duty percent (0-100), ignores temperature
    Fixed {
        duty: u8,
    },
}

#[derive(clap::Subcommand)]
enum GalleryAction {
    /// List device media, annotated with playlist position / foreign status
    List,
    /// Upload an image and append it to the playlist (keeps current settings)
    Add {
        /// Image file (png/jpg/gif/…)
        path: PathBuf,
    },
    /// Remove one image: delete its file and drop it from the playlist
    Rm {
        /// Device file name (as shown by `gallery list`)
        name: String,
    },
    /// Delete all of our uploads (keeps foreign files) and empty the playlist
    Clear,
    /// Re-send the saved gallery to the device now
    Apply,
    /// Set the play mode and re-apply
    Mode {
        /// Single | Loop | Shuffle
        #[arg(value_parser = ["Single", "Loop", "Shuffle"])]
        mode: String,
    },
}

#[derive(Args)]
struct ScreenConfigArgs {
    /// "Full Screen" (or "Screen Splitting" — not supported yet)
    #[arg(long, default_value = "Full Screen")]
    screen_mode: String,
    /// Single or Loop
    #[arg(long, default_value = "Single")]
    play_mode: String,
    #[arg(long, default_value = "2:1")]
    ratio: String,
    #[arg(long, default_value = "#dcdcdc")]
    color: String,
    /// Left, Center or Right
    #[arg(long, default_value = "Left")]
    align: String,
    /// Animation overlay: Rain, Smoke (device built-ins)
    #[arg(long)]
    filter: Option<String>,
    #[arg(long, default_value_t = 100)]
    filter_opacity: u8,
    /// Comma-separated: "CPU Badge,GPU Badge,RAM Badge,FPS Badge"
    #[arg(long, value_delimiter = ',', default_value = "GPU Badge,CPU Badge")]
    badges: Vec<String>,
    /// Comma-separated: "CPU Temperature,GPU Temperature,CPU Usage,…"
    #[arg(long, value_delimiter = ',', default_value = "CPU Temperature,GPU Temperature")]
    sysinfo_display: Vec<String>,
}

impl From<&ScreenConfigArgs> for ScreenConfig {
    fn from(a: &ScreenConfigArgs) -> Self {
        Self {
            id: "Customization".to_string(),
            screen_mode: a.screen_mode.clone(),
            play_mode: a.play_mode.clone(),
            ratio: a.ratio.clone(),
            color: a.color.clone(),
            align: a.align.clone(),
            filter_value: a.filter.clone(),
            filter_opacity: a.filter_opacity,
            badges: a.badges.clone(),
            sysinfo_display: a.sysinfo_display.clone(),
        }
    }
}

fn init_cli_logging(verbose: u8) {
    let level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level))
        .format_timestamp_millis()
        .init();
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let command = match cli.command {
        Some(cmd) => cmd,
        // No subcommand: launch the GUI when it's compiled in, otherwise show help
        None => {
            #[cfg(feature = "gui")]
            {
                Cmd::Gui
            }
            #[cfg(not(feature = "gui"))]
            {
                use clap::CommandFactory;
                Cli::command().print_help()?;
                return Ok(());
            }
        }
    };

    match command {
        Cmd::Gui => {
            #[cfg(feature = "gui")]
            {
                // The GUI installs its own logger (egui_logger)
                gui::run()
            }
            #[cfg(not(feature = "gui"))]
            {
                anyhow::bail!(
                    "This binary was built without the GUI. Rebuild with: cargo build --features gui"
                )
            }
        }
        cmd => {
            init_cli_logging(cli.verbose);
            let gallery_path = gallery::Gallery::resolve_path(cli.gallery_file.as_deref());
            match cmd {
                Cmd::Detect => commands::detect(),
                Cmd::Conn { retries } => commands::conn(&cli.port, retries),
                Cmd::Listen {
                    hex,
                    timeout_secs,
                    keepalive,
                } => commands::listen(&cli.port, hex, timeout_secs, keepalive),
                Cmd::Send {
                    cmd_type,
                    json,
                    method,
                    wait_secs,
                } => commands::send(&cli.port, &method, &cmd_type, &json, wait_secs),
                Cmd::Sysinfo {
                    interval_ms,
                    count,
                    dry_run,
                } => commands::sysinfo_stream(&cli.port, interval_ms, count, dry_run),
                Cmd::Daemon {
                    interval_ms,
                    conn,
                    quiet,
                    status_every,
                    no_gallery,
                } => commands::daemon(
                    &cli.port,
                    interval_ms,
                    conn,
                    quiet,
                    status_every,
                    &gallery_path,
                    no_gallery,
                ),
                Cmd::Bridge { listen } => commands::bridge(&cli.port, &listen),
                Cmd::Image {
                    path,
                    replace,
                    keep_media: _,
                    config,
                } => commands::image(
                    &cli.port,
                    &path,
                    &ScreenConfig::from(&config),
                    &gallery_path,
                    replace,
                ),
                Cmd::Gallery { action } => match action {
                    GalleryAction::List => commands::gallery_list(&gallery_path),
                    GalleryAction::Add { path } => {
                        commands::gallery_add(&cli.port, &path, &gallery_path)
                    }
                    GalleryAction::Rm { name } => {
                        commands::gallery_rm(&cli.port, &name, &gallery_path)
                    }
                    GalleryAction::Clear => commands::gallery_clear(&cli.port, &gallery_path),
                    GalleryAction::Apply => commands::apply_gallery(&cli.port, &gallery_path),
                    GalleryAction::Mode { mode } => {
                        commands::gallery_mode(&cli.port, &mode, &gallery_path)
                    }
                },
                Cmd::Screen { media, config } => {
                    commands::screen(&cli.port, &media, &ScreenConfig::from(&config))
                }
                Cmd::Pump {
                    enable,
                    value,
                    wait_secs,
                } => commands::pump(&cli.port, enable, value, wait_secs),
                Cmd::Brightness { value } => commands::brightness(&cli.port, value, 2),
                Cmd::ScreenPower { state } => commands::screen_power(&cli.port, state.into(), 2),
                Cmd::DisplayInSleep { state } => {
                    commands::display_in_sleep(&cli.port, state.into(), 2)
                }
                Cmd::Power { event } => commands::power(&cli.port, event.as_str(), 2),
                Cmd::Temperature { unit } => commands::temperature(&cli.port, unit.as_str(), 2),
                Cmd::Rotate { degree } => commands::rotate(&cli.port, degree, 2),
                Cmd::Spec { cpu, gpu } => commands::spec(&cli.port, cpu, gpu, 2),
                Cmd::SysinfoDisplay { items } => commands::sysinfo_display(&cli.port, &items, 2),
                Cmd::Disconn => commands::disconn(&cli.port, 2),
                Cmd::Fan { action } => match action {
                    FanAction::Smart { curve, raw } => {
                        let points = commands::parse_curve(&curve)?;
                        commands::fan_smart(&cli.port, points, raw, 2)
                    }
                    FanAction::Fixed { duty } => commands::fan_fixed(&cli.port, duty, 2),
                },
                Cmd::Gui => unreachable!(),
            }
        }
    }
}
