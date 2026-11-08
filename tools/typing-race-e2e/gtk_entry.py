#!/usr/bin/env python3
"""GTK single-line entry that records what it receives.

usage: gtk_entry.py [--gtk3] [--busy] TITLE OUT_FILE

Each Enter appends the entry's text plus a newline to OUT_FILE and clears the
entry. GTK speaks zwp_text_input_v3 with surrounding text, so daklak drives it
through the SurroundingText tier (delete_surrounding_text + commit_string),
unlike terminals, which use ForwardKey.

--gtk3   use GTK 3 instead of GTK 4.
--busy   block the main loop 40 ms out of every 50 ms, like an app under load.
         Wayland events then pile up and are dispatched in batches, which
         shows ordering differences between the keyboard and text-input
         channels (a committed `M` landing before the space typed just
         before it) that an idle entry hides.
"""
import argparse
import time

import gi

BUSY_EVERY_MS = 10
BUSY_BLOCK_S = 0.04


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gtk3", action="store_true")
    ap.add_argument("--busy", action="store_true")
    ap.add_argument("title")
    ap.add_argument("out")
    args = ap.parse_args()

    gi.require_version("Gtk", "3.0" if args.gtk3 else "4.0")
    from gi.repository import GLib, Gtk

    def build(win):
        win.set_title(args.title)
        win.set_default_size(600, 80)
        entry = Gtk.Entry()

        def on_enter(entry):
            with open(args.out, "a", encoding="utf-8") as f:
                f.write(entry.get_text() + "\n")
            entry.set_text("")

        entry.connect("activate", on_enter)
        return entry

    if args.busy:
        # Low priority so window setup and input still run between the blocks.
        GLib.timeout_add(BUSY_EVERY_MS, lambda: time.sleep(BUSY_BLOCK_S) or True,
                         priority=GLib.PRIORITY_LOW)

    if args.gtk3:
        win = Gtk.Window()
        entry = build(win)
        win.connect("destroy", Gtk.main_quit)
        win.add(entry)
        win.show_all()
        entry.grab_focus()
        Gtk.main()
        return

    app = Gtk.Application(application_id=None)

    def on_activate(app):
        win = Gtk.ApplicationWindow(application=app)
        entry = build(win)
        win.set_child(entry)
        win.present()
        entry.grab_focus()

    app.connect("activate", on_activate)
    app.run([])


if __name__ == "__main__":
    main()
