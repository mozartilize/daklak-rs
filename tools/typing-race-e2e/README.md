# Typing-race end-to-end check (sway / scroll / KDE)

`run.py` types the same Telex input many times into a throw-away window
through a temporary uinput keyboard, with seeded random key timing, and reports
every line that differs from the expected text. It targets timing-dependent
bugs that a single `tools/ghostty-e2e` run at a steady pace does not hit, for
example a tone key followed quickly by a space losing the word's last letter
(`hanhj ` → `hạn ` instead of `hạnh `).

```sh
# terminal (ForwardKey tier), fast typing with key rollover
tools/typing-race-e2e/run.py -t ghostty -p rollover -n 50 'hanhj ' -e 'hạnh '
# GTK4 entry (SurroundingText tier), space within 5-20 ms after the tone key
tools/typing-race-e2e/run.py -t gtk -p tone-space -n 50 'hanhj ' -e 'hạnh '
# sway: start a specific build with debug logging for the run
tools/typing-race-e2e/run.py --daklak build/daklak -t foot -p burst 'hanhj ' -e 'hạnh '
# KDE: respawn KWin's daklak (picks up a rebuilt binary), then run
tools/typing-race-e2e/run.py --restart-im -t ghostty -p rollover -n 20 \
  'hanhj phucs nhieeuf tieengs vieetj nam ' -e 'hạnh phúc nhiều tiếng việt nam '
```

## Compositors

Focus is driven by `wm.py`:

- **sway / scroll:** `swaymsg` / `scrollmsg` IPC. `--daklak BIN` starts a
  build with debug logging to `--log` (default `/tmp/daklak-race-e2e.log`).
- **KDE (KWin):** one-shot KWin scripts loaded over D-Bus
  (`org.kde.KWin /Scripting`) that report back through `callDBus` to an object
  exported on the script's own session-bus connection. KWin launches daklak
  itself as its input method (kwinrc `[Wayland] InputMethod`), so `--daklak`
  is refused; the run uses the running instance and reads its
  `DAKLAK_LOG_PATH` from `/proc/PID/environ`. `--restart-im` sends it SIGKILL:
  KWin respawns an input method that dies from a signal (it does not respawn
  one that exits normally, including a Rust panic), so this loads a freshly
  built binary.

Without either, the new window is assumed to receive focus.

## Timing

Each key gets a random gap (press to next press) and hold (press to release).
When the hold exceeds the gap, the next key goes down before the previous one
comes up, like real rollover. Profiles:

| Profile | Gap (ms) | Hold (ms) | Extra |
| ------- | -------- | --------- | ----- |
| `steady` | 110-130 | 25-35 | the pace of `tools/ghostty-e2e` |
| `rollover` | 35-90 | 60-140 | frequent overlap |
| `burst` | 8-30 | 40-90 | heavy overlap |
| `tone-space` | 40-90 | 50-110 | the key after a tone key (`s f r x j`) follows in 5-20 ms |
| `pause-tone` | 40-90 | 50-110 | 2.3 s pause before a tone key, then `tone-space` |

`--gap MIN:MAX` and `--hold MIN:MAX` override a profile's ranges. Shift is
always released before the next key goes down, so overlap never turns an
unshifted character into a shifted one (`` ` `` into `~`).

`pause-tone` crosses daklak's 2-second idle policy. On terminals (ForwardKey)
idle resets the word on purpose, so a raw `hanhj` there is expected; on the GTK
entry (SurroundingText) the tone key must still compose.

## Output

Each iteration prints its UTC start time (daklak logs in UTC) and the received
line. The summary lists a histogram of outcomes and, for every mismatch, the
key schedule (`char@press_ms+hold_ms`), a `--replay SEED:ITER` argument that
reruns exactly that schedule, and a copy of the daklak log written during that
line (`/tmp/daklak-race-e2e-PID-TARGET-N.log`). The run stops if daklak dies.

Exit codes: **0** for a completed run with no mismatches, **1** for incorrect
text (including browser model/DOM disagreement even without `-e`), and **2**
for a harness failure or interrupted run. Missing browser reports are not
counted as successful `None` output.

## Targets

| Target | Window | daklak tier |
| ------ | ------ | ----------- |
| `ghostty`, `foot`, `kitty` | terminal running `cat > FILE` (echo on) | ForwardKey |
| `gtk`, `gtk3` | GTK4 / GTK3 `Gtk.Entry` (`gtk_entry.py`), Enter records and clears the line | SurroundingText |
| `firefox`, `chromium` | fresh-profile window on a local page (`browser_page.py`); `--field` picks the text field (below) | SurroundingText |

`--busy` (`gtk`, `gtk3`) blocks the entry's main loop 40 ms out of every 50 ms,
like an app under load. Wayland events then pile up and are dispatched in
batches. GDK applies a text-input `commit_string` as soon as it reads it but
queues keyboard events, so a busy client shows ordering bugs an idle one hides:
with capitals sent as committed text on KWin, every fast line of capitalised
words came out as `cho MotH aiB a ...`. Lines of lowercase keys are unaffected.

```sh
tools/typing-race-e2e/run.py -t gtk3 --busy -p burst --gap 5:25 -n 20 \
  'cho Mot Hai Ba Bon Nam ' -e 'cho Mot Hai Ba Bon Nam '
```

### Browser fields

| `--field` | What it is |
| --------- | ---------- |
| `input`, `textarea`, `contenteditable` | native editable elements, no editor framework |
| `prosemirror` | ProseMirror composer, the editor behind claude.ai (Tiptap) and ChatGPT: paragraph blocks with ProseMirror's trailing `<br>`, placeholder decoration, history |
| `lexical` | Lexical rich-text composer, the editor behind Facebook / Messenger: edits go through `beforeinput` into Lexical's model, then re-render |
| `tg-like` | hand-written stand-in for composers like Telegram Web's (not its code): keeps its own text and re-renders the whole field into per-word spans after every edit, restoring the caret by offset |

These are the real libraries on a local page, not the sites themselves, so
site-specific plugins and markup are missing. On Enter each editor records its
own document text and the visible page text and then clears itself like a chat
composer after sending. If the two differ, the line is recorded as
`MODEL='…' DOM='…'`, which fails `-e` even when the visible text is right.

The prebuilt `editors.bundle.js` is included alongside the source, so editor
assets are served locally without a CDN. To rebuild it after changing
`editors/src/main.js` or the pinned
versions in `editors/package.json`:

```sh
cd tools/typing-race-e2e/editors && npm install && npm run build
```

`--browser-bin PATH` runs another binary of the same browser family, e.g.
Firefox Developer Edition. Firefox starts on a fresh profile whose `user.js`
turns off first-run UI; the Terms of Use onboarding modal in particular takes
focus and would swallow every typed key (`browser.preonboarding.enabled=false`
is the opt-out Firefox's own automation uses).

### Readiness and failure diagnostics

Browser runs wait for the editor to mount, finish its initial model update,
and have focus and a valid selection inside the editable element. This is
observational: the harness never inserts warm-up characters, moves the caret,
or steals focus to make an editor look ready. The **first line is still tested**.
`--browser-timeout SECONDS` controls readiness and per-line report waits
(default 10). Script errors, browser exits, and timeouts are reported as
harness failures, separately from wrong text.

Each Enter report carries an iteration ID. The runner waits for that exact ID,
so delayed/out-of-order HTTP responses cannot be assigned to another line.
Duplicate or malformed reports fail the harness; a missing report stops the
run rather than shifting all later results.

`--browser-trace` opts into event/model diagnostics:

```sh
tools/typing-race-e2e/run.py -t firefox --browser-bin ~/.local/share/firefox-de/firefox \\
  --field prosemirror --browser-trace -p steady -n 3 --seed 3 \\
  'swword{L}{L}{L}{BS}ww' -e 'sword'
```

`--native-trace DIR` (`firefox` only) runs Firefox with `WAYLAND_DEBUG=client`
and `MOZ_LOG=IMEHandler:5,sync`, keeping `DIR/wayland.log` (the protocol events
Firefox received, in order) and `DIR/ime.log.moz_log` (the order its IME
handler processed key events and commits). Comparing the two tells whether an
ordering bug happened on the wire or inside the client. The logs cover the
whole run, so keep it short (`--line N` with `poem.py`).

Only failed lines get a `.browser.json` artifact next to the daemon log;
startup/runtime harness failures get a `harness.browser.json` artifact. They
include the seed, schedule, iteration ID, full submitted model and DOM text,
and diagnostics. Successful line traces are discarded. The event ring keeps
at most **256** entries; text/HTML previews are capped at **1024 UTF-16 code
units**, with lengths and a dropped-event count. The actual submitted result
is not truncated.

Traces capture keyboard, `beforeinput`/`input`, composition and selection
changes, DOM mutations, editor updates, and before/after-clear snapshots.
Selection offsets are browser UTF-16 text offsets, not daemon byte offsets or
ProseMirror document positions. Event `ms` plus `timeOrigin` gives epoch
milliseconds for alignment with the daemon log. Trace data is sent only to the
local test server. If no line was submitted, diagnostics may contain only the
last readiness state and script errors, rather than a completed event trace.

Tracing reads editor/DOM state and can perturb timing. Confirm intermittent
failures without tracing as well; a clean trace-enabled run is not proof a race
is gone. It never cancels input events or changes the editors' clear/render
rules.

Harness-only tests (no live compositor/browser/uinput device needed):

```sh
python3 -m unittest discover -s tools/typing-race-e2e -p 'test_*.py'
node --test tools/typing-race-e2e/test_browser_harness.cjs
```

### Editing keys

`{L}`, `{R}`, `{BS}`, `{CBS}` and `{P}` in the typed text press Left, Right,
Backspace, Ctrl+Backspace, or pause for 2.3 s (crossing daklak's 2 s idle
policy):

```sh
# typo `anwser`, delete the `w`, then `ww` after the `s` → `answer`
tools/typing-race-e2e/run.py -t firefox --field prosemirror -p steady \
  'anwwser{L}{L}{L}{BS}{R}ww' -e 'answer'
```

### Poem with realistic mistake corrections (`poem.py`)

`poem.py` types a 26-line Vietnamese poem into any target, injecting mistakes
and corrections at `--rate` (default `0.12`, ~12% of words): tone-mark
overrides (`Ddanhfs` → `Đánh`), missed vowel modifiers fixed with Backspace
(`nuoc{BS}{BS}wowcs` → `nước`), spillover/consonant typos deleted with
Backspace (`gioongs` / `hanhjph{BS}{BS}{BS}j`), whole-word deletes with
Ctrl+Backspace (`Myr{CBS}Myx` → `Mỹ`), and cursor-move edits (`{L}{BS}{R}`).

```sh
# inspect the generated Telex + corrections without launching a window
tools/typing-race-e2e/poem.py --dry-run
# run all 26 lines into Firefox ProseMirror with rollover timing
tools/typing-race-e2e/poem.py -t firefox --field prosemirror -p rollover --seed 42
# replay a single poem line
tools/typing-race-e2e/poem.py -t firefox --field prosemirror --seed 42 --line 16
```

Terminals keep echo on: on KWin, foot and kitty first look like
SurroundingText clients, and daklak downgrades them to ForwardKey only after
seeing the empty surrounding-text frames a redraw produces. A terminal that
never redraws (`stty -echo`) never sends them.

Ghostty enables text-input only after its first focus change, so focus is
bounced through another window first; `--no-refocus` tests daklak's synthetic
session instead.

## Requirements

A running daklak (or `--daklak BIN` on sway), `scrollmsg`/`swaymsg` or KWin,
`python-evdev`, write access to `/dev/uinput`, `python-gobject` (KWin control
and the `gtk` / `gtk3` targets, which also need GTK4 / GTK3), and the target
program. Don't type or change focus while it runs; the run
stops if the target window loses focus.
