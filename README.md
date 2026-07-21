# Tryx Panorama Linux

Linux controller for AIO liquid cooler displays (Tryx/Baiyi). Reverse-engineered from the official Android APK (`com.baiyi.service.serialservice`) and device-side logcat captures of the Windows app's live traffic (frame format verified byte-exact against wire hex dumps).

The cooler contains a Rockchip Android board (`productId: cm01`, Android 11) driving the screen. The PC talks to it over a USB gadget serial channel; media files are pushed over ADB.

## Quick Start (CLI)

The default build is a headless CLI — no GUI dependencies:

```bash
cargo build --release
./target/x86_64-unknown-linux-gnu/release/tryx_panorama_linux detect   # find device, diagnose permissions
```

| Command | Purpose |
|---------|---------|
| `detect` | Find the device, check serial permissions and ADB availability |
| `conn` | Handshake: query device identity/capabilities (retries through the boot gate) |
| `listen [--hex] [--keepalive]` | Decode and print incoming frames |
| `send <cmdType> --json '{...}' [--method POST\|STATE]` | Send a raw command, print replies |
| `sysinfo [--interval-ms 1000] [--count N] [--dry-run]` | Stream system stats to the display (one-shot / bounded) |
| `daemon [--conn] [--interval-ms 1000] [--quiet] [--status-every N]` | Run forever: 1 Hz sysinfo with auto-reconnect + graceful stop (for systemd) |
| `image <file> [config flags]` | Full upload flow: `transport` → ADB push → `transported` → `mediaDelete` → `waterBlockScreenId` |
| `screen <media>... [config flags]` | Reconfigure the screen for media already on the device |
| `pump --enable --value 65` | Turbo pump control |
| `brightness <0-100>` | Display brightness (device maps to 0-250 internally) |
| `screen-power on\|off` | Display panel on/off |
| `display-in-sleep on\|off` | Keep panel on while the PC sleeps |
| `gui` | Desktop app (only with `cargo build --features gui`) |

Config flags for `image`/`screen`: `--screen-mode`, `--play-mode` (Single/Loop), `--ratio`, `--color`, `--align` (Left/Center/Right), `--filter` (Rain/Smoke), `--filter-opacity`, `--badges`, `--sysinfo-display`.

Global flags: `-p/--port <dev>` (default `/dev/ttyACM0`), `-v/-vv` verbosity.

## Device Access Setup

The device enumerates as USB `18d1:2d03` ("Android Open Accessory device (audio + ADB)" — the label is misleading; see Transport below). It exposes:

- **CDC-ACM serial** → `/dev/ttyACM0` (stable name: `/dev/serial/by-id/usb-rockchip_cm01_*-if00`) — the protocol channel
- **ADB** (interface class `ff/42/01`) — file transfer + shell access

One-time setup (Ubuntu 25.10; package is `adb`, not `android-tools-adb`):

```bash
sudo apt install -y adb
sudo usermod -aG dialout,plugdev $USER

sudo tee /etc/udev/rules.d/51-tryx-panorama.rules >/dev/null <<'EOF'
# Tryx Panorama (Rockchip cm01) — ADB interface access
SUBSYSTEM=="usb", ATTR{idVendor}=="18d1", ATTR{idProduct}=="2d03", MODE="0660", GROUP="plugdev", TAG+="uaccess"
# Serial channel — stable symlink + access
SUBSYSTEM=="tty", ATTRS{idVendor}=="18d1", ATTRS{idProduct}=="2d03", MODE="0660", GROUP="dialout", SYMLINK+="tryx0"
EOF
sudo udevadm control --reload-rules && sudo udevadm trigger
# then log out/in (or reboot) for the group changes to take effect
```

## Transport

- **Device side**: the Android `SerialService` opens `/dev/ttyGS0` @ 115200 (`u_serial` USB **gadget** serial function; the baud rate is cosmetic over USB). It polls reads every 50 ms.
- **PC side**: that gadget function surfaces as the CDC-ACM interface pair of the `18d1:2d03` composite → `cdc_acm` binds → `/dev/ttyACM0`. The AOA product-ID label ("audio + ADB") comes from Google's PID table and does not match the actual descriptors (ACM + ADB, no audio).
- The APK contains dormant alternatives that are **never used**: `SerialPort2` (`/dev/ttyMT1`) and a HID gadget path (`/dev/hidg0`). Ignore them.
- **RX watchdog**: if the device receives no bytes for **10 s**, it closes and reopens `ttyGS0`. Send *something* (e.g. `STATE all` sysinfo) at least every ~5-8 s to hold the link. This is why `listen --keepalive` exists and why config flows interleave sysinfo pushes.
- **Init gate**: after boot/service start the device ignores all input for up to ~5 s (pump-detection state machine). Retry `conn` a few times when connecting.

## Protocol

### Frame Structure

From `SerialMsgManager.sendRequestMsg` (device source — both directions use it):

```
interior = [len:2 BE][message][crc:1]
   len = message.len + 5      (counts len:2 + message + crc:1 + both markers)
   crc = (len_hi + len_lo + Σ message) & 0xFF     (over UNESCAPED bytes)

frame = [0x5A] escape(interior) [0x5A]
```

The **entire interior is escaped**, so `0x5A` never appears between the markers — receivers can scan for the closing `0x5A` delimiter-style, then unescape and split.

| Escaping | |
|----------|--------------|
| `0x5A` → | `0x5B 0x01` |
| `0x5B` → | `0x5B 0x02` |

> ⚠️ An earlier revision of this project framed differently (length = escaped-payload only, CRC over escaped bytes, len/crc outside the escaping). That *happened to work* because the device's parser ignores both fields on receive — but any frame whose raw length/CRC byte hit `0x5A` would corrupt. The current implementation matches the device exactly.

Device RX quirks (their parser, `DataConvert`): CRC is stripped and **never verified**; the length field is logged and **never enforced**; frames split across two reads are dropped, and >2 frames in one read are dropped. Our decoder reassembles across reads and verifies both fields (lenient: warns unless both fail).

> **CRC collision quirk** (observed twice in the captures): when the checksum byte itself
> lands on `0x5A`/`0x5B`, the Windows app sends it *unescaped* and the device leaks a junk
> byte into the JSON body (that sample gets dropped device-side). Escaping it instead also
> breaks, because the device strips the CRC *before* unescaping. We sidestep both failure
> modes: when the CRC would be `0x5A`/`0x5B`, we pad the JSON body with spaces until it
> isn't (JSON-invisible, always converges).

### Message Format

HTTP-like text inside the frame. **Requests** (PC → device) — the Windows app sends exactly these four headers, in this order:

```
POST <cmdType> 1\r\n
SeqNumber=<seq>\r\n
Date=<epoch_ms>\r\n
ContentType=json\r\n
ContentLength=<len>\r\n
\r\n
<json_body>
```

Methods (`requestState`): `POST`, `STATE`, `GET`, `DELETE`. Sysinfo pushes use `STATE`; everything else observed uses `POST`.

**Replies** (device → PC) use a different first line — `<version> <code>`, i.e. two tokens:

```
1 200\r\n
AckNumber=<your seq + 1>\r\n
ContentLength=<len>\r\n
ContentType=json\r\n
\r\n
<json_body, if any>
```

Sequencing (observed):

- One monotonically increasing PC-side `SeqNumber` shared by all message types.
- The device replies exactly once per received frame, within ~120-170 ms, with `AckNumber = SeqNumber + 1`; the PC's next message uses that number as its seq.
- Replies carry no `SeqNumber`/`Date`; requests carry no `AckNumber`. `msgId`, `Counter`, `ContentRange`, `FileName`, `FileSize` never appear in normal traffic (all default `-1` = omitted).
- **Hard requirements** (device NPEs otherwise): 3-token request line, and **at least 2 header lines**.
- Quirk: the Windows app's body `timestamp` in `all` is `Date − 7h` (local-time-as-epoch bug). We send real epoch ms; the device doesn't care.

### Commands

Handled by the device (`SerialMsgReceiverHandler`):

| Command | Method | Body | Device behavior / reply |
|---------|--------|------|--------------------------|
| `conn` | POST | `{}` | Replies 200 with identity: `{"attribute":["Status","Water Block Screen","Fan LCD\|rw","Turbo Pump"?],"OS":"Android","productId":"cm01","version":{app,firmware,hardware},"sn":...}`. "Turbo Pump" only listed if pump rpm ≥ 3200. |
| `transport` | POST | `{"fileName","fileSize","type"}` | Opens `/sdcard/pcMedia/<fileName>` for writing; replies `{"state":"success","blockMaxSize":888888888}`. `type:"firmware"` arms an RK OTA update on completion. |
| `transported` | POST | `{"fileName","md5"}` | Closes the file. **md5 is never actually verified.** Replies `{"state":"success"}`; if `type` was `firmware`, broadcasts to `android.rockchip.update.service`. |
| `mediaDelete` | POST | `{"type":"custom","exclude":[...]}` (or `include`) | Deletes everything-but-listed (or listed) in `/sdcard/pcMedia`. The app always sends `type:"custom"` and pairs this with a `waterBlockScreenId` ≤100 ms later. Bare 200 reply. |
| `turboPump` | POST | `{"enable":bool,"value":int}` | Writes sysfs `aio_cooler/control_source` (0/1) + `aio_cooler/pwm`. Bare 200. |
| `config` | POST | `{"turboPump":{"enable","value"}}` | Same as turboPump. Bare 200. (Never seen on the wire; fan curve edits presumably use it too.) |
| `all` | STATE | full sysinfo JSON (below) | Forwarded to HomeUI, which answers with the **status 200 body** (fan/pump RPM etc.) and re-evaluates its fan curve. |
| `waterBlockScreenId` | POST | screen config (see below) | Handled by HomeUI (forwarded over AIDL); configures display mode/media/overlays. Empty 200. |
| `brightness` | POST | `{"value":0-100}` | Panel brightness percent (device maps ×2.5 → 0-250). Empty 200. |
| `waterBlockScreen` | POST | `{"enable":bool}` | Panel on/off (off = brightness forced 0). Empty 200. |
| `displayInSleep` | POST | `{"enable":bool}` | Keep panel on during PC sleep. Empty 200. |
| anything else | POST/GET/DELETE | — | Bare 200 ACK. |

Additional cmdTypes defined for the AIDL/UI layer (per-metric pushes, schemas live in HomeUI): `allState`, `cpu`, `gpu`, `mem`, `net`, `disk`, `fan`, `other`. None observed on the wire; the bundled `all` is what the Windows app uses.

### Sysinfo stream and the status reply

The Windows app sends `STATE all` at **1 Hz** continuously. This is load-bearing three ways: it holds the 10 s RX watchdog open, it feeds `cpu.temperature` to the **device-side smart fan curve** (8-point [temp %, duty %] curve, linearly interpolated, written to the LCD-fan PWM every second), and each push is answered with the status body:

```json
{"status":{"fanLCD":"2010","turboPump":"1410"},
 "warning":"[{\"description\":\"No ERROR\",\"type\":\"Fan LCD\"}]",
 "availableStorage":3433697280}
```

- `fanLCD` / `turboPump`: RPM as strings (from sysfs tachometers, 30-RPM granularity)
- `warning`: a **double-encoded** JSON string (parse leniently)
- `availableStorage`: device flash free bytes (~3.4 GB total)

The `all` payload schema matches `src/sysinfo.rs` (`network`, `memory` MB, `cpu` °C/MHz/W/V/%, `gpu`, `disk` GB, `fans[]` RPM, `motherboard`, `timestamp`). Observed enums and ranges are documented in the code.

### Real sensor reading

`SysInfoReader` (in `src/sysinfo.rs`) is stateful — construct once, call `sample()` per tick — because several fields are **rates** (deltas between samples): `cpu.load` (from `/proc/stat`), `network.upload/download` (`/sys/class/net/*/statistics`, KB/s), `disk.readSpeed/writeSpeed`+`activity` (`/proc/diskstats`), and `cpu.power` (RAPL `energy_uj` delta). Sensor paths are discovered once and cached, generically across:

| Field | Source |
|-------|--------|
| CPU temp | `k10temp` `Tctl` (AMD) / `coretemp` Package (Intel) / thermal_zone |
| CPU freq | avg `cpufreq/scaling_cur_freq` |
| CPU power | RAPL `intel-rapl:*/energy_uj` delta — **root-only**; reports 0 otherwise |
| GPU (temp/fan/load/clock/power/voltage) | `amdgpu` hwmon on the largest-VRAM `card*`, or `nvidia-smi` |
| RAM temp | DDR5 `spd5118`/`jc42` module sensors (max) |
| Disk temp | `nvme` composite |
| Disk IO / capacity | `/proc/diskstats` (root LV) + `df` |
| Case fans | super-I/O (`nct*`/`asus`) — empty if that driver isn't loaded; GPU fan is in `gpu.fan`, not `fans[]` |

Fields we can't read without extra privilege/tools are sent as `0` (honest) rather than faked: `cpu.voltage`, `memory.speed`, and `cpu.power` when not root. Run `sysinfo --dry-run --count 1` to see exactly what this machine reports, or `-v` on the daemon to log the detected sensor set.

> **CPU power needs root.** Linux exposes RAPL `energy_uj` at mode `0400 root`. The systemd unit runs as root so power populates; running as an unprivileged `dialout` user works for everything else but reports `cpu.power: 0`.

### Daemon & systemd

`daemon` streams sysinfo at 1 Hz forever, with two layers of resilience: it reconnects internally if the serial link drops (verified against a live USB reset — "Send failed (Broken pipe); will reconnect" → reconnected in <1 s), and it exits cleanly on SIGINT/SIGTERM so `systemctl stop` is graceful. `--conn` re-runs the handshake on each connect (handles the device's ~5 s post-reset boot gate via retries); `--status-every N` prints a combined line showing what we sent and what the device reports (fan/pump RPM, free storage).

Install as a service (`packaging/tryx-panorama.service`):

```bash
sudo install -m755 target/x86_64-unknown-linux-gnu/release/tryx_panorama_linux /usr/local/bin/
sudo install -m644 packaging/tryx-panorama.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now tryx-panorama.service
systemctl status tryx-panorama.service
journalctl -u tryx-panorama.service -f
```

The unit targets `/dev/tryx0` (the stable symlink from the udev rule above) and runs as root so CPU power works. It documents a least-privilege alternative (a `dialout` user, with `cpu.power` then reporting 0) inline.

`waterBlockScreenId` body:

```json
{
  "id": "Customization",
  "screenMode": "Full Screen",
  "playMode": "Single",
  "ratio": "2:1",
  "media": ["filename.png"],
  "settings": {
    "color": "#dcdcdc",
    "align": "Left",
    "filter": { "value": null, "opacity": 100 },
    "badges": ["GPU Badge", "CPU Badge"]
  },
  "sysinfoDisplay": ["CPU Temperature", "GPU Temperature"]
}
```

Observed values: `screenMode`: `Full Screen` | `Screen Splitting`; `playMode`: `Single` | `Loop` (Loop with a multi-file `media` list); `filter.value`: `null` | `"Rain"` | `"Smoke"` (device plays `/system/media/anim/<name>.webp`); badges `CPU Badge`/`GPU Badge`; sysinfo entries `CPU Temperature`/`GPU Temperature`.

**Screen Splitting variant**: `ratio` is omitted, `settings` becomes an *array* (one object per region), and `sysinfoDisplay` an *array of arrays*:

```json
{"id":"Customization","screenMode":"Screen Splitting","playMode":"Single",
 "media":["file.png"],
 "settings":[{"color":"#000000","align":"Left","filter":{"value":null,"opacity":100},"badges":[]},
              {"color":"#000000","align":"Center","filter":{"value":null,"opacity":100},"badges":["CPU Badge","GPU Badge"]}],
 "sysinfoDisplay":[[],["CPU Temperature","GPU Temperature"]]}
```

### Device internals (for reference)

- Media dir: `/sdcard/pcMedia/`; service log: `/sdcard/catchlog/log/serialserver_log.txt`
- Pump sysfs: `/sys/bus/i2c/drivers/aio_cooler/{control_source,pwm,rpm}` (rpm ≥ 3200 ⇒ pump present)
- Fan LCD sysfs: `/sys/bus/platform/drivers/lcd_fan/speed`
- UI app: `com.baiyi.homeui.tkcfanhomeui` (TKSMainActivity); talks to SerialService via AIDL (`ISerialControl.sendData` / `IOnSerialDataCallback`)
- The unsolicited device→PC status push (`{"status":{"fanLCD","turboPump"},"warning":[...],"availableStorage":...}`) is built by HomeUI and sent as a body on a `1 200` reply-shaped message

## Image Transfer Flow

Wire captures settle an old question: **the Windows app never streams file bytes over serial either.** In the observed transfer, `transport` and `transported` arrived 475 ms apart with zero payload bytes between them (device logged `size--0/28558`), the app's serial md5 was literally the string `"todo"`, and the actual PNG landed via ADB (`com.android.shell` filesystem activity at the same moment). The serial pair is an announce/commit envelope; the device refreshes its media list on commit.

Both this tool and the Windows app therefore do:

1. `STATE all` → establish traffic (watchdog!)
2. `transport` `{"type":"media","fileSize":N,"fileName":"<ts>.png"}` → device opens/truncates the file, replies `{"state":"success","blockMaxSize":888888888}`
3. `adb push` → `/sdcard/pcMedia/<name>` (the actual bytes)
4. `transported` `{"md5":...,"fileName":...}` → replies `{"state":"success"}` (md5 unchecked)
5. `mediaDelete` `{"type":"custom","exclude":[...]}` → prune playlist
6. `waterBlockScreenId` → configure display
7. 1 Hz `STATE all` → temps + keepalive

File naming: `YYYY-MM-DD_HH-MM-SS-mmm.<ext>` from the PC's local clock.

> The device's serial-streaming file path exists in code but has an apparent bug
> (`isReceiverFile` is never set true in the decompiled source) and chunking headers
> (`Counter`/`ContentRange`) are never used — consistent with ADB being the real channel.

## Project Structure

```
src/
├── main.rs          # clap CLI entry; GUI behind `gui` feature
├── commands.rs      # CLI subcommands: detect/conn/listen/send/sysinfo/daemon/image/screen/pump/…
├── data.rs          # Protocol: framing, escaping, FrameDecoder (RX), message parse/build
├── screen_setup.rs  # AIO controller: ADB push, screen config, keepalive
├── sysinfo.rs       # SysInfoReader: real sensor reading (deltas) for the STATE all payload
├── gui.rs           # egui desktop app        (feature = "gui")
└── app_state.rs     # GUI state/threading     (feature = "gui")

packaging/
└── tryx-panorama.service   # systemd unit for the daemon
```

## Building

```bash
cargo build --release                 # CLI only (headless-friendly)
cargo build --release --features gui  # CLI + desktop GUI
cargo test                            # protocol unit tests
```

Note: `.cargo/config.toml` configures clang+mold for CI. On machines without them:

```bash
RUSTFLAGS="" CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=cc cargo build --release
```

## APK Source Reference

Decompiled sources live in `aio-reverse/` (not committed). Key files under
`com/baiyi/service/serialservice/serialdataservice/`:

```
├── data/
│   ├── entity/SerialData.java         # Message model + cmdType/state constants
│   ├── entity/DataHeader.java         # Header fields
│   ├── manager/SerialMsgManager.java  # TX: frame building, 100ms send queue, 10s RX watchdog
│   ├── manager/SerialMsgReceiverHandler.java  # RX: all command handlers, sysfs writes
│   └── tool/{ByteTools,DataConvert}.java      # CRC/hex helpers; frame parsing
├── serial/SerialManage.java           # Opens /dev/ttyGS0 @115200 (the real transport)
└── serial/SerialPort2.java            # /dev/ttyMT1 — dormant, ignore
```

Other on-device APKs (in `aio-reverse/app/`): `HomeUI.apk` (the 225 MB UI app — owns
screen rendering, status pushes, per-metric schemas), `RKUpdateService.apk` (firmware
updates). HomeUI is the next decompilation target for full `waterBlockScreenId` /
sysinfo-schema semantics.
