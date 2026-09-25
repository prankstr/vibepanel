#!/usr/bin/env python3
"""Keep a virtual pointer alive while running tests in the private compositor session.

Tests write normalized coordinates, optionally followed by 'click' or 'right-click', to VIBEPANEL_TEST_POINTER.
Sway tests can also use seat cursor IPC. No third-party modules required.
Protocol: https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1
"""

import os
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path


def words(*values):
    return struct.pack("=" + "I" * len(values), *values)


def send(connection, object_id, opcode, body):
    connection.sendall(words(object_id, ((8 + len(body)) << 16) | opcode) + body)


def read_exact(connection, size):
    result = b""
    while len(result) < size:
        chunk = connection.recv(size - len(result))
        if not chunk:
            raise RuntimeError("Wayland connection closed")
        result += chunk
    return result


with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
    connection.settimeout(5)
    connection.connect(os.environ["WAYLAND_DISPLAY"])
    send(connection, 1, 1, words(2))  # wl_display.get_registry
    send(connection, 1, 0, words(3))  # wl_display.sync
    manager = None
    while True:
        object_id, header = struct.unpack("=II", read_exact(connection, 8))
        body = read_exact(connection, (header >> 16) - 8)
        if object_id == 3:
            break
        if object_id == 2 and header & 0xFFFF == 0:
            name, size = struct.unpack("=II", body[:8])
            if body[8:8 + size - 1] == b"zwlr_virtual_pointer_manager_v1":
                manager = name
    if manager is None:
        raise RuntimeError("Headless compositor lacks virtual-pointer support")
    interface = b"zwlr_virtual_pointer_manager_v1\0"
    padded = interface + b"\0" * (-len(interface) % 4)
    send(connection, 2, 0, words(manager, len(interface)) + padded + words(1, 4))
    send(connection, 4, 0, words(0, 5))  # create_virtual_pointer(default seat)
    send(connection, 1, 0, words(6))
    while True:
        object_id, header = struct.unpack("=II", read_exact(connection, 8))
        read_exact(connection, (header >> 16) - 8)
        if object_id == 6:
            break
    control = Path(os.environ["XDG_RUNTIME_DIR"]) / "test-pointer"
    os.environ["VIBEPANEL_TEST_POINTER"] = str(control)
    process = subprocess.Popen(sys.argv[1:])
    previous = None
    while process.poll() is None:
        if control.exists():
            value = control.read_text()
            parts = value.split()
            if value != previous and len(parts) in (2, 3):
                x, y = map(int, parts[:2])
                if previous is None or previous.split()[:2] != parts[:2]:
                    send(connection, 5, 1, words(int(time.monotonic() * 1000) & 0xFFFFFFFF, x, y, 1280, 800))
                    send(connection, 5, 4, b"")
                if parts[2:] in (["click"], ["right-click"]):
                    button = 273 if parts[2] == "right-click" else 272
                    for state in (1, 0):
                        send(connection, 5, 2, words(int(time.monotonic() * 1000) & 0xFFFFFFFF, button, state))
                        send(connection, 5, 4, b"")
                previous = value
        time.sleep(0.01)
    sys.exit(process.returncode)
