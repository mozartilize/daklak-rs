#!/usr/bin/env bash
#
# ibus-lifecycle-probe.sh — driver for the daklak IBus lifecycle probe.
#
# Ephemeral dev tool, NOT part of the product. Answers, on a REAL ibus session:
#   Q1  Does ibus-daemon SIGTERM a spawned engine when it drops its D-Bus
#       connection? (drop = simulate switching the transport to evdev grab)
#   Q2  Can the engine re-register afterwards and resume routing without the
#       user re-selecting it? (reconnect = simulate switching back to native)
#
# The probe binary reuses daklak's real registration path, so ibus-daemon runs
# the identical bus/component.c spawn/lifecycle code as for production daklak.
#
# Usage:
#   ./ibus-lifecycle-probe.sh install     # build + install component + ibus restart
#   ./ibus-lifecycle-probe.sh install --impersonate
#                                         # same, but register as the REAL daklak
#                                         # names (org.freedesktop.IBus.Daklak /
#                                         # engine 'daklak'). Only when daklak is
#                                         # NOT installed.
#   ./ibus-lifecycle-probe.sh pid         # show probe PID + parent (expect ibus-daemon)
#   ./ibus-lifecycle-probe.sh drop        # SIGUSR1 → drop connection, watch survival 5s
#   ./ibus-lifecycle-probe.sh reconnect   # SIGUSR2 → re-register, then retest typing
#   ./ibus-lifecycle-probe.sh log         # print the event timeline
#   ./ibus-lifecycle-probe.sh watch       # tail -f the timeline
#   ./ibus-lifecycle-probe.sh cleanup     # remove component + kill probe + ibus restart
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
COMPONENT_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/ibus/component"
COMPONENT_FILE="$COMPONENT_DIR/daklak-probe.xml"
LOG="${XDG_RUNTIME_DIR:-/tmp}/daklak-ibus-probe.log"
BIN="$REPO_ROOT/target/debug/ibus_lifecycle_probe"

# Match the ibus-spawned probe by its binary path. NOTE: `pgrep -x <name>`
# can't be used — Linux truncates comm to 15 chars and "ibus_lifecycle_probe"
# is 20, so an exact-name match never hits. `-f "$BIN"` matches the full
# command line (the component <exec>), which is unambiguous and dash-safe
# (the driver script uses dashes, the binary uses underscores).
probe_pid() { pgrep -f "$BIN" 2>/dev/null || true; }

require_running() {
  local p; p="$(probe_pid | head -n1)"
  if [ -z "$p" ]; then
    echo "probe not running — did you 'install' and select the 'daklak-probe' engine?" >&2
    exit 1
  fi
  echo "$p"
}

case "${1:-help}" in
  install)
    IMPERSONATE=0
    [ "${2:-}" = "--impersonate" ] && IMPERSONATE=1
    cargo build -p probe --bin ibus_lifecycle_probe
    mkdir -p "$COMPONENT_DIR"

    if [ "$IMPERSONATE" = 1 ]; then
      bus_name="org.freedesktop.IBus.Daklak"
      engine_name="daklak"
      longname="Daklak (probe impersonation)"
      exec_line="$BIN --name $bus_name"
      # Guard: refuse to shadow a real daklak component.
      for d in "$COMPONENT_DIR" /usr/share/ibus/component /usr/local/share/ibus/component; do
        if [ -e "$d/daklak.xml" ]; then
          echo "REFUSING: real daklak component exists at $d/daklak.xml" >&2
          echo "Impersonation would collide. Uninstall daklak first, or drop --impersonate." >&2
          exit 1
        fi
      done
      echo "!! IMPERSONATION MODE: registering as $bus_name / engine '$engine_name'"
    else
      bus_name="org.freedesktop.IBus.DaklakProbe"
      engine_name="daklak-probe"
      longname="Daklak Lifecycle Probe"
      exec_line="$BIN"
    fi

    sed -e "s|@PROBE_EXEC@|$exec_line|g" \
        -e "s|@BUS_NAME@|$bus_name|g" \
        -e "s|@ENGINE_NAME@|$engine_name|g" \
        -e "s|@LONGNAME@|$longname|g" \
        "$SCRIPT_DIR/daklak-probe.component.xml.in" > "$COMPONENT_FILE"
    : > "$LOG" || true
    echo "installed: $COMPONENT_FILE"
    echo "exec:      $exec_line"
    echo "engine:    $engine_name   (bus name: $bus_name)"
    ibus write-cache
    ibus restart
    cat <<EOF

Next steps (in a real GNOME/IBus session):
  1. Select the engine:   ibus engine $engine_name
                          (or GNOME Settings / ibus-setup → add "$longname")
  2. Type in any text field — keys pass through normally = engine is alive.
  3. $0 pid         # confirm the parent is ibus-daemon (spawned child)
  4. $0 drop        # simulate switch → evdev; watch whether ibus kills us
  5. $0 reconnect   # simulate switch → native; then retype WITHOUT re-selecting
  6. $0 log         # read the full timeline
  7. $0 cleanup     # remove component + ibus restart
EOF
    ;;

  pid)
    p="$(require_running)"
    # Show every matching instance (a 2nd one appearing = ibus re-spawned us).
    n=0
    for one in $(probe_pid); do
      n=$((n+1))
      ppid="$(ps -o ppid= -p "$one" | tr -d ' ')"
      echo "probe PID : $one   parent: $ppid ($(ps -o comm= -p "$ppid" 2>/dev/null || echo '?'))"
    done
    echo "(expect ONE instance, parent = ibus-daemon → confirms ibus spawned & tracks this PID)"
    [ "$n" -gt 1 ] && echo "WARNING: $n instances — ibus spawned a second copy."
    ;;

  drop)
    p="$(require_running)"
    echo "SIGUSR1 → $p : drop zbus connection (simulate switch → evdev grab)"
    kill -USR1 "$p"
    for i in 1 2 3 4 5; do
      sleep 1
      if kill -0 "$p" 2>/dev/null; then
        echo "  t+${i}s: PID $p ALIVE"
      else
        echo "  t+${i}s: PID $p GONE — ibus-daemon killed it after the drop (Q1 = killed)"
        exit 0
      fi
    done
    echo "→ survived 5s after connection drop (Q1 = survives; matches vendored ibus source)"
    ;;

  reconnect)
    p="$(require_running)"
    echo "SIGUSR2 → $p : reconnect + re-register (simulate switch → native ibus)"
    kill -USR2 "$p"
    sleep 1
    echo "--- last log lines ---"
    tail -n 6 "$LOG" 2>/dev/null || true
    echo "----------------------"
    echo "Now retype in a text field WITHOUT re-selecting the engine:"
    echo "  - keys route again → Q2 = switch-back viable"
    echo "  - dead/no input, or a 2nd probe PID appeared → Q2 = needs work (see 'pid')"
    ;;

  log)   tail -n 200 "$LOG" 2>/dev/null || echo "no log at $LOG";;
  watch) exec tail -f "$LOG";;

  cleanup)
    rm -f "$COMPONENT_FILE" && echo "removed $COMPONENT_FILE"
    for p in $(probe_pid); do
      kill -TERM "$p" 2>/dev/null && echo "sent SIGTERM to $p" || true
    done
    ibus write-cache || true
    ibus restart || true
    echo "ibus restarted"
    ;;

  *)
    echo "usage: $0 {install [--impersonate]|pid|drop|reconnect|log|watch|cleanup}"
    ;;
esac
