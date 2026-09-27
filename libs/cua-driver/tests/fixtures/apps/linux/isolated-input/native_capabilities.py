#!/usr/bin/env python3
"""Real GTK3 Wayland/X11 controls with an independent, app-owned event journal.

Each invocation owns one temporary journal and one window. No installed profile,
clipboard, or document is read or modified. Run source and destination as separate
processes for a real cross-client wl_data_device/XDND offer, not a pointer-only drag.
"""
import argparse
import json
import os
import time

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk, GLib, Gtk

parser = argparse.ArgumentParser()
parser.add_argument("--actor", choices=("Source", "Destination", "Target", "Sibling"), required=True)
parser.add_argument("--journal", required=True)
parser.add_argument("--x", type=int, help="Initial position within an owned X11/Xpra display")
parser.add_argument("--y", type=int, help="Initial position within an owned X11/Xpra display")
args = parser.parse_args()
journal = open(args.journal, "x", buffering=1, encoding="utf-8")


def record(kind, **values):
    journal.write(json.dumps({"kind": kind, "time": time.monotonic_ns(),
                              "pid": os.getpid(), **values}, ensure_ascii=False) + "\n")


def named(widget, name):
    widget.get_accessible().set_name(name)
    return widget


window = Gtk.Window(title="Cua Native Capabilities " + args.actor)
window.set_default_size(440, 350)
if (args.x is None) != (args.y is None):
    parser.error("--x and --y must be supplied together")
if args.x is not None:
    window.move(args.x, args.y)
root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
root.set_border_width(16)
window.add(root)
entry = named(Gtk.Entry(), "native-exact-text")
entry.set_placeholder_text("Semantic Unicode target")
entry.connect("changed", lambda widget: record("text", value=widget.get_text()))
root.pack_start(entry, False, False, 0)

button = named(Gtk.Button(label="Activate exact target"), "native-activate")
button.connect("clicked", lambda *_: record("activated"))
root.pack_start(button, False, False, 0)

popup_anchor = named(Gtk.Button(label="Open native popup"), "native-popup-anchor")
root.pack_start(popup_anchor, False, False, 0)
popup = Gtk.Popover.new(popup_anchor)
popup_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
popup_box.set_border_width(12)
popup_button = named(Gtk.Button(label="POPUP_CHILD_CAPTURE_MARKER"), "native-popup-child")
popup_button.connect("clicked", lambda *_: (record("popup-activated"), popup.hide()))
popup_box.pack_start(popup_button, False, False, 0)
popup.add(popup_box)


def open_popup(*_):
    popup.show_all()
    popup.popup()
    record("popup-opened")


popup_anchor.connect("clicked", open_popup)

source = named(Gtk.EventBox(), "native-data-source")
source.set_size_request(280, 55)
source.add(Gtk.Label(label="Drag real UTF-8 offer from this source"))
root.pack_start(source, False, False, 0)
destination = named(Gtk.EventBox(), "native-data-destination")
destination.set_size_request(280, 55)
destination.add(Gtk.Label(label="Drop offer here"))
root.pack_start(destination, False, False, 0)

target = Gtk.TargetEntry.new("text/plain", 0, 0)
source.drag_source_set(Gdk.ModifierType.BUTTON1_MASK, [target], Gdk.DragAction.COPY)
destination.drag_dest_set(Gtk.DestDefaults.ALL, [target], Gdk.DragAction.COPY)


def offer(_widget, _context, selection, _info, _timestamp):
    selection.set_text("Cua native cross-window data offer — 漢字", -1)
    record("offer-sent")


def received(_widget, context, _x, _y, selection, _info, timestamp):
    text = selection.get_text()
    accepted = text == "Cua native cross-window data offer — 漢字"
    record("drop-received", text=text, accepted=accepted)
    Gtk.drag_finish(context, accepted, False, timestamp)


source.connect("drag-data-get", offer)
destination.connect("drag-data-received", received)


def key(_widget, event):
    record("key-press", key=Gdk.keyval_name(event.keyval), modifiers=int(event.state))
    return False


window.connect("key-press-event", key)
window.connect("destroy", Gtk.main_quit)
window.show_all()
record("ready", actor=args.actor, backend=Gdk.Display.get_default().__class__.__name__)


def heartbeat():
    record("heartbeat")
    return True


GLib.timeout_add(100, heartbeat)
Gtk.main()
journal.close()
