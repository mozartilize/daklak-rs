#!/usr/bin/env python3
"""Stress daklak with timing-varied real keystrokes and report wrong output.

Types the same Telex input many times into a throw-away window through a
temporary uinput keyboard, varying the per-key gap and hold time from a seeded
RNG (holds longer than the gap give real key rollover: the next key is pressed
before the previous one is released). Every line the window receives is
compared with the expected text; mismatches are printed with their UTC time
(to line up with daklak's log) and the exact key schedule, and can be replayed
with --replay.

Targets:
  ghostty, foot, kitty  terminal running `cat > FILE` (ForwardKey tier)
  gtk                   GTK4 Gtk.Entry (SurroundingText tier)
  firefox, chromium     local page with a text field (SurroundingText tier);
                        --field picks a native element or a chat-style editor
                        (prosemirror, lexical, tg-like), --browser-bin the binary

Window focus is driven through scrollmsg / swaymsg, or KWin scripting on
KDE. On KDE, KWin launches daklak itself (kwinrc [Wayland] InputMethod), so
the run uses that instance and its DAKLAK_LOG_PATH; --restart-im respawns it
to pick up a new build.

Example (the hạnh → hạn report: tone key j, then a quick space):
  tools/typing-race-e2e/run.py -t ghostty -p tone-space -n 50 'hanhj ' -e 'hạnh '
  tools/typing-race-e2e/run.py -t gtk -p rollover -n 50 'hanhj ' -e 'hạnh '

Requires python-evdev and write access to /dev/uinput. Don't type or change
focus while it runs; the run aborts if the target loses focus.
"""
import argparse
import collections
import datetime
import json
import os
import random
import shutil
import signal
import subprocess
import sys
import tempfile
import time

import browser_page
import wm
from evdev import UInput, ecodes as e

HERE = os.path.dirname(os.path.abspath(__file__))

PLAIN = {c: getattr(e, "KEY_" + c.upper()) for c in "abcdefghijklmnopqrstuvwxyz0123456789"}
PLAIN.update({
    " ": e.KEY_SPACE, "-": e.KEY_MINUS, "=": e.KEY_EQUAL, "[": e.KEY_LEFTBRACE,
    "]": e.KEY_RIGHTBRACE, "\\": e.KEY_BACKSLASH, ";": e.KEY_SEMICOLON,
    "'": e.KEY_APOSTROPHE, "`": e.KEY_GRAVE, ",": e.KEY_COMMA, ".": e.KEY_DOT,
    "/": e.KEY_SLASH,
})
SHIFTED = {c.upper(): PLAIN[c] for c in "abcdefghijklmnopqrstuvwxyz"}
SHIFTED.update({s: PLAIN[b] for s, b in zip('!@#$%^&*()_+{}|:"~<>?', "1234567890-=[]\\;'`,./")})

TONE_KEYS = "sfrxjSFRXJ"

# Editing keys, written in the typed text as {NAME}.
SPECIAL = {"\x01": e.KEY_LEFT, "\x02": e.KEY_RIGHT, "\x08": e.KEY_BACKSPACE, "\x04": e.KEY_BACKSPACE}
SPECIAL_NAMES = {"{L}": "\x01", "{R}": "\x02", "{BS}": "\x08", "{CBS}": "\x04", "{P}": "\x03"}
# {P}: no key, just an idle pause longer than daklak's 2 s idle policy.
IDLE_PAUSE_MS = 2300


def expand_specials(text):
    for name, ch in SPECIAL_NAMES.items():
        text = text.replace(name, ch)
    return text

# name: (gap ms, hold ms, pause-before chars, pause seconds, tight-after chars)
PROFILES = {
    # ghostty-e2e's pace: no overlap.
    "steady": ((110, 130), (25, 35), "", 0.0, ""),
    # A fast typist: holds often exceed the gap, so keys overlap.
    "rollover": ((35, 90), (60, 140), "", 0.0, ""),
    # Very fast bursts with heavy overlap.
    "burst": ((8, 30), (40, 90), "", 0.0, ""),
    # Fast typing, and the key after a tone key follows within 5-20 ms.
    "tone-space": ((40, 90), (50, 110), "", 0.0, TONE_KEYS),
    # Pause > 2 s before the tone key (idle policy), then a quick next key.
    "pause-tone": ((40, 90), (50, 110), TONE_KEYS, 2.3, TONE_KEYS),
}


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%H:%M:%S.%fZ")


def range_arg(s):
    lo, _, hi = s.partition(":")
    lo = float(lo)
    return (lo, float(hi) if hi else lo)


# ── daklak process ──────────────────────────────────────────────────────────


def daklak_pid():
    r = subprocess.run(["pgrep", "-xo", "daklak"], capture_output=True, text=True)
    return int(r.stdout.split()[0]) if r.returncode == 0 else None


def daklak_log_path(pid):
    """DAKLAK_LOG_PATH of a running daklak (KWin starts it with its own env)."""
    try:
        with open(f"/proc/{pid}/environ", "rb") as f:
            env = dict(kv.split(b"=", 1) for kv in f.read().split(b"\0") if b"=" in kv)
    except OSError:
        return None
    path = env.get(b"DAKLAK_LOG_PATH")
    return path.decode() if path else None


def restart_kwin_im(old_pid, timeout=10):
    """KWin respawns its input method after a crash exit; SIGKILL counts as one
    (a clean exit does not), so this picks up a freshly built binary."""
    os.kill(old_pid, signal.SIGKILL)
    deadline = time.time() + timeout
    while time.time() < deadline:
        pid = daklak_pid()
        if pid and pid != old_pid:
            time.sleep(1.0)  # bind input method + keymap
            return pid
        time.sleep(0.2)
    raise SystemExit("run.py: KWin did not restart daklak")


def log_size(path):
    try:
        return os.path.getsize(path)
    except OSError:
        return 0


def save_log_slice(path, start, dest):
    """Copy the log written since byte `start` (whole file if it was truncated)."""
    try:
        with open(path, "rb") as f:
            if log_size(path) >= start:
                f.seek(start)
            data = f.read()
    except OSError:
        return False
    with open(dest, "wb") as f:
        f.write(data)
    return True


def write_browser_trace(path, session, context, report=None):
    with open(path, "w", encoding="utf-8") as f:
        # UTF-16 preview clipping may leave a lone surrogate. JSON's ASCII
        # escapes preserve it without breaking UTF-8 artifact serialization.
        json.dump({**context, "report": report, "harness": session.diagnostics()},
                  f, ensure_ascii=True, indent=2)
    return path


# ── key schedule ────────────────────────────────────────────────────────────


def build_schedule(text, rng, gap, hold, pause_chars, pause_s, tight_chars):
    """Return [(t_ms, code, value)] sorted by time, plus per-key info."""
    events, keys = [], []
    t = 0.0
    held_until = {}  # code -> index into events of its pending release
    mod_release = None  # index of the pending Shift/Ctrl release
    prev = None
    for ch in text:
        if ch == "\x03":
            t += IDLE_PAUSE_MS
            continue
        if ch == "\n":
            code, mod = e.KEY_ENTER, None
        elif ch == "\x04":
            code, mod = e.KEY_BACKSPACE, e.KEY_LEFTCTRL
        elif ch in SPECIAL:
            code, mod = SPECIAL[ch], None
        elif ch in PLAIN:
            code, mod = PLAIN[ch], None
        elif ch in SHIFTED:
            code, mod = SHIFTED[ch], e.KEY_LEFTSHIFT
        else:
            raise SystemExit(f"run.py: cannot type {ch!r}")
        if prev is not None:
            if prev in tight_chars:
                t += rng.uniform(5, 20)
            else:
                t += rng.uniform(*gap)
        if ch in pause_chars and pause_s:
            t += pause_s * 1000
        h = rng.uniform(*hold)
        # Shift/Ctrl belongs to one character only: lift it before the next
        # press, otherwise the next key would be shifted (` → ~) or become a
        # Ctrl shortcut. For Ctrl+Backspace, also lift Backspace before Ctrl.
        mod_up = None
        if mod_release is not None:
            prev_press = events[mod_release - 1][0]
            prev_mod = events[mod_release][1]
            if prev_mod == e.KEY_LEFTCTRL and events[mod_release + 1][0] >= t - 3.0:
                bs_up = max(prev_press + 0.5, t - 3.0)
                events[mod_release + 1] = (bs_up, events[mod_release + 1][1], 0)
            if events[mod_release][0] >= t - 2.0:
                up = max(prev_press + 1.0, t - 2.0)
                events[mod_release] = (up, prev_mod, 0)
            mod_up = events[mod_release][0]
        mod_release = None
        # Before pressing Ctrl+Backspace, release any still-held keys so they
        # are not released while Ctrl is down.
        if mod == e.KEY_LEFTCTRL:
            ctrl_down = min(t - 8.0 if mod_up is None else max(t - 8.0, mod_up + 0.3), t - 0.5)
            for held_code, (i, press_t) in list(held_until.items()):
                if events[i][0] >= ctrl_down:
                    events[i] = (max(press_t + 0.5, ctrl_down - 0.5), held_code, 0)
                    held_until.pop(held_code, None)
        # A key cannot be pressed again while it is still down: release the
        # earlier press just before.
        if code in held_until:
            i, _ = held_until.pop(code)
            if events[i][0] >= t:
                events[i] = (t - 1.0, code, 0)
        if mod is not None:
            down = t - 8.0 if mod_up is None else max(t - 8.0, mod_up + 0.3)
            events.append((min(down, t - 0.2), mod, 1))
        events.append((t, code, 1))
        if mod is not None:
            events.append((t + h + 2.0, mod, 0))
            mod_release = len(events) - 1
        events.append((t + h, code, 0))
        held_until[code] = (len(events) - 1, t)
        keys.append((ch, round(t), round(h)))
        prev = ch
    # Stable sort: at equal times keep insertion order (press before release).
    events.sort(key=lambda ev: ev[0])
    return events, keys


def play(ui, events):
    start = time.perf_counter()
    for t_ms, code, value in events:
        delay = start + t_ms / 1000 - time.perf_counter()
        if delay > 0:
            time.sleep(delay)
        ui.write(e.EV_KEY, code, value)
        ui.syn()


def fmt_schedule(keys):
    names = {v: k for k, v in SPECIAL_NAMES.items()}
    return " ".join(f"{names.get(ch, repr(ch))}@{t}+{h}" for ch, t, h in keys)


# ── targets ─────────────────────────────────────────────────────────────────


FIREFOX_PREFS = """
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("trailhead.firstrun.didSeeAboutWelcome", true);
user_pref("browser.tabs.warnOnClose", false);
// Terms of Use / privacy onboarding modal (Firefox 136+). It takes focus and
// swallows the typed keys. `browser.preonboarding.enabled=false` is the
// opt-out Firefox's own automation uses; the others are belt and braces.
user_pref("browser.preonboarding.enabled", false);
user_pref("termsofuse.bypassNotification", true);
user_pref("termsofuse.acceptedVersion", 999);
// A date before the last policy update (Dec 2025) brings back a
// "We've updated our Privacy Notice" bar, so use a far-future one.
user_pref("termsofuse.acceptedDate", "4102444800000");
user_pref("termsofuse.firstAcceptedDate", "4102444800000");
user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);
user_pref("browser.startup.upgradeDialog.enabled", false);
user_pref("browser.shell.skipDefaultBrowserCheckOnFirstRun", true);
user_pref("startup.homepage_welcome_url", "about:blank");
user_pref("app.update.disabledForTesting", true);
user_pref("browser.translations.automaticallyPopup", false);
"""


TARGETS = ["ghostty", "foot", "kitty", "gtk", "gtk3", "firefox", "chromium"]


def add_app_options(ap):
    """Options for the app under test shared by run.py and poem.py."""
    ap.add_argument("--busy", action="store_true",
                    help="gtk/gtk3: block the entry's main loop 40 ms of every 50 ms")
    ap.add_argument("--native-trace", metavar="DIR",
                    help="firefox: save its Wayland (WAYLAND_DEBUG=client) and IME "
                         "(MOZ_LOG=IMEHandler:5) logs in DIR")


def check_app_options(ap, args):
    if args.busy and args.target not in ("gtk", "gtk3"):
        ap.error("--busy needs -t gtk or -t gtk3")
    if args.native_trace and args.target != "firefox":
        ap.error("--native-trace needs -t firefox")


def firefox_trace_env(trace_dir):
    """Environment and stderr file for a Firefox with native input logs."""
    os.makedirs(trace_dir, exist_ok=True)
    env = dict(os.environ, WAYLAND_DEBUG="client", MOZ_LOG="IMEHandler:5,sync",
               MOZ_LOG_FILE=os.path.join(trace_dir, "ime.log"))
    # WAYLAND_DEBUG goes to stderr; Firefox appends .moz_log (and a child
    # suffix per content process) to MOZ_LOG_FILE.
    return env, open(os.path.join(trace_dir, "wayland.log"), "w")


def launch(target, title, out, tmpdir, field, browser_bin=None,
           browser_session=None, browser_trace=False, busy=False, native_trace=None):
    if target in ("firefox", "chromium"):
        url, _server = browser_page.serve(title, out, field, browser_trace, browser_session)
        profile = os.path.join(tmpdir, "profile")
        os.makedirs(profile)
        if target == "firefox":
            with open(os.path.join(profile, "user.js"), "w") as f:
                f.write(FIREFOX_PREFS)
            argv = [browser_bin or "firefox", "--new-instance", "--profile", profile,
                    "--new-window", url]
            if native_trace:
                env, stderr = firefox_trace_env(native_trace)
                return subprocess.Popen(argv, env=env, stdout=subprocess.DEVNULL,
                                        stderr=stderr, start_new_session=True)
        else:
            argv = [browser_bin or "chromium", f"--user-data-dir={profile}", "--no-first-run",
                    "--no-default-browser-check", "--ozone-platform=wayland",
                    "--enable-wayland-ime", "--wayland-text-input-version=3", f"--app={url}"]
        return subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                start_new_session=True)
    # Keep echo on: a terminal that never redraws sends no text-input updates,
    # which hides daklak's dead-surrounding-text downgrade on KWin.
    # Bind werase to ^H so Ctrl+Backspace deletes the previous word like GUI apps.
    sink = f"stty werase '^H' iutf8; exec cat > '{out}'"
    if target == "ghostty":
        argv = ["ghostty", "--gtk-single-instance=false", f"--title={title}", "-e", "sh", "-c", sink]
    elif target == "foot":
        argv = ["foot", "--title", title, "sh", "-c", sink]
    elif target == "kitty":
        argv = ["kitty", "--title", title, "sh", "-c", sink]
    elif target in ("gtk", "gtk3"):
        argv = [sys.executable, os.path.join(HERE, "gtk_entry.py")]
        argv += ["--gtk3"] if target == "gtk3" else []
        argv += ["--busy"] if busy else []
        argv += [title, out]
    else:
        raise SystemExit(f"unknown target {target}")
    return subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                            start_new_session=True)


def read_lines(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        data = f.read()
    return data.split("\n")[:-1]  # only complete lines


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("text", help="Telex input typed per iteration (Enter is appended); "
                    "{L} {R} {BS} {CBS} type Left, Right, Backspace, Ctrl+Backspace; "
                    "{P} pauses 2.3 s")
    ap.add_argument("-e", "--expect", help="expected line; without it only a histogram is shown")
    ap.add_argument("-t", "--target", default="ghostty", choices=TARGETS)
    ap.add_argument("--field", default="input", choices=browser_page.FIELDS,
                    help="browser targets: text field (see browser_page.py)")
    ap.add_argument("--browser-bin", metavar="PATH",
                    help="browser targets: binary to run instead of firefox / chromium "
                         "(e.g. Firefox Developer Edition)")
    ap.add_argument("--browser-trace", action="store_true",
                    help="browser targets: save bounded event/model traces for failing lines")
    ap.add_argument("--browser-timeout", type=float, default=10,
                    help="browser readiness/report timeout in seconds (default: 10)")
    add_app_options(ap)
    ap.add_argument("-n", "--iterations", type=int, default=20)
    ap.add_argument("-p", "--profile", default="rollover", choices=sorted(PROFILES))
    ap.add_argument("--gap", type=range_arg, help="override gap MIN:MAX ms between presses")
    ap.add_argument("--hold", type=range_arg, help="override hold MIN:MAX ms")
    ap.add_argument("--seed", type=int, default=None, help="RNG seed (printed if random)")
    ap.add_argument("--replay", metavar="SEED:ITER", help="rerun one recorded schedule -n times")
    ap.add_argument("--settle", type=float, default=0.5, help="seconds to wait after each line")
    ap.add_argument("--no-refocus", action="store_true",
                    help="ghostty: skip the focus bounce (tests the synthetic session)")
    ap.add_argument("--daklak", metavar="BIN",
                    help="sway: start this daklak build for the run")
    ap.add_argument("--log", default="/tmp/daklak-race-e2e.log", help="log path with --daklak")
    ap.add_argument("--restart-im", action="store_true",
                    help="KDE: kill daklak so KWin respawns it (picks up a new build)")
    args = ap.parse_args()
    is_browser = args.target in ("firefox", "chromium")
    if (args.browser_bin or args.browser_trace) and not is_browser:
        ap.error("--browser-bin / --browser-trace need -t firefox or -t chromium")
    if args.browser_timeout <= 0:
        ap.error("--browser-timeout must be positive")
    check_app_options(ap, args)

    gap, hold, pause_chars, pause_s, tight = PROFILES[args.profile]
    gap = args.gap or gap
    hold = args.hold or hold
    seed = args.seed if args.seed is not None else random.randrange(1 << 31)
    replay = None
    if args.replay:
        s, _, it = args.replay.partition(":")
        seed, replay = int(s), int(it)
    text = expand_specials(args.text.rstrip("\n")) + "\n"

    comp = wm.detect()
    if comp is None:
        print("run.py: no sway/scroll/KWin control; assuming the new window gets focus",
              file=sys.stderr)
    kwin = comp is not None and comp.name == "kwin"

    daklak = None
    if args.daklak:
        if kwin:
            raise SystemExit("run.py: on KDE, KWin launches daklak; rebuild and use --restart-im")
        if daklak_pid():
            raise SystemExit("run.py: a daklak is already running; stop it or drop --daklak")
        env = dict(os.environ,
                   DAKLAK_ENABLE_WAYLAND="1", DAKLAK_ENABLE_EVDEV_GRAB="0",
                   DAKLAK_LOG_PATH=args.log, DAKLAK_LOG_LEVEL="error",
                   DAKLAK_LOG_MODULES="daklak=debug,viet_ime_wayland_adapter=debug,"
                                      "viet_ime_key_emitter=debug")
        open(args.log, "w").close()
        daklak = subprocess.Popen([args.daklak], env=env, stdout=subprocess.DEVNULL,
                                  stderr=open(args.log, "a"))
        time.sleep(1.0)
        if daklak.poll() is not None:
            raise SystemExit(f"run.py: daklak exited early; see {args.log}")
        pid, log = daklak.pid, args.log
    else:
        pid = daklak_pid()
        if not pid:
            raise SystemExit("run.py: daklak is not running"
                             + ("" if kwin else " (start it or pass --daklak BIN)"))
        if args.restart_im:
            if not kwin:
                raise SystemExit("run.py: --restart-im needs KWin (use --daklak elsewhere)")
            pid = restart_kwin_im(pid)
        log = daklak_log_path(pid)

    caps = set(PLAIN.values()) | set(SPECIAL.values()) | {
        e.KEY_LEFTSHIFT, e.KEY_LEFTCTRL, e.KEY_ENTER,
    }
    ui = UInput({e.EV_KEY: sorted(caps)}, name="daklak-race-e2e-kbd")

    title = f"daklak-race-e2e-{os.getpid()}"
    tmpdir = tempfile.mkdtemp(prefix="daklak-race-e2e-")
    out = os.path.join(tmpdir, "out.txt")
    open(out, "w").close()
    prev = comp.save_focus() if comp else None
    browser_session = browser_page.BrowserSession() if is_browser else None
    browser_context = dict(target=args.target, field=args.field,
                           browser=args.browser_bin or args.target, seed=seed, expected=args.expect)
    proc = None

    def save_browser_trace(suffix, report=None):
        path = f"/tmp/daklak-race-e2e-{os.getpid()}-{args.target}-{suffix}.browser.json"
        return write_browser_trace(path, browser_session, browser_context, report)

    def cleanup():
        ui.close()
        if proc is not None:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                proc.wait()
        if browser_session:
            browser_session.close()
        if comp:
            comp.restore_focus(prev)
        if daklak:
            daklak.terminate()
        time.sleep(0.5)
        shutil.rmtree(tmpdir, ignore_errors=True)

    try:
        proc = launch(args.target, title, out, tmpdir, args.field, args.browser_bin,
                      browser_session, args.browser_trace, args.busy, args.native_trace)
        if comp:
            if not comp.wait_for(title):
                raise browser_page.HarnessError("target window never appeared")
            time.sleep(1.0)
            if args.target == "ghostty" and not args.no_refocus:
                # Ghostty enables text-input only after its first focus change.
                if comp.focus_other(title):
                    time.sleep(0.6)
            comp.focus_title(title)
        time.sleep(1.5)  # new uinput device + focus settle
        if browser_session:
            browser_session.wait_ready(args.browser_timeout, proc)

        browser = ""
        if args.target in ("firefox", "chromium"):
            browser = f" field={args.field} bin={args.browser_bin or args.target}"
        print(f"target={args.target}{browser} profile={args.profile} gap={gap} hold={hold} "
              f"seed={seed} text={text!r} expect={args.expect!r}")
        print(f"wm={comp.name if comp else None} daklak pid={pid} log={log}")
        results = collections.Counter()
        failures = []
        aborted = False
        for n in range(args.iterations):
            browser_context.update(iteration=n)
            if browser_session:
                browser_session.ensure_healthy()
                browser_session.wait_ready(args.browser_timeout, proc)
            it = replay if replay is not None else n
            if comp and title not in (comp.focused_title() or ""):
                print(f"run.py: target lost focus before iteration {n}; stopping",
                      file=sys.stderr)
                aborted = True
                break
            if not os.path.exists(f"/proc/{pid}") or (daklak and daklak.poll() is not None):
                print(f"run.py: daklak (pid {pid}) died before iteration {n}; see {log}",
                      file=sys.stderr)
                aborted = True
                break
            log_start = log_size(log) if log else 0
            rng = random.Random(f"{seed}:{it}")
            events, keys = build_schedule(text, rng, gap, hold, pause_chars, pause_s, tight)
            started = utc_now()
            browser_context.update(scheduleIteration=it, started=started, keys=keys)
            play(ui, events)
            time.sleep(args.settle)
            report = None
            if browser_session:
                report = browser_session.wait_report(n, args.browser_timeout, proc)
                got = browser_page.format_line(report)
            else:
                lines = read_lines(out)
                got = lines[n] if n < len(lines) else None
            results[got] += 1
            agrees = report is None or report["model"] == report["dom"]
            ok = got is not None and agrees and (args.expect is None or got == args.expect)
            mark = "ok  " if ok else "FAIL"
            print(f"[{n:3}] {mark} {started} {got!r}")
            if not ok:
                slice_path = None
                if log:
                    slice_path = f"/tmp/daklak-race-e2e-{os.getpid()}-{args.target}-{n}.log"
                    if not save_log_slice(log, log_start, slice_path):
                        slice_path = None
                trace_path = save_browser_trace(n, report) if args.browser_trace else None
                failures.append((n, it, started, got, keys, slice_path, trace_path))

        if browser_session and not aborted:
            browser_session.verify_complete(args.iterations)
        print("\noutcomes:")
        for got, count in results.most_common():
            print(f"  {count:4} × {got!r}")
        for n, it, started, got, keys, slice_path, trace_path in failures:
            print(f"\n[{n}] {started} got {got!r}  replay: --replay {seed}:{it}")
            print(f"  keys (char@press_ms+hold_ms): {fmt_schedule(keys)}")
            if slice_path:
                print(f"  daklak log for this line: {slice_path}")
            if trace_path:
                print(f"  browser trace for this line: {trace_path}")
        return 2 if aborted else (1 if failures else 0)
    except (browser_page.HarnessError, OSError) as error:
        print(f"run.py: harness failure: {error}", file=sys.stderr)
        if args.browser_trace:
            print(f"  browser diagnostics: {save_browser_trace('harness')}", file=sys.stderr)
        return 2
    finally:
        cleanup()


if __name__ == "__main__":
    try:
        code = main()
    except SystemExit as error:
        if not isinstance(error.code, str):
            raise
        print(error.code, file=sys.stderr)
        code = 2  # preflight/setup failure, not a text mismatch
    sys.exit(code)
