<!-- documentation sources of truth -->
## Sources of truth: code and `docs/` only

**For anything about how the code works, trust only: (1) the source code
itself, and (2) the published documentation under [`docs/`](docs/) and the root
[`README.md`](README.md).** Code is the final ground truth; `docs/` is the
official, maintained description of it.

**Do NOT treat any of the following as authoritative — they are local,
unofficial drafts and scratch notes, and will be deleted in a future
iteration:**

- root-level `plan*.md` and any files under `plans/`
- `thinking.md`, `chatgpt-review*.md`, `*-thinking.md`, and similar drafts
- `docs/superpowers/` and any other un-tracked draft notes
- claude-mem observations and the `memory/` store
- `*.log` files and one-off probe outputs

These may be stale, speculative, or contradicted by the current code. You may
read them for historical context, but **never cite them as the reason code
behaves a certain way, and never copy claims from them into `docs/` without
re-verifying against the source.** When you learn something durable, put it in
`docs/`, not in a scratch file.

**Never reference these scratch artifacts from committed code, comments, test
names/docs, or `docs/`.** They are ephemeral and will be deleted, leaving
dangling references. Concretely: no `logs/…`, `plans/…`, `memory/…`,
`thinking.md`, observation IDs, or similar paths in source — not even in a
doc-comment like `/// Replays logs/foo.log`. A test reproducing a captured
trace must **describe the scenario inline** (app, keystrokes, expected output),
not point at the log it came from. Code and `docs/` must stand on their own
once every scratch file is gone.

## Formatting discipline

Do not run broad formatters such as `cargo fmt` or `rustfmt` over the whole
workspace/package/file unless the user explicitly asks for formatting. Broad
formatting creates noisy diffs and can rewrite unrelated vendored or scratch
code.

Never format files you did not otherwise edit. If a formatting check is needed,
scope it to the touched file(s) only, and do not apply formatting outside the
current patch.
