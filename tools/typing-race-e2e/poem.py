#!/usr/bin/env python3
"""End-to-end poem typing test with realistic Vietnamese mistake corrections.

Types the 26-line poem "Đánh cho Mỹ cút Ngụy nhào" into a live target window
through a temporary uinput keyboard, injecting human-like typing mistakes and
corrections at a configurable per-word rate (default 0.12 ≈ 12% of words):

  - tone_override: wrong tone mark first, then the right tone key to replace it
                   in place (e.g. `Ddanhfs` → `Đánh`, `nhaosf` → `nhào`)
  - diacritic_bs:  missed vowel modifier (`aa`/`ee`/`oo`/`aw`/`ow`/`uw`) noticed
                   after the next consonant, fixed with `{BS}` and retyped
                   (e.g. `chet{BS}{BS}ets` → `chết`, `nuoc{BS}{BS}owcs` → `nước`)
  - char_bs:       wrong consonant or spillover chars after tone (including
                   non-Vietnamese auto-restore), deleted with `{BS}` and fixed
                   (e.g. `haanjph{BS}{BS}{BS}j` → `hận`, `khoau{BS}i` → `khoai`)
  - ctrl_bs:       mistyped whole word deleted with `{CBS}` (Ctrl+Backspace)
                   and retyped cleanly (e.g. `nhaox{CBS}nhaof` → `nhào`)
  - cursor_edit:   mid-word typo fixed with `{L}`, `{BS}`, key, `{R}`

Every submitted line is checked against the expected Vietnamese text.
"""
import argparse
import os
import random
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unicodedata

import browser_page
import run as harness
import wm
from evdev import UInput, ecodes as e

POEM_LINES = [
    "Đánh cho Mỹ cút Ngụy nhào,",
    "Thơ Bùi Giáng",
    "Đánh cho Mỹ cút Ngụy nhào,",
    "Đánh cho chết mẹ đồng bào miền Nam.",
    "Đánh cho khoai sắn thành vàng,",
    "Đánh cho dép lốp phải mang thế giầy.",
    "Đánh cho Bắc đoạ Nam đày,",
    "Đánh cho thù hận giờ này chưa tan.",
    "Đánh cho cả nước Việt Nam,",
    "Áo ôm khố rách xếp hàng xin cho.",
    "Đánh cho hết muốn tự do,",
    "Hết mơ dân chủ hết lo quyền người.",
    "Đánh cho dở khóc dở cười,",
    "Hai miền thống nhất kiếp người ngựa trâu.",
    "Đánh cho hai nước Việt Tàu,",
    "Không còn biên giới cùng nhau đại đồng.",
    "Đánh cho dòng giống Tiên rồng,",
    "Osin, nô lệ, lao công xứ người.",
    "Đánh cho chín chục triệu người,",
    "Thành dân vô sản thành người lưu vong.",
    "Đánh cho non nước Lạc Hồng,",
    "Tiến lên thời đại mang gông mang cùm.",
    "Đánh cho cả nước chết chùm,",
    "Đánh cho con cháu khốn cùng mai sau.",
    "Đánh cho Bác Đảng Nga Tàu,",
    "Triệu dân nô lệ ngàn năm căm hờn !",
]

# Explicit Telex spellings (tone key at end of word) for every distinct word in
# the poem, lowercase. Title-case words capitalize the first keystroke.
WORD_TELEX = {
    "áo": "aos",
    "bác": "bacs",
    "bào": "baof",
    "bắc": "bawcs",
    "biên": "bieen",
    "bùi": "buif",
    "cả": "car",
    "căm": "cawm",
    "cháu": "chaus",
    "chết": "cheets",
    "chín": "chins",
    "cho": "cho",
    "chủ": "chur",
    "chục": "chucj",
    "chùm": "chumf",
    "chưa": "chuwa",
    "con": "con",
    "còn": "conf",
    "công": "coong",
    "cùm": "cumf",
    "cùng": "cungf",
    "cười": "cuwowif",
    "cút": "cuts",
    "dân": "daan",
    "dép": "deps",
    "do": "do",
    "dòng": "dongf",
    "dở": "dowr",
    "đại": "ddaij",
    "đảng": "ddangr",
    "đánh": "ddanhs",
    "đày": "ddayf",
    "đoạ": "ddoaj",
    "đọa": "ddoaj",
    "đồng": "ddoongf",
    "giáng": "giangs",
    "giầy": "giaayf",
    "giống": "gioongs",
    "giờ": "giowf",
    "giới": "giowis",
    "gông": "goong",
    "hai": "hai",
    "hàng": "hangf",
    "hận": "haanj",
    "hết": "heets",
    "hồng": "hoongf",
    "hờn": "hownf",
    "khóc": "khocs",
    "khoai": "khoai",
    "khố": "khoos",
    "khốn": "khoons",
    "không": "khoong",
    "kiếp": "kieeps",
    "lạc": "lacj",
    "lao": "lao",
    "lệ": "leej",
    "lên": "leen",
    "lo": "lo",
    "lốp": "loops",
    "lưu": "luwu",
    "mai": "mai",
    "mang": "mang",
    "mẹ": "mej",
    "miền": "mieenf",
    "mơ": "mow",
    "muốn": "muoons",
    "mỹ": "myx",
    "nam": "nam",
    "này": "nayf",
    "năm": "nawm",
    "nga": "nga",
    "ngàn": "nganf",
    "ngựa": "nguwaj",
    "người": "nguwowif",
    "ngụy": "nguyj",
    "nguỵ": "nguyj",
    "nhào": "nhaof",
    "nhau": "nhau",
    "nhất": "nhaats",
    "nô": "noo",
    "non": "non",
    "nước": "nuwowcs",
    "ôm": "oom",
    "phải": "phair",
    "quyền": "quyeenf",
    "rách": "rachs",
    "rồng": "roongf",
    "sản": "sanr",
    "sau": "sau",
    "sắn": "sawns",
    "tan": "tan",
    "tàu": "tauf",
    "thành": "thanhf",
    "thế": "thees",
    "thơ": "thow",
    "thời": "thowif",
    "thống": "thoongs",
    "thù": "thuf",
    "tiên": "tieen",
    "tiến": "tieens",
    "trâu": "traau",
    "triệu": "trieeuj",
    "tự": "tuwj",
    "vàng": "vangf",
    "việt": "vieetj",
    "vô": "voo",
    "vong": "vong",
    "xếp": "xeeps",
    "xin": "xin",
    "xứ": "xuws",
}

WRONG_TONES = {
    "s": ["f", "r", "j"],
    "f": ["s", "r", "x"],
    "r": ["f", "x", "s"],
    "x": ["r", "f", "j"],
    "j": ["f", "s", "x"],
}

# A key next to each letter on a QWERTY keyboard, for one-letter typos.
NEIGHBOR_KEYS = {
    "a": "s", "b": "v", "c": "x", "d": "s", "e": "r", "g": "h",
    "h": "g", "i": "u", "k": "l", "l": "k", "m": "n", "n": "m",
    "o": "i", "p": "o", "r": "t", "s": "d", "t": "r", "u": "i",
    "v": "b", "x": "c", "y": "u",
}


def detect_modern_style():
    """Match daklak's Config::load() modern_style setting."""
    env = os.environ.get("DAKLAK_MODERN_STYLE")
    if env is not None:
        return env.strip().lower() in ("1", "true", "yes", "on")
    cfg = os.path.join(
        os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config")),
        "daklak",
        "config.toml",
    )
    try:
        with open(cfg, encoding="utf-8") as f:
            for raw in f:
                line = raw.split("#", 1)[0].strip()
                m = re.match(r"^modern_style\s*=\s*(true|false)\b", line)
                if m:
                    return m.group(1) == "true"
    except OSError:
        pass
    return True


def expected_poem_line(line, modern_style):
    if modern_style:
        return line.replace("Ngụy", "Nguỵ").replace("đọa", "đoạ")
    return line.replace("Nguỵ", "Ngụy").replace("đoạ", "đọa")


def clean_telex_for_word(word):
    lower = word.lower()
    # `Osin` composes `Ói` and is restored to raw on `n`, so the poem also
    # exercises a capitalized non-Vietnamese restore.
    base = "osin" if lower == "osin" else WORD_TELEX[lower]
    if word[0].isupper():
        return base[0].upper() + base[1:]
    return base


# ── mistake + correction strategies ─────────────────────────────────────────


def mut_tone_override(word, clean, rng):
    """Type a wrong tone key first, then the right tone key to update it."""
    if len(clean) < 2 or clean[-1] not in WRONG_TONES:
        return None
    # Stop-consonant endings (c, ch, p, t) in vnkey only accept s (Tone1) and
    # j (Tone5); other tone keys are appended as raw letters.
    stem = clean[:-1].lower()
    if stem.endswith(("c", "ch", "p", "t")):
        wrong = "j" if clean[-1] == "s" else "s"
    else:
        wrong = rng.choice(WRONG_TONES[clean[-1]])
    seq = clean[:-1] + wrong + clean[-1]
    return seq, f"tone_override({word}: {wrong}→{clean[-1]})"


def mut_diacritic_bs(word, clean, rng):
    """Forget a vowel modifier (`aa`/`ee`/`oo`/`aw`/`ow`/`uw`), then Backspace
    back and retype with the modifier."""
    lower = clean.lower()
    for mod in ("uwow", "aa", "ee", "oo", "aw", "ow", "uw"):
        idx = lower.find(mod)
        # A word may start with the modified vowel (`ước`, `ấy`); only a
        # missing match skips this modifier.
        if idx < 0:
            continue
        after_idx = idx + len(mod)
        tail = clean[after_idx:]
        # Need a following consonant/semivowel (not a tone key) so typing it
        # without the vowel modifier appends a plain char that {BS} deletes.
        if not tail or tail[0].lower() in "sfrxjw":
            continue
        if mod == "uwow":
            prefix = clean[:idx] + clean[idx] + clean[idx + 2] + tail[0]
            fix = "{BS}{BS}wow" + tail
        else:
            prefix = clean[: idx + 1] + tail[0]
            fix = "{BS}" + clean[idx + 1] + tail
        return prefix + fix, f"diacritic_bs({word}: {prefix}{fix})"
    return None


def mut_char_bs(word, clean, rng):
    """Either spillover chars after tone (triggering auto-restore when invalid)
    deleted with Backspace + re-toned, or a wrong letter deleted with Backspace."""
    lower = clean.lower()
    # Case 1: a toned word ending in a nasal (`hanhj`, `ddanhs`, `thoongs`)
    # where `p`/`ph` makes it non-Vietnamese, so daklak restores it to raw
    # letters (`hanhjp`); {BS} deletes the spillover and the tone key, and
    # the tone key is typed again (`hạnh`).
    if (
        len(clean) >= 4
        and clean[-1] in WRONG_TONES
        and lower[:-1].endswith(("nh", "ng", "n", "m"))
        and rng.random() < 0.6
    ):
        spill = rng.choice(["p", "ph"])
        bs = "{BS}" * (len(spill) + 1)
        seq = clean + spill + bs + clean[-1]
        return seq, f"char_bs_spill({word}: +{spill}{bs}{clean[-1]})"
    # Case 2: hit a neighbouring key instead of the last letter before the
    # tone (`h` for `g`: `thoonh`, which may stop being Vietnamese and be
    # restored to raw letters), delete it with {BS} and finish the word.
    pos = len(clean) - 2 if clean[-1] in WRONG_TONES and len(clean) >= 3 else len(clean) - 1
    ch = clean[pos].lower()
    wrong = NEIGHBOR_KEYS.get(ch, "k")
    # Not a Telex modifier/tone key (`s f r x j w`), not a doubled letter,
    # and not `a`/`e`/`o`/`d` when the word already has that letter: Telex
    # reads it as a modifier on it (`lơ` + `o` is `lô`), so one Backspace
    # would not undo it.
    if (
        wrong in "sfrxjw"
        or (pos > 0 and wrong == clean[pos - 1].lower())
        or (wrong in "aeod" and wrong in lower[:pos])
    ):
        wrong = "k"
    seq = clean[:pos] + wrong + "{BS}" + clean[pos:]
    return seq, f"char_bs({word}: {wrong}{{BS}}{clean[pos:]})"


def mut_ctrl_bs(word, clean, rng):
    """Type a mistyped word, delete the whole word with Ctrl+Backspace ({CBS}),
    and retype the clean word."""
    if clean.lower() == "osin" or len(clean) < 3:
        return None
    if clean[-1] in WRONG_TONES:
        stem = clean[:-1].lower()
        if stem.endswith(("c", "ch", "p", "t")):
            wrong_tone = "j" if clean[-1] == "s" else "s"
        else:
            wrong_tone = rng.choice(WRONG_TONES[clean[-1]])
        typo = clean[:-1] + wrong_tone
    else:
        typo = clean + rng.choice(["f", "s"])
    seq = f"{typo}{{CBS}}{clean}"
    return seq, f"ctrl_bs({word}: {typo}{{CBS}}{clean})"


def mut_cursor_edit(word, clean, rng):
    """Insert a stray consonant inside a plain-vowel word, move Left, Backspace
    it, move Right, and finish the word (e.g. `thanhf` -> `thabnh{L}{L}{BS}{R}{R}f`)."""
    lower = clean.lower()
    if lower == "osin" or len(clean) < 4:
        return None
    has_tone = clean[-1] in WRONG_TONES
    body = clean[:-1] if has_tone else clean
    tone = clean[-1] if has_tone else ""
    body_lower = body.lower()
    # Avoid words with composed letters (`dd`/`aa`/`ee`/`oo`/`w`) where an
    # invalid consonant cluster would trigger vnkey's non-VN auto-restore before {L}.
    if any(m in body_lower for m in ("dd", "aa", "ee", "oo", "w")):
        return None
    if body_lower.endswith(("nh", "ng", "ch")) and len(body) >= 4:
        seq = f"{body[:-2]}b{body[-2:]}{{L}}{{L}}{{BS}}{{R}}{{R}}{tone}"
        return seq, f"cursor_edit({word}: {seq})"
    if body_lower[-1] in "nmcptiauy" and len(body) >= 3:
        seq = f"{body[:-1]}b{body[-1]}{{L}}{{BS}}{{R}}{tone}"
        return seq, f"cursor_edit({word}: {seq})"
    return None


STRATEGIES = [
    ("tone_override", 0.30, mut_tone_override),
    ("diacritic_bs", 0.25, mut_diacritic_bs),
    ("char_bs", 0.20, mut_char_bs),
    ("ctrl_bs", 0.15, mut_ctrl_bs),
    ("cursor_edit", 0.10, mut_cursor_edit),
]


def tokenize_line(line):
    """Split a line into alternating (is_word, token) parts."""
    parts = []
    for m in re.finditer(r"([^\W\d_]+)|(\W+|\d+)", line, flags=re.UNICODE):
        if m.group(1) is not None:
            parts.append((True, m.group(1)))
        else:
            parts.append((False, m.group(2)))
    return parts


def build_poem_plan(lines, rate, rng, modern_style, allow_ctrl_bs=True, allow_cursor_edit=True):
    """Return [(line_no, expected_line, telex_input, [correction_descriptions])]."""
    active_strategies = [
        (name, weight, fn)
        for name, weight, fn in STRATEGIES
        if (allow_ctrl_bs or name != "ctrl_bs")
        and (allow_cursor_edit or name != "cursor_edit")
    ]
    plan = []
    used_kinds = set()

    for line_no, raw_line in enumerate(lines, 1):
        expected = expected_poem_line(raw_line, modern_style)
        parts = tokenize_line(expected)
        typed_parts = []
        line_corrections = []

        for idx, (is_word, tok) in enumerate(parts):
            if not is_word:
                typed_parts.append(tok)
                continue
            clean = clean_telex_for_word(tok)
            # Only use {CBS} when the word is preceded by whitespace (or start
            # of line) so Ctrl+Backspace has a clean word boundary on every app.
            prev_tok = parts[idx - 1][1] if idx > 0 else " "
            can_ctrl_bs = allow_ctrl_bs and prev_tok.endswith(" ")

            if rate > 0 and rng.random() < rate:
                candidates = list(active_strategies)
                # Prefer any strategy kind not yet used in this poem pass.
                unseen = [c for c in candidates if c[0] not in used_kinds]
                seen = [c for c in candidates if c[0] in used_kinds]
                rng.shuffle(unseen)
                rng.shuffle(seen)
                order = unseen + seen
                applied = None
                for name, _w, fn in order:
                    if name == "ctrl_bs" and not can_ctrl_bs:
                        continue
                    res = fn(tok, clean, rng)
                    if res is not None:
                        applied = (name, res[0], res[1])
                        break
                if applied is not None:
                    kind, seq, desc = applied
                    used_kinds.add(kind)
                    typed_parts.append(seq)
                    line_corrections.append(desc)
                    continue

            typed_parts.append(clean)

        plan.append((line_no, expected, "".join(typed_parts), line_corrections))

    return plan


def replay_command(args, pass_no, line_no):
    """Command line that types `line_no` of pass `pass_no` with this run's setup."""
    parts = ["tools/typing-race-e2e/poem.py", "-t", args.target]
    if args.target in ("firefox", "chromium"):
        parts += ["--field", args.field]
        if args.browser_bin:
            parts += ["--browser-bin", args.browser_bin]
    if args.busy:
        parts.append("--busy")
    parts += ["-p", args.profile]
    if args.gap:
        parts += ["--gap", f"{args.gap[0]}:{args.gap[1]}"]
    if args.hold:
        parts += ["--hold", f"{args.hold[0]}:{args.hold[1]}"]
    parts += ["--seed", str(args.seed), "--rate", str(args.rate)]
    if args.no_ctrl_bs:
        parts.append("--no-ctrl-bs")
    if args.modern_style != "auto":
        parts += ["--modern-style", args.modern_style]
    parts += ["--pass", str(pass_no), "--line", str(line_no)]
    return " ".join(parts)


def parse_line_range(spec, total):
    if not spec:
        return list(range(1, total + 1))
    if ":" in spec:
        lo, _, hi = spec.partition(":")
        a = int(lo) if lo else 1
        b = int(hi) if hi else total
        return [i for i in range(a, b + 1) if 1 <= i <= total]
    n = int(spec)
    return [n] if 1 <= n <= total else []


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("-t", "--target", default="firefox", choices=harness.TARGETS)
    ap.add_argument("--field", default="textarea", choices=browser_page.FIELDS,
                    help="browser targets: text field (default: textarea)")
    ap.add_argument("--browser-bin", metavar="PATH",
                    help="browser binary (e.g. ~/.local/share/firefox-de/firefox)")
    ap.add_argument("--browser-trace", action="store_true",
                    help="save .browser.json traces for failing lines")
    ap.add_argument("--browser-timeout", type=float, default=15,
                    help="browser readiness/report timeout in seconds (default: 15)")
    harness.add_app_options(ap)
    ap.add_argument("-p", "--profile", default="rollover", choices=sorted(harness.PROFILES))
    ap.add_argument("--gap", type=harness.range_arg, help="override gap MIN:MAX ms")
    ap.add_argument("--hold", type=harness.range_arg, help="override hold MIN:MAX ms")
    ap.add_argument("--rate", type=float, default=0.12,
                    help="per-word mistake/correction rate (default: 0.12; 0 for clean typing)")
    ap.add_argument("--no-ctrl-bs", action="store_true",
                    help="disable whole-word Ctrl+Backspace corrections")
    ap.add_argument("--passes", type=int, default=1,
                    help="number of full poem passes (default: 1)")
    ap.add_argument("--pass", dest="only_pass", type=int, metavar="N",
                    help="only type 0-based pass N (each pass has its own mistakes)")
    ap.add_argument("--line", metavar="N[:M]",
                    help="only type 1-based poem line N or range N:M")
    ap.add_argument("--seed", type=int, default=42, help="RNG seed (default: 42)")
    ap.add_argument("--settle", type=float, default=0.5,
                    help="seconds to wait after each line (default: 0.5)")
    ap.add_argument("--modern-style", choices=["auto", "true", "false"], default="auto",
                    help="expected tone placement for oa/oe/uy (default: auto from config)")
    ap.add_argument("--no-refocus", action="store_true",
                    help="ghostty: skip the focus bounce")
    ap.add_argument("--daklak", metavar="BIN", help="sway: start this daklak build")
    ap.add_argument("--log", default="/tmp/daklak-poem-e2e.log", help="log path with --daklak")
    ap.add_argument("--restart-im", action="store_true",
                    help="KDE: respawn KWin's daklak before running")
    ap.add_argument("--dry-run", action="store_true",
                    help="print generated Telex + mistake corrections and exit")
    args = ap.parse_args()

    is_browser = args.target in ("firefox", "chromium")
    if (args.browser_bin or args.browser_trace) and not is_browser:
        ap.error("--browser-bin / --browser-trace need -t firefox or -t chromium")
    harness.check_app_options(ap, args)

    if args.modern_style == "auto":
        modern_style = detect_modern_style()
    else:
        modern_style = args.modern_style == "true"

    selected_indices = set(parse_line_range(args.line, len(POEM_LINES)))
    if not selected_indices:
        raise SystemExit(f"poem.py: --line {args.line!r} matched no lines (1..{len(POEM_LINES)})")

    gap, hold, pause_chars, pause_s, tight = harness.PROFILES[args.profile]
    gap = args.gap or gap
    hold = args.hold or hold

    is_terminal = args.target in ("ghostty", "foot", "kitty")
    all_steps = []
    passes = [args.only_pass] if args.only_pass is not None else range(args.passes)
    for p in passes:
        pass_rng = random.Random(f"poem-plan:{args.seed}:{p}")
        plan = build_poem_plan(
            POEM_LINES,
            args.rate,
            pass_rng,
            modern_style,
            allow_ctrl_bs=not args.no_ctrl_bs,
            allow_cursor_edit=not is_terminal,
        )
        for line_no, expected, telex_in, corrections in plan:
            if line_no in selected_indices:
                all_steps.append((p, line_no, expected, telex_in, corrections))

    total_corrections = sum(len(c) for _, _, _, _, c in all_steps)
    if args.dry_run:
        print(f"seed={args.seed} rate={args.rate} modern_style={modern_style} "
              f"lines={len(all_steps)} corrections={total_corrections}")
        for p, line_no, expected, telex_in, corrections in all_steps:
            corr_str = ", ".join(corrections) if corrections else "clean"
            print(f"[{p}:{line_no:02d}] {expected}")
            print(f"       telex: {telex_in}")
            print(f"       fixes: {corr_str}")
        return 0

    comp = wm.detect()
    if comp is None:
        print("poem.py: no sway/scroll/KWin control; assuming the new window gets focus",
              file=sys.stderr)
    kwin = comp is not None and comp.name == "kwin"

    daklak = None
    if args.daklak:
        if kwin:
            raise SystemExit("poem.py: on KDE, KWin launches daklak; rebuild and use --restart-im")
        if harness.daklak_pid():
            raise SystemExit("poem.py: a daklak is already running; stop it or drop --daklak")
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
            raise SystemExit(f"poem.py: daklak exited early; see {args.log}")
        pid, log = daklak.pid, args.log
    else:
        pid = harness.daklak_pid()
        if not pid:
            raise SystemExit("poem.py: daklak is not running")
        if args.restart_im:
            if not kwin:
                raise SystemExit("poem.py: --restart-im needs KWin")
            pid = harness.restart_kwin_im(pid)
        log = harness.daklak_log_path(pid)

    caps = set(harness.PLAIN.values()) | set(harness.SPECIAL.values()) | {
        e.KEY_LEFTSHIFT, e.KEY_LEFTCTRL, e.KEY_ENTER,
    }
    ui = UInput({e.EV_KEY: sorted(caps)}, name="daklak-poem-e2e-kbd")

    title = f"daklak-poem-e2e-{os.getpid()}"
    tmpdir = tempfile.mkdtemp(prefix="daklak-poem-e2e-")
    out = os.path.join(tmpdir, "out.txt")
    open(out, "w").close()
    prev = comp.save_focus() if comp else None
    browser_session = browser_page.BrowserSession() if is_browser else None
    browser_context = dict(target=args.target, field=args.field,
                           browser=args.browser_bin or args.target, seed=args.seed)
    proc = None

    def save_browser_trace(suffix, report=None):
        path = f"/tmp/daklak-poem-e2e-{os.getpid()}-{args.target}-{suffix}.browser.json"
        return harness.write_browser_trace(path, browser_session, browser_context, report)

    def cleanup():
        ui.close()
        if proc is not None:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except Exception:
                pass
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except Exception:
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
        proc = harness.launch(args.target, title, out, tmpdir, args.field, args.browser_bin,
                              browser_session, args.browser_trace, args.busy,
                              args.native_trace)
        if comp:
            if not comp.wait_for(title):
                raise browser_page.HarnessError("target window never appeared")
            time.sleep(1.0)
            if args.target == "ghostty" and not args.no_refocus:
                if comp.focus_other(title):
                    time.sleep(0.6)
            comp.focus_title(title)
        time.sleep(1.5)
        if browser_session:
            browser_session.wait_ready(args.browser_timeout, proc)

        browser = ""
        if is_browser:
            browser = f" field={args.field} bin={args.browser_bin or args.target}"
        print(f"target={args.target}{browser} profile={args.profile} gap={gap} hold={hold} "
              f"seed={args.seed} rate={args.rate} modern_style={modern_style} "
              f"lines={len(all_steps)} corrections={total_corrections}")
        print(f"wm={comp.name if comp else None} daklak pid={pid} log={log}")

        failures = []
        aborted = False
        p_of = [p for p, *_ in all_steps]
        for step_idx, (p, line_no, expected, telex_in, corrections) in enumerate(all_steps):
            browser_context.update(iteration=step_idx, line=line_no, expected=expected)
            if browser_session:
                browser_session.ensure_healthy()
                browser_session.wait_ready(args.browser_timeout, proc)
            if comp and title not in (comp.focused_title() or ""):
                print(f"poem.py: target lost focus before step {step_idx} (line {line_no}); stopping",
                      file=sys.stderr)
                aborted = True
                break
            if not os.path.exists(f"/proc/{pid}") or (daklak and daklak.poll() is not None):
                print(f"poem.py: daklak (pid {pid}) died before step {step_idx} (line {line_no}); see {log}",
                      file=sys.stderr)
                aborted = True
                break

            log_start = harness.log_size(log) if log else 0
            expanded = harness.expand_specials(telex_in) + "\n"
            sched_rng = random.Random(f"poem-sched:{args.seed}:{p}:{line_no}")
            events, keys = harness.build_schedule(
                expanded, sched_rng, gap, hold, pause_chars, pause_s, tight
            )
            started = harness.utc_now()
            browser_context.update(started=started, telex=telex_in, corrections=corrections, keys=keys)
            harness.play(ui, events)
            time.sleep(args.settle)

            report = None
            if browser_session:
                report = browser_session.wait_report(step_idx, args.browser_timeout, proc)
                got = browser_page.format_line(report)
                agrees = report["model"] == report["dom"]
                ok = agrees and unicodedata.normalize("NFC", got) == unicodedata.normalize("NFC", expected)
            else:
                lines = harness.read_lines(out)
                got = lines[step_idx] if step_idx < len(lines) else None
                if got is None:
                    print(f"poem.py: missing output for step {step_idx} (line {line_no}); stopping",
                          file=sys.stderr)
                    aborted = True
                    break
                ok = unicodedata.normalize("NFC", got) == unicodedata.normalize("NFC", expected)

            mark = "ok  " if ok else "FAIL"
            corr_tag = f"  [{'; '.join(corrections)}]" if corrections else ""
            print(f"[{step_idx:3d} L{line_no:02d}] {mark} {started} {got!r}{corr_tag}")
            if not ok:
                slice_path = None
                if log:
                    slice_path = f"/tmp/daklak-poem-e2e-{os.getpid()}-{args.target}-L{line_no}-{step_idx}.log"
                    if not harness.save_log_slice(log, log_start, slice_path):
                        slice_path = None
                trace_path = (
                    save_browser_trace(f"L{line_no}-{step_idx}", report)
                    if args.browser_trace and browser_session else None
                )
                failures.append((
                    step_idx, line_no, started, expected, got, telex_in,
                    corrections, keys, slice_path, trace_path,
                ))

        if browser_session and not aborted:
            browser_session.verify_complete(len(all_steps))

        print(f"\nsummary: {len(all_steps) - len(failures)}/{len(all_steps)} lines matched "
              f"({total_corrections} mistake corrections injected)")
        if failures:
            print(f"\n{len(failures)} mismatched line(s):")
            for (step_idx, line_no, started, expected, got, telex_in,
                 corrections, keys, slice_path, trace_path) in failures:
                print(f"\n[step {step_idx} line {line_no}] {started}")
                print(f"  expected:    {expected!r}")
                print(f"  got:         {got!r}")
                print(f"  telex input: {telex_in!r}")
                print(f"  corrections: {corrections}")
                print(f"  replay line: {replay_command(args, p_of[step_idx], line_no)}")
                print(f"  keys:        {harness.fmt_schedule(keys)}")
                if slice_path:
                    print(f"  daklak log:  {slice_path}")
                if trace_path:
                    print(f"  browser:     {trace_path}")
        return 2 if aborted else (1 if failures else 0)
    except (browser_page.HarnessError, OSError) as error:
        if args.browser_trace and browser_session:
            trace_path = save_browser_trace("harness")
            print(f"poem.py: browser trace: {trace_path}", file=sys.stderr)
        print(f"poem.py: harness failure: {error}", file=sys.stderr)
        return 2
    finally:
        cleanup()


if __name__ == "__main__":
    sys.exit(main())
