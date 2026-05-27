#!/bin/sh
# Post-install script for daklak.
# Called by meson install when evdev_grab is enabled.

if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules 2>/dev/null || true
    udevadm trigger --name-match=uinput 2>/dev/null || true
    echo "daklak: udev rules reloaded."
    echo "daklak: you may also need to add yourself to the 'input' group:"
    echo "  sudo usermod -aG input \$USER"
    echo "  (then log out and back in)"
fi
