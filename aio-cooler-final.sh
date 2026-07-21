#!/usr/bin/env bash
set -euo pipefail

# AIO Liquid Cooler - Correct Protocol Implementation
# Reverse-engineered from com.baiyi.service.serialservice.serialdataservice

SERIAL_DEV="${SERIAL_DEV:-/dev/ttyACM0}"

[[ $# -lt 1 ]] && { echo "Usage: $0 <image_file>"; exit 1; }
IMG="$1"
[[ ! -f "$IMG" ]] && { echo "Error: File not found: $IMG"; exit 1; }

NAME="$(date +%Y-%m-%d_%H-%M-%S-%3N).png"
FILE_SIZE=$(stat -c%s "$IMG")
FILE_MD5=$(md5sum "$IMG" | awk '{print $1}')

echo "[INFO] Image: $IMG → $NAME ($FILE_SIZE bytes)"

echo "[INFO] Pushing image to device..."
adb wait-for-device
adb push "$IMG" "/sdcard/pcMedia/$NAME"

echo "[INFO] Sending commands via $SERIAL_DEV..."

python3 << PYEOF
import serial
import struct
import json
import time
import sys

SERIAL_DEV = "$SERIAL_DEV"
NAME = "$NAME"
FILE_SIZE = $FILE_SIZE
FILE_MD5 = "$FILE_MD5"

def escape_data(data):
    """
    Escape special bytes:
    0x5A -> 0x5B 0x01
    0x5B -> 0x5B 0x02
    """
    result = bytearray()
    for b in data:
        if b == 0x5A:
            result.extend([0x5B, 0x01])
        elif b == 0x5B:
            result.extend([0x5B, 0x02])
        else:
            result.append(b)
    return bytes(result)

def calc_crc(data):
    """Simple sum CRC (1 byte)"""
    return sum(data) & 0xFF

def build_frame(message_bytes):
    """
    Build complete frame:
    [0x5A][length:2bytes BE][escaped_message][CRC:1byte][0x5A]
    """
    # Escape the message
    escaped = escape_data(message_bytes)
    
    # Calculate length (of escaped data)
    length = len(escaped)
    
    # Build frame
    frame = bytearray()
    frame.append(0x5A)  # Start marker
    frame.extend(struct.pack('>H', length))  # 2-byte length, big-endian
    frame.extend(escaped)  # Escaped message
    frame.append(calc_crc(escaped))  # CRC of escaped data
    frame.append(0x5A)  # End marker
    
    return bytes(frame)

def build_message(request_state, cmd_type, version, headers, json_content):
    """
    Build message content:
    POST cmdType version\r\n
    Key=Value\r\n
    ...\r\n
    \r\n
    {json}
    """
    lines = []
    
    # First line: request state, cmd type, version
    lines.append(f"{request_state} {cmd_type} {version}")
    
    # Header lines (Key=Value format!)
    for key, value in headers.items():
        lines.append(f"{key}={value}")
    
    # Join with \r\n, add separator, then content
    header_part = "\r\n".join(lines)
    message = f"{header_part}\r\n\r\n{json_content}"
    
    return message.encode('utf-8')

def send_command(ser, cmd_type, json_dict):
    """Send a command with proper framing"""
    json_content = json.dumps(json_dict, separators=(',', ':'))
    
    seq = int(time.time() * 1000) % 100000
    ts = int(time.time() * 1000)
    
    headers = {
        "SeqNumber": seq,
        "AckNumber": -1,
        "ContentLength": len(json_content),
        "ContentType": "json",
        "FileName": "-1",
        "FileSize": -1,
        "ContentRange": -1,
        "Counter": -1,
        "Date": ts,
        "msgId": -1
    }
    
    message = build_message("POST", cmd_type, "1", headers, json_content)
    frame = build_frame(message)
    
    print(f"[INFO] Sending {cmd_type} ({len(frame)} bytes)")
    print(f"[DEBUG] Frame hex: {frame[:30].hex()}...{frame[-10:].hex()}")
    
    ser.write(frame)
    ser.flush()
    time.sleep(0.5)

try:
    ser = serial.Serial(SERIAL_DEV, 115200, timeout=2)
    time.sleep(0.5)
    ser.reset_input_buffer()
    ser.reset_output_buffer()
    
    # Send transport command
    send_command(ser, "transport", {
        "type": "media",
        "fileSize": FILE_SIZE,
        "fileName": NAME
    })
    
    time.sleep(0.3)
    
    # Send transported command
    send_command(ser, "transported", {
        "md5": FILE_MD5,
        "fileName": NAME
    })
    
    time.sleep(0.3)
    
    # Send screen config
    send_command(ser, "waterBlockScreenId", {
        "id": "Customization",
        "screenMode": "Full Screen",
        "playMode": "Single",
        "ratio": "2:1",
        "media": [NAME],
        "settings": {
            "color": "#dcdcdc",
            "align": "Left",
            "filter": {"value": None, "opacity": 100},
            "badges": ["GPU Badge", "CPU Badge"]
        },
        "sysinfoDisplay": ["CPU Temperature", "GPU Temperature"]
    })
    
    time.sleep(1)
    ser.close()
    print("[INFO] Commands sent successfully!")
    
except Exception as e:
    print(f"[ERROR] {e}")
    sys.exit(1)
PYEOF

echo "[INFO] Done! Check your AIO cooler display."

