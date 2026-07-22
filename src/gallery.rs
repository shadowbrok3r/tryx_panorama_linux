// ============================================================================
// Persistent image gallery — the host-side source of truth for what the cooler
// displays.
//
// The device keeps uploaded files on /sdcard/pcMedia (flash) but does NOT
// persist the *displayed playlist* across a power-cycle: on reconnect it
// re-applies an in-memory config, and only brightness/turbo hit its
// SharedPreferences. So we store the playlist + display settings here and
// re-apply them on every (re)connect (see commands::daemon / apply_gallery).
// ============================================================================

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::screen_setup::ScreenConfig;

/// Where uploaded media lives on the device.
pub const MEDIA_DIR: &str = "/sdcard/pcMedia";

/// The persisted gallery: playlist order + how to display it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gallery {
    /// Device filenames (in `/sdcard/pcMedia`), in playlist order.
    #[serde(default)]
    pub media: Vec<String>,
    /// `"Single"` | `"Loop"` | `"Shuffle"` (device-recognized playModes).
    #[serde(default = "default_play_mode")]
    pub play_mode: String,
    /// Display settings applied alongside the playlist.
    #[serde(default)]
    pub config: ScreenConfig,
}

fn default_play_mode() -> String {
    "Loop".to_string()
}

impl Default for Gallery {
    fn default() -> Self {
        Self {
            media: Vec::new(),
            play_mode: default_play_mode(),
            config: ScreenConfig::default(),
        }
    }
}

impl Gallery {
    /// Resolve the gallery file path: explicit flag → `TRYX_GALLERY` env →
    /// `$XDG_CONFIG_HOME/tryx-panorama/gallery.json` → `$HOME/.config/...`.
    /// The CLI, GUI, and daemon must agree on this path to share one gallery.
    pub fn resolve_path(explicit: Option<&str>) -> PathBuf {
        if let Some(p) = explicit {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        if let Ok(p) = std::env::var("TRYX_GALLERY") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                format!("{home}/.config")
            });
        PathBuf::from(base).join("tryx-panorama").join("gallery.json")
    }

    /// Load the gallery, returning a default (empty) one if the file is absent.
    pub fn load(path: &Path) -> Result<Gallery> {
        match std::fs::read_to_string(path) {
            Ok(s) if s.trim().is_empty() => Ok(Gallery::default()),
            Ok(s) => serde_json::from_str(&s)
                .with_context(|| format!("Parsing gallery file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Gallery::default()),
            Err(e) => Err(e).with_context(|| format!("Reading gallery file {}", path.display())),
        }
    }

    /// Persist the gallery, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("Creating {}", dir.display()))?;
            }
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json).with_context(|| format!("Writing {}", path.display()))?;
        Ok(())
    }

    /// The playMode to actually send: a single image can't "Loop"/"Shuffle", so
    /// collapse to "Single" then; otherwise the stored mode.
    pub fn effective_play_mode(&self) -> &str {
        if self.media.len() <= 1 {
            "Single"
        } else {
            &self.play_mode
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.media.iter().any(|m| m == name)
    }

    /// Append `name` to the playlist if not already present.
    pub fn add(&mut self, name: String) {
        if !self.contains(&name) {
            self.media.push(name);
        }
    }

    /// Remove `name` from the playlist. Returns whether it was present.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.media.len();
        self.media.retain(|m| m != name);
        self.media.len() != before
    }
}

/// True if `name` matches our upload naming `YYYY-MM-DD_HH-MM-SS-mmm.ext`
/// (see `AioCoolerController::generate_filename`). Files that don't match are
/// treated as foreign (e.g. a factory-shipped file) — excluded from the auto
/// playlist and never deleted by `gallery clear`.
pub fn is_our_upload(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if ext.is_empty() || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    // Fixed shape: '#' = digit, anything else = that literal separator.
    const SHAPE: &[u8] = b"####-##-##_##-##-##-###";
    let bytes = stem.as_bytes();
    if bytes.len() != SHAPE.len() {
        return false;
    }
    bytes.iter().zip(SHAPE).all(|(b, s)| match s {
        b'#' => b.is_ascii_digit(),
        _ => b == s,
    })
}

/// List the media files currently on the device (via `adb shell ls`). Honors
/// `ADB_SERVER_SOCKET`, so it works over the remote bridge. Returns an empty
/// list (with a warning) if the directory is missing or adb is unreachable.
pub fn list_device_media() -> Result<Vec<String>> {
    let out = Command::new("adb")
        .args(["shell", "ls", "-1", MEDIA_DIR])
        .output()
        .context("running `adb shell ls` (is adb installed / a device connected?)")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        log::warn!("adb ls {MEDIA_DIR} failed: {}", stderr.trim());
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut files: Vec<String> = text
        .lines()
        .map(|l| l.trim()) // strip Android shell CRLF
        .filter(|l| !l.is_empty() && !l.ends_with(':'))
        .map(|s| s.to_string())
        .collect();
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_name_matches() {
        assert!(is_our_upload("2025-11-29_01-19-22-612.gif"));
        assert!(is_our_upload("2026-07-22_00-30-03-841.png"));
    }

    #[test]
    fn foreign_name_rejected() {
        assert!(!is_our_upload("qrcode.jpg")); // factory-style name
        assert!(!is_our_upload("2025-11-29.png")); // too short
        assert!(!is_our_upload("2025-11-29_01-19-22-612")); // no extension
        assert!(!is_our_upload("2025-11-29_01-19-22-6X2.png")); // non-digit
    }

    #[test]
    fn effective_mode_collapses_single() {
        let mut g = Gallery {
            media: vec!["a.png".into()],
            play_mode: "Loop".into(),
            config: ScreenConfig::default(),
        };
        assert_eq!(g.effective_play_mode(), "Single");
        g.media.push("b.png".into());
        assert_eq!(g.effective_play_mode(), "Loop");
    }

    #[test]
    fn add_dedups_and_remove_works() {
        let mut g = Gallery::default();
        g.add("a.png".into());
        g.add("a.png".into());
        g.add("b.png".into());
        assert_eq!(g.media, vec!["a.png", "b.png"]);
        assert!(g.remove("a.png"));
        assert!(!g.remove("a.png"));
        assert_eq!(g.media, vec!["b.png"]);
    }

    #[test]
    fn serde_roundtrip() {
        let g = Gallery {
            media: vec!["2025-11-29_01-19-22-612.gif".into(), "x.png".into()],
            play_mode: "Shuffle".into(),
            config: ScreenConfig::default(),
        };
        let s = serde_json::to_string(&g).unwrap();
        let back: Gallery = serde_json::from_str(&s).unwrap();
        assert_eq!(back.media, g.media);
        assert_eq!(back.play_mode, "Shuffle");
    }

    #[test]
    fn load_missing_is_default() {
        let g = Gallery::load(Path::new("/nonexistent/tryx/gallery.json")).unwrap();
        assert!(g.media.is_empty());
        assert_eq!(g.play_mode, "Loop");
    }
}
