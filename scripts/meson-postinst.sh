#!/bin/sh
# Post-install reminder for daklak evdev mode.
# Called by meson install when evdev_grab is enabled.

echo "daklak: evdev hooks and keymap installed."
echo "daklak: device permissions are NOT handled by this package."
echo "daklak: enable /dev/uinput access by activating the udev rule:"
echo "  getent group uinput || sudo groupadd --system uinput"
echo "  sudo cp /usr/share/daklak/99-daklak-input.rules /etc/udev/rules.d/99-daklak.rules"
echo "  sudo udevadm control --reload-rules"
echo "  sudo udevadm trigger --name-match=uinput"
echo "  sudo usermod -aG input,uinput \$USER   # then log out + back in"
