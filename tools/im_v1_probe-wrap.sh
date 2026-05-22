#!/usr/bin/env bash
# Launch wrapper for im_v1_probe. KWin calls --inputmethod with this path.
# Logs everything to /tmp/im_v1_probe.log so we can diff vs daklak.
exec "$(dirname "$(readlink -f "$0")")/target/debug/im_v1_probe" \
  >> /tmp/im_v1_probe.log 2>&1
