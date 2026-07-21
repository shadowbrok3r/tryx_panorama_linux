use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[cfg(feature = "gui")]
mod app_state;
mod commands;
mod data;
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
    },
    /// Push an image via ADB and configure the display to show it
    Image {
        /// Image file (png/jpg/gif/…)
        path: PathBuf,
        /// Skip the mediaDelete step (keep existing files on the device)
        #[arg(long)]
        keep_media: bool,
        #[command(flatten)]
        config: ScreenConfigArgs,
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
                } => commands::daemon(&cli.port, interval_ms, conn, quiet, status_every),
                Cmd::Image {
                    path,
                    keep_media,
                    config,
                } => commands::image(&cli.port, &path, &ScreenConfig::from(&config), keep_media),
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
                Cmd::Gui => unreachable!(),
            }
        }
    }
}
