# SPDX-License-Identifier: GPL-3.0-or-later
#
# Nautilus "Send to" menu for quickshare-daemon.
#
# Structure adapted from Packet's `packet_nautilus.py.in` (GPL-3.0-or-later,
# https://github.com/nozwock/packet, commit f634d66). The menu it builds is
# different: Packet hands files to its GUI, which then shows a device picker,
# whereas there is no GUI here -- so the device list has to be in the menu.
#
# Install to ~/.local/share/nautilus-python/extensions/ and restart Nautilus
# with `nautilus -q`.

import sys
from typing import Any, List, Optional

import gi

gi.require_version("Gio", "2.0")
gi.require_version("GLib", "2.0")
gi.require_version("Nautilus", "4.1")
from gi.repository import Gio, GLib, GObject, Nautilus  # noqa: E402

BUS_NAME = "org.quickshare.Daemon1"
OBJECT_PATH = "/org/quickshare/Daemon1"
INTERFACE = "org.quickshare.Daemon1"

# How long to keep discovery running after a menu is built. The daemon reference
# counts leases and applies its own idle grace on top, so this only needs to be
# long enough for someone to read the menu and decide.
DISCOVERY_SECONDS = 60


def log(*values: Any) -> None:
    print("quickshare:", *values, file=sys.stderr)


class Daemon:
    """Thin wrapper over the daemon's D-Bus API.

    Uses GDBus rather than `python3-dbus` because Nautilus already depends on
    GIO, so the extension adds no dependency of its own.
    """

    def __init__(self) -> None:
        self._proxy: Optional[Gio.DBusProxy] = None

    def proxy(self) -> Optional[Gio.DBusProxy]:
        """The proxy, or None when no daemon is running.

        Rebuilt on failure rather than cached forever: the daemon can be
        restarted under a long-lived Nautilus, and a dead proxy would make the
        menu permanently empty.
        """
        if self._proxy is not None:
            return self._proxy
        try:
            self._proxy = Gio.DBusProxy.new_for_bus_sync(
                Gio.BusType.SESSION,
                # Do not auto-start: there is no D-Bus activation file, and
                # waiting on one would block the menu.
                Gio.DBusProxyFlags.DO_NOT_AUTO_START,
                None,
                BUS_NAME,
                OBJECT_PATH,
                INTERFACE,
                None,
            )
            # A proxy is returned even when nothing owns the name, so this is
            # what actually tells us whether a daemon is there.
            if self._proxy.get_name_owner() is None:
                self._proxy = None
        except GLib.Error as error:
            log("cannot reach the daemon:", error.message)
            self._proxy = None
        return self._proxy

    def call(self, method: str, params: Optional[GLib.Variant] = None) -> Optional[GLib.Variant]:
        proxy = self.proxy()
        if proxy is None:
            return None
        try:
            # Short timeout: this runs while Nautilus builds a context menu,
            # and a stalled call would freeze the file manager.
            return proxy.call_sync(method, params, Gio.DBusCallFlags.NONE, 2000, None)
        except GLib.Error as error:
            log(f"{method} failed:", error.message)
            self._proxy = None
            return None

    def start_discovery(self) -> None:
        self.call("StartDiscovery")

    def stop_discovery(self) -> None:
        self.call("StopDiscovery")

    def devices(self) -> List[dict]:
        result = self.call("ListDevices")
        return list(result[0]) if result is not None else []

    def send_files(self, device_id: str, paths: List[str]) -> None:
        self.call("SendFiles", GLib.Variant("(sas)", (device_id, paths)))


class QuickShareMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    def __init__(self) -> None:
        super().__init__()
        self.daemon = Daemon()
        self._stop_timer: Optional[int] = None

    # -- discovery lifecycle -------------------------------------------------

    def _hold_discovery(self) -> None:
        """Start discovery, and arrange to release it.

        Nautilus is long-lived, and the daemon keys leases on our bus name, so
        a lease taken here would otherwise be held until Nautilus exits --
        keeping mDNS and the BLE advertiser running all day.
        """
        self.daemon.start_discovery()
        if self._stop_timer is not None:
            GLib.source_remove(self._stop_timer)
        self._stop_timer = GLib.timeout_add_seconds(
            DISCOVERY_SECONDS, self._release_discovery
        )

    def _release_discovery(self) -> bool:
        self._stop_timer = None
        self.daemon.stop_discovery()
        return GLib.SOURCE_REMOVE

    # -- menu ----------------------------------------------------------------

    def _send(self, _item: Nautilus.MenuItem, device_id: str, paths: List[str]) -> None:
        self.daemon.send_files(device_id, paths)

    def _pick_then_send(self, _item: Nautilus.MenuItem, paths: List[str]) -> None:
        """Fallback for when no device is known yet.

        The menu is built synchronously but discovery takes a few seconds, so
        the first time it is opened the list is usually empty. Rather than show
        an empty submenu, offer a picker that can wait -- PLAN.md's zenity
        fallback.
        """
        self.daemon.start_discovery()

        def poll(attempt: int = 0) -> bool:
            devices = [d for d in self.daemon.devices() if d.get("reachable")]
            if not devices:
                # ~15s, which is about how long a phone takes to appear after
                # its share sheet is opened.
                return GLib.SOURCE_CONTINUE if attempt < 15 else self._no_devices()
            self._choose(devices, paths)
            return GLib.SOURCE_REMOVE

        attempts = {"n": 0}

        def tick() -> bool:
            attempts["n"] += 1
            return poll(attempts["n"])

        GLib.timeout_add_seconds(1, tick)

    def _no_devices(self) -> bool:
        self._notify(
            "No devices found",
            "Open Quick Share on the other device so it becomes visible.",
        )
        return GLib.SOURCE_REMOVE

    def _choose(self, devices: List[dict], paths: List[str]) -> None:
        """Shows a zenity list and sends to the chosen device."""
        rows: List[str] = []
        for device in devices:
            rows += [str(device.get("id", "")), str(device.get("name", "Unknown device"))]

        try:
            # --print-column=1 returns the id, which is what SendFiles wants,
            # while --hide-column=1 keeps it out of the user's way.
            process = Gio.Subprocess.new(
                [
                    "zenity", "--list", "--title=Quick Share",
                    "--text=Send to which device?",
                    "--column=id", "--column=Device",
                    "--hide-column=1", "--print-column=1",
                ] + rows,
                Gio.SubprocessFlags.STDOUT_PIPE,
            )
            ok, stdout, _ = process.communicate_utf8(None, None)
        except GLib.Error as error:
            log("zenity failed:", error.message)
            return

        chosen = (stdout or "").strip()
        if ok and chosen:
            self.daemon.send_files(chosen, paths)

    def _notify(self, summary: str, body: str) -> None:
        notification = Gio.Notification.new(summary)
        notification.set_body(body)
        Gio.Application.get_default() and Gio.Application.get_default().send_notification(
            None, notification
        )

    def get_file_items(self, files: List[Nautilus.FileInfo]) -> List[Nautilus.MenuItem]:
        # Quick Share sends files, not directories, and rqs_lib rejects them.
        if not files or any(f.is_directory() for f in files):
            return []
        if self.daemon.proxy() is None:
            # No daemon: show nothing rather than a menu item that fails.
            return []

        paths = [f.get_location().get_path() for f in files]
        paths = [p for p in paths if p]
        if not paths:
            # Non-local files (trash, remote mounts) have no path to send.
            return []

        self._hold_discovery()
        devices = [d for d in self.daemon.devices() if d.get("reachable")]

        top = Nautilus.MenuItem(
            name="QuickShare::SendTo",
            label="Send with Quick Share",
        )

        if not devices:
            # Nothing known yet -- the common case on first open, since the
            # menu is built before discovery has had time to answer.
            top.connect("activate", self._pick_then_send, paths)
            return [top]

        submenu = Nautilus.Menu()
        for device in devices:
            device_id = str(device.get("id", ""))
            name = str(device.get("name", "Unknown device"))
            entry = Nautilus.MenuItem(
                name=f"QuickShare::Send::{device_id}",
                label=name,
            )
            entry.connect("activate", self._send, device_id, paths)
            submenu.append_item(entry)

        # Always offered: a device that appears after the menu was built would
        # otherwise be unreachable without closing and reopening it.
        more = Nautilus.MenuItem(
            name="QuickShare::Send::Other",
            label="Other device…",
        )
        more.connect("activate", self._pick_then_send, paths)
        submenu.append_item(more)

        top.set_submenu(submenu)
        return [top]
