#!/usr/bin/env python3
"""Case-owned activated D-Bus service: report its actual inherited environment.

Install a service file in the disposable owned Xpra profile only. The private
session bus activates this process, which writes a journal before serving the
Report method. This does not read or edit the host user's bus/profile.
"""
import argparse
import json
import os
from pathlib import Path

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib

parser = argparse.ArgumentParser()
parser.add_argument("--journal", type=Path, required=True)
args = parser.parse_args()

fields = ("DISPLAY", "XAUTHORITY", "GDK_BACKEND", "QT_QPA_PLATFORM", "HOME",
          "XDG_RUNTIME_DIR", "XDG_DATA_HOME", "DBUS_SESSION_BUS_ADDRESS",
          "WAYLAND_DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE")
with args.journal.open("x", encoding="utf-8") as journal:
    json.dump({"kind": "activated-environment", "pid": os.getpid(),
               "environment": {field: os.environ.get(field) for field in fields}}, journal)
    journal.write("\n")

interface = Gio.DBusNodeInfo.new_for_xml("""
<node><interface name="org.cua.OwnedEnvProbe">
  <method name="Report"><arg type="s" direction="out"/></method>
</interface></node>
""").interfaces[0]
connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
loop = GLib.MainLoop()


def method_call(_connection, _sender, _path, _interface, method, _params, invocation):
    if method != "Report":
        invocation.return_dbus_error("org.cua.OwnedEnvProbe.UnknownMethod", method)
        return
    invocation.return_value(GLib.Variant("(s)", ("owned-env-recorded",)))
    GLib.idle_add(loop.quit)


connection.register_object("/org/cua/OwnedEnvProbe", interface, method_call, None, None)
Gio.bus_own_name_on_connection(connection, "org.cua.OwnedEnvProbe",
                               Gio.BusNameOwnerFlags.NONE, None, None)
loop.run()
