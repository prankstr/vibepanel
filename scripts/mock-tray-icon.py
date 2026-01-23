#!/usr/bin/env python3
"""
Mock StatusNotifierItem for testing tray icon inversion.

Usage:
    ./mock-tray-icon.py [color]
    
    color: white, black, gray, or a hex color like "ff0000" (default: white)

This creates a tray icon with a solid color pixmap for testing
the automatic grayscale inversion feature.
"""

import sys
import struct
import dbus
import dbus.service
import dbus.mainloop.glib
from gi.repository import GLib

ITEM_INTERFACE = "org.kde.StatusNotifierItem"


def parse_color(color_arg):
    """Parse color argument to RGB tuple."""
    color = color_arg.lower()
    if color == "white":
        return (255, 255, 255)
    elif color == "black":
        return (0, 0, 0)
    elif color == "gray" or color == "grey":
        return (128, 128, 128)
    elif color == "red":
        return (255, 0, 0)
    elif color == "green":
        return (0, 255, 0)
    elif color == "blue":
        return (0, 0, 255)
    elif len(color) == 6:
        try:
            r = int(color[0:2], 16)
            g = int(color[2:4], 16)
            b = int(color[4:6], 16)
            return (r, g, b)
        except ValueError:
            pass
    print(f"Unknown color '{color_arg}', using white")
    return (255, 255, 255)


def create_circle_pixmap(size, r, g, b, alpha=255):
    """
    Create a circular icon with transparent background.
    Format is ARGB in network byte order (big-endian).
    """
    pixels = []
    center = size // 2
    radius = size // 2 - 2  # Small margin
    
    for y in range(size):
        for x in range(size):
            dx = x - center
            dy = y - center
            dist_sq = dx * dx + dy * dy
            
            if dist_sq <= radius * radius:
                # Inside circle - use the color
                pixels.append(struct.pack(">BBBB", alpha, r, g, b))
            else:
                # Outside circle - transparent
                pixels.append(struct.pack(">BBBB", 0, 0, 0, 0))
    
    return b"".join(pixels)


class MockTrayItem(dbus.service.Object):
    """Mock StatusNotifierItem implementation."""
    
    def __init__(self, bus, path, color):
        dbus.service.Object.__init__(self, bus, path)
        self.r, self.g, self.b = color
        self.color_name = f"#{self.r:02x}{self.g:02x}{self.b:02x}"
        
        # Create a 64x64 circular icon
        self.pixmap_data = create_circle_pixmap(64, self.r, self.g, self.b)
        
    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature='ss', out_signature='v')
    def Get(self, interface, prop):
        return self.GetAll(interface)[prop]
    
    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature='s', out_signature='a{sv}')
    def GetAll(self, interface):
        if interface != ITEM_INTERFACE:
            return {}
        
        # Pixmap format: a(iiay) - array of (width, height, data)
        pixmap = dbus.Array([
            dbus.Struct((
                dbus.Int32(64),
                dbus.Int32(64),
                dbus.Array([dbus.Byte(b) for b in self.pixmap_data], signature='y')
            ), signature='(iiay)')
        ], signature='(iiay)')
        
        return {
            'Id': dbus.String('mock-tray-icon'),
            'Category': dbus.String('ApplicationStatus'),
            'Status': dbus.String('Active'),
            'Title': dbus.String(f'Mock Icon {self.color_name}'),
            'IconName': dbus.String(''),
            'IconPixmap': pixmap,
            'AttentionIconName': dbus.String(''),
            'AttentionIconPixmap': dbus.Array([], signature='(iiay)'),
            'ToolTip': dbus.Struct((
                dbus.String(''),
                dbus.Array([], signature='(iiay)'),
                dbus.String(f'Mock Tray Icon'),
                dbus.String(f'Color: {self.color_name}')
            ), signature='(sa(iiay)ss)'),
            'ItemIsMenu': dbus.Boolean(False),
            'Menu': dbus.ObjectPath('/NO_MENU'),
            'IconThemePath': dbus.String(''),
        }
    
    @dbus.service.method(ITEM_INTERFACE, in_signature='ii')
    def Activate(self, x, y):
        print(f"Activated at ({x}, {y})")
    
    @dbus.service.method(ITEM_INTERFACE, in_signature='ii')
    def SecondaryActivate(self, x, y):
        print(f"Secondary activated at ({x}, {y})")
    
    @dbus.service.method(ITEM_INTERFACE, in_signature='is')
    def Scroll(self, delta, orientation):
        print(f"Scroll: {delta} {orientation}")
    
    @dbus.service.method(ITEM_INTERFACE, in_signature='ii')
    def ContextMenu(self, x, y):
        print(f"Context menu at ({x}, {y})")


def main():
    color_arg = sys.argv[1] if len(sys.argv) > 1 else "white"
    color = parse_color(color_arg)
    
    print(f"Creating mock tray icon with color RGB({color[0]}, {color[1]}, {color[2]})")
    
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    
    bus = dbus.SessionBus()
    item_path = "/StatusNotifierItem"
    item = MockTrayItem(bus, item_path, color)
    
    bus_name = bus.get_unique_name()
    
    try:
        watcher = bus.get_object("org.kde.StatusNotifierWatcher", "/StatusNotifierWatcher")
        watcher_iface = dbus.Interface(watcher, "org.kde.StatusNotifierWatcher")
        service_name = f"{bus_name}{item_path}"
        watcher_iface.RegisterStatusNotifierItem(service_name)
        print(f"Registered with watcher as: {service_name}")
    except dbus.exceptions.DBusException as e:
        print(f"Failed to register with watcher: {e}")
        return 1
    
    print("Running... Press Ctrl+C to exit")
    
    loop = GLib.MainLoop()
    try:
        loop.run()
    except KeyboardInterrupt:
        print("\nExiting")
    
    return 0


if __name__ == "__main__":
    sys.exit(main())
