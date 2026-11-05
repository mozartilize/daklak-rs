#!/bin/sh
# Post-install reminder for the daklak:vn xkb option.
# Called by meson install when the XKB option files are installed.

xkb_dir="$1"
user_dir="$2"

echo "daklak: xkb option files installed to $xkb_dir/{symbols/daklak_vn,rules/evdev.post}."
if [ "$xkb_dir" = "$user_dir" ]; then
    stale="$user_dir/rules/evdev"
else
    stale="~/.config/xkb/{symbols/daklak_vn,rules/evdev}"
fi
echo "daklak: copies from a manual setup in $stale take precedence;"
echo "daklak: delete them unless you manage them yourself."
