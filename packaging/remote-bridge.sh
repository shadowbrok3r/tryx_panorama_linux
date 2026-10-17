#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# remote-bridge.sh — expose this machine's TRYX Panorama cooler to a remote
# desktop GUI/CLI over the LAN.
#
# Run this on the box physically wired to the cooler (USB serial + ADB).
# It starts BOTH channels the controller needs:
#   1. a serial<->TCP bridge  (device commands)      -> tcp://THIS_HOST:9600
#   2. a shared ADB server    (image push over adb)  -> tcp:THIS_HOST:5037
#
# Then on your desktop machine:
#   # GUI: set the "Device" field to  tcp://THIS_HOST:9600
#   tryx_panorama_linux            # (built --features gui)
#   # or CLI, e.g.:
#   tryx_panorama_linux --port tcp://THIS_HOST:9600 spec
# A tcp:// device sends adb to tcp:THIS_HOST:5037 by itself; set
# ADB_SERVER_SOCKET only to point somewhere else.
#
# Keep the tryx daemon running here: when it is, the bridge relays through its
# socket instead of opening the serial port, and the daemon keeps the display
# alive (and the fan curve fed) between remote commands.
#
# SECURITY: neither channel is authenticated. Anyone who can reach these ports
# gets full control of the cooler + adb to the Android board. Only run this on a
# trusted LAN, or restrict BIND_ADDR / firewall the ports.
# ---------------------------------------------------------------------------
set -euo pipefail

SERIAL_DEV="${SERIAL_DEV:-/dev/tryx0}"
BRIDGE_PORT="${BRIDGE_PORT:-9600}"
ADB_PORT="${ADB_PORT:-5037}"
BIND_ADDR="${BIND_ADDR:-0.0.0.0}"   # set to a specific LAN IP to limit exposure
BIN="${BIN:-tryx_panorama_linux}"   # override with a path to the built binary

# Best-effort LAN IP for the printed instructions.
HOST_IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
[ -z "${HOST_IP}" ] && HOST_IP="$(ip route get 1.1.1.1 2>/dev/null | awk '{print $7; exit}')"
[ -z "${HOST_IP}" ] && HOST_IP="THIS_HOST"

# A USB device can be claimed by only ONE adb server, so replace the local one
# with a shared server that listens on all interfaces.
echo ">> Restarting adb as a shared server on ${BIND_ADDR}:${ADB_PORT} ..."
adb kill-server >/dev/null 2>&1 || true
adb -a -P "${ADB_PORT}" nodaemon server >/tmp/tryx-adb-shared.log 2>&1 &
ADB_PID=$!

cleanup() {
    echo
    echo ">> Stopping bridge + shared adb server ..."
    kill "${ADB_PID}" >/dev/null 2>&1 || true
    adb kill-server >/dev/null 2>&1 || true
    adb start-server >/dev/null 2>&1 || true   # restore a normal local server
}
trap cleanup EXIT INT TERM

sleep 1
echo
echo "=========================================================================="
echo " TRYX Panorama remote bridge is up."
echo
echo " On your DESKTOP machine:"
echo "   # GUI: set 'Device' to   tcp://${HOST_IP}:${BRIDGE_PORT}"
echo "   # (adb follows a tcp:// device to tcp:${HOST_IP}:5037 by itself;"
echo "   #  export ADB_SERVER_SOCKET=tcp:${HOST_IP}:${ADB_PORT} if you changed ADB_PORT)"
echo "   tryx_panorama_linux                     # (built --features gui)"
echo "   # CLI example:"
echo "   tryx_panorama_linux --port tcp://${HOST_IP}:${BRIDGE_PORT} spec"
echo "=========================================================================="
echo

# Serial bridge runs in the foreground; Ctrl-C stops everything (see trap).
exec "${BIN}" --port "${SERIAL_DEV}" bridge --listen "${BIND_ADDR}:${BRIDGE_PORT}"
