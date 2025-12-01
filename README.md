# Tryx Panorama Linux

Linux controller for AIO liquid cooler displays (Tryx/Baiyi). Reverse-engineered from the official Android APK.

## Protocol Overview

Communication occurs over USB serial (`/dev/ttyACM0` (could be named differently) @ 115200 baud). The cooler runs Android and exposes a serial interface for commands.
- If you run into `Permission Denied` running this tool, you likely need to setup a udev rule, which varies from distro to distro. 
- To do this, we need the Vendor ID and Product ID.
```bash
# First, make sure you are a member of either the dialout or uucp group
# for manjaro/arch, it is the dialout group
sudo usermod -aG dialout $USER
# or uucp for other distros, but check docs first
sudo usermod -aG uucp $USER

lsusb
# Example output: Bus 001 Device 010: ID 1e71:2007 XYZ Corp AIO Display

sudo nano /etc/udev/rules.d/99-usb-serial.rules
```

in that new file, you will need to copy the ID's referenced in the last step here:
```ru
# Replace 1e71 and 2007 with your idVendor / idProduct
SUBSYSTEM=="usb", ATTR{idVendor}=="1e71", ATTR{idProduct}=="2007", MODE="0666", GROUP="plugdev"
```

then run:
```bash
sudo udevadm control --reload-rules
sudo udevadm trigger # or logout / log back in
```

### Frame Structure

```
[0x5A][length:2B BE][escaped_payload][CRC:1B][0x5A]
```

| Field | Size | Description |
|-------|------|-------------|
| Start marker | 1 byte | Always `0x5A` |
| Length | 2 bytes | Big-endian, length of escaped payload |
| Payload | variable | Escaped message content |
| CRC | 1 byte | Sum of escaped payload bytes (& 0xFF) |
| End marker | 1 byte | Always `0x5A` |

### Byte Escaping

Since `0x5A` is the frame marker, payload bytes must be escaped:

| Original | Escaped |
|----------|---------|
| `0x5A` | `0x5B 0x01` |
| `0x5B` | `0x5B 0x02` |

### Message Format

HTTP-like text protocol inside the frame:

```
POST <cmdType> <version>\r\n
SeqNumber=<seq>\r\n
AckNumber=<ack>\r\n
ContentLength=<len>\r\n
ContentType=json\r\n
FileName=-1\r\n
FileSize=-1\r\n
ContentRange=-1\r\n
Counter=-1\r\n
Date=<timestamp_ms>\r\n
msgId=-1\r\n
\r\n
<json_body>
```

## APK Source Mapping

Decompiled from: `com.baiyi.service.serialservice`

### Data Structures

| APK Class | Rust Equivalent | Purpose |
|-----------|-----------------|---------|
| `data.entity.SerialData` | `CommandMessage` | Message container with headers + body |
| `data.entity.DataHeader` | Header fields in `CommandMessage` | Protocol headers (SeqNumber, AckNumber, etc.) |

### Protocol Functions

| APK Method | Rust Function | Purpose |
|------------|---------------|---------|
| `ByteTools.getCRC()` | `calc_crc()` | Sum-based CRC calculation |
| `ByteTools.int2Bytes()` | `u16::to_be_bytes()` | Big-endian length encoding |
| `SerialMsgManager.sendRequestMsg()` (escape loop) | `escape_data()` | Escape `0x5A`/`0x5B` bytes |
| `SerialMsgManager.sendRequestMsg()` (frame assembly) | `build_frame()` | Assemble complete frame |
| `DataConvert.analy()` (unescape) | — | Parse incoming frames (not implemented yet) |
| `DataConvert.getSerDataByBytes()` | — | Receive handler (not implemented yet) |

### Commands

| Command | Direction | Purpose | Implemented |
|---------|-----------|---------|-------------|
| `conn` | Device→PC | Connection handshake, device capabilities | ❌ |
| `transport` | PC→Device | Announce file transfer (name, size, type) | ✅ |
| `transported` | PC→Device | Confirm transfer complete (md5, fileName) | ✅ |
| `waterBlockScreenId` | PC→Device | Configure display (mode, media, overlays) | ✅ |
| `mediaDelete` | PC→Device | Delete media files | ❌ |
| `turboPump` | PC→Device | Control turbo pump | ❌ |
| `config` | PC→Device | Device configuration | ❌ |
| `all` | Both | System state broadcast | ❌ |

## Project Structure

```
src/
├── main.rs          # egui application, UI
├── data.rs          # Protocol: framing, escaping, message builder
├── screen_setup.rs  # AIO controller: ADB, serial commands
└── app_state.rs     # Application state, async messaging
```

### Module Details

**`data.rs`** — Serial protocol implementation

- `CommandMessage` / `CommandMessageBuilder` — Message construction
- `escape_data()` — Byte escaping (`0x5A`→`0x5B01`)
- `calc_crc()` — CRC checksum
- `build_frame()` — Frame assembly
- `send_command()` — Send framed JSON command

**`screen_setup.rs`** — Device controller

- `AioCoolerController` — Main controller struct
- `adb_push()` — Push files via ADB to `/sdcard/pcMedia/`
- `send_image_commands()` — Send transport→transported→waterBlockScreenId sequence
- `calculate_md5()` — File hash for transfer verification

**`app_state.rs`** — UI state management

- `AioCoolerApp` — Application state
- `AppMessage` — Cross-thread messaging (Log, Progress, Success, Error)

## Image Transfer Flow

1. **ADB Push** — Copy image to device at `/sdcard/pcMedia/<timestamp>.png`
2. **transport** — Announce transfer: `{ "type": "media", "fileSize": N, "fileName": "..." }`
3. **transported** — Confirm complete: `{ "md5": "...", "fileName": "..." }`
4. **waterBlockScreenId** — Configure display:
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

## Not Implemented

- **Response parsing**              — `DataConvert.analy()`, `getSerDataByBytes()`
- **Bidirectional communication**   — ACK handling, sequence tracking
- **File streaming**                — Direct serial file transfer (currently using ADB)
- **Device discovery**              — `conn` handshake, capability detection
- **Pump control**                  — `turboPump` command
- **Media management**              — `mediaDelete` command

## APK Source Reference

```
com/baiyi/service/serialservice/serialdataservice/
├── data/
│   ├── entity/
│   │   ├── SerialData.java                # Message container
│   │   └── DataHeader.java                # Header fields
│   ├── manager/
│   │   ├── SerialMsgManager.java          # Frame building, send queue
│   │   └── SerialMsgReceiverHandler.java  # Command handlers
│   └── tool/
│       ├── ByteTools.java                 # Byte conversion, CRC, escaping
│       └── DataConvert.java               # Frame parsing, unescaping
└── serial/
    └── SerialPort2.java                   # Serial port wrapper
```

## Building

```bash
cargo build --release
cargo run
```

