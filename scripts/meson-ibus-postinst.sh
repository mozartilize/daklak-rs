#!/bin/sh
# Post-install notice for daklak's IBus component.
set -eu

component="${MESON_INSTALL_PREFIX:-$HOME/.local}/share/ibus/component/daklak.xml"

echo "daklak: IBus component installed. Refresh IBus after install:"
echo "  ibus write-cache"
echo "  ibus restart"
echo "daklak: if GNOME Settings does not list Daklak from the user-local component, run:"
echo "  sudo cp $component /usr/share/ibus/component/daklak.xml"
echo "  ibus write-cache"
echo "  ibus restart"
echo "  # or log out and back in"
