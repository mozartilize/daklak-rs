"""Local browser page and sequence-numbered reports for run.py.

Native fields and real rich-text libraries are served offline. The runner
waits for a mounted, settled, focused editor before typing (no warm-up), then
reads each Enter report by iteration ID, never by HTTP arrival order. Optional
traces describe browser events/model updates and the post-send clear.
"""
import http.server
import json
import os
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
BUNDLE = os.path.join(HERE, "editors.bundle.js")
EDITOR_FIELDS = ("prosemirror", "lexical", "tg-like")
FIELDS = ("input", "textarea", "contenteditable") + EDITOR_FIELDS

STYLE = """
body { margin: 2em; font: 20px sans-serif; }
#f { width: 90%; min-height: 1.5em; border: 1px solid; padding: 4px; white-space: pre-wrap; }
#f p { margin: 0; }
#f p.is-empty::before { content: attr(data-placeholder); color: #999; float: left;
  height: 0; pointer-events: none; }
#f code { background: #eee; }
#f a { color: #26c; }
"""

NATIVE_PAGE = """<!doctype html>
<meta charset="utf-8">
<title>{title}</title>
<style>{style}</style>
<body>
{element}
<script src="/harness.js"></script>
<script>
const harness = window.createDaklakHarness({{trace: {trace}}});
const f = document.getElementById('f');
const editable = f.isContentEditable;
const read = () => {{
  const text = editable ? f.innerText.replace(/\\u00a0/g, ' ') : f.value;
  return {{model: text, dom: text}};
}};
f.focus();
f.addEventListener('keydown', ev => {{
  if (ev.key !== 'Enter') return;
  ev.preventDefault();
  const report = read();
  const beforeClear = harness.beforeClear();
  if (editable) f.textContent = ''; else f.value = '';
  harness.submit(report, beforeClear);
}});
harness.mount({{root: f, read, ready: Promise.resolve()}}).catch(harness.error);
</script>
"""

EDITOR_PAGE = """<!doctype html>
<meta charset="utf-8">
<title>{title}</title>
<style>{style}</style>
<body>
<div id="f"></div>
<script src="/harness.js"></script>
<script>
const harness = window.createDaklakHarness({{trace: {trace}}});
</script>
<script src="/editors.js"></script>
<script>
try {{
  const editor = window.daklakEditors[{kind}](document.getElementById('f'),
    harness.submit, {{trace: harness.trace, error: harness.error}});
  harness.mount(editor).catch(harness.error);
}} catch (error) {{ harness.error(error); }}
</script>
"""

NATIVE_ELEMENTS = {
    "input": '<input id="f" type="text" autofocus>',
    "textarea": '<textarea id="f" rows="3" autofocus></textarea>',
    "contenteditable": '<div id="f" contenteditable="true"></div>',
}


def page_html(title, field, trace=False):
    args = dict(title=title, style=STYLE, trace=json.dumps(trace))
    if field in EDITOR_FIELDS:
        return EDITOR_PAGE.format(**args, kind=json.dumps(field))
    return NATIVE_PAGE.format(**args, element=NATIVE_ELEMENTS[field])


def format_line(report):
    model, dom = report["model"], report["dom"]
    if not isinstance(model, str) or not isinstance(dom, str):
        raise ValueError("model and dom must be strings")
    return model if model == dom else f"MODEL={model!r} DOM={dom!r}"


class HarnessError(RuntimeError):
    pass


class BrowserSession:
    def __init__(self):
        self.condition = threading.Condition()
        self.status = {}
        self.errors = []
        self.reports = {}
        self.consumed = -1
        self.server = None

    def accept(self, path, report):
        if not isinstance(report, dict):
            raise ValueError("report must be an object")
        with self.condition:
            if path == "/status":
                version = report.get("version")
                if type(version) is not int or version < 0 or type(report.get("ready")) is not bool:
                    raise ValueError("invalid readiness status")
                state = report.get("state", {})
                selection = state.get("selection", {})
                if report["ready"] and not (
                    state.get("mounted") and state.get("settled") and state.get("editable")
                    and state.get("focused")
                    and selection.get("anchor") is not None and selection.get("focus") is not None
                ):
                    raise ValueError("ready without mounted/settled/editable/focused selection")
                if version > self.status.get("version", -1):
                    self.status = report
            elif path == "/line":
                iteration = report.get("iteration")
                if type(iteration) is not int or iteration < 0:
                    raise ValueError("missing/invalid iteration ID")
                format_line(report)  # validate before storing
                if iteration <= self.consumed or iteration in self.reports:
                    raise ValueError(f"duplicate iteration {iteration}")
                if len(self.reports) >= 32:
                    raise ValueError("too many unconsumed reports")
                self.reports[iteration] = report
            elif path == "/error":
                self.errors.append(report)
                if len(self.errors) > 10:
                    self.errors = self.errors[:1] + self.errors[-9:]
            else:
                raise ValueError(f"unknown endpoint {path}")
            self.condition.notify_all()

    def ensure_healthy(self):
        if self.errors:
            raise HarnessError(self.errors[0].get("message", "browser script failed"))

    def _wait(self, predicate, description, timeout, process):
        deadline = time.monotonic() + timeout
        with self.condition:
            while True:
                self.ensure_healthy()
                if process is not None and process.poll() is not None:
                    raise HarnessError(f"browser exited ({process.returncode}) while waiting for {description}")
                if predicate():
                    return
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise HarnessError(f"timeout waiting for {description}; last status={self.status!r}")
                self.condition.wait(min(remaining, 0.1))

    def wait_ready(self, timeout=10, process=None):
        self._wait(lambda: self.status.get("ready"), "editor readiness", timeout, process)

    def wait_report(self, iteration, timeout=10, process=None):
        self._wait(lambda: iteration in self.reports, f"iteration {iteration}", timeout, process)
        with self.condition:
            self.ensure_healthy()
            report = self.reports.pop(iteration)
            self.consumed = iteration
            return report

    def verify_complete(self, expected):
        with self.condition:
            self.ensure_healthy()
            if self.consumed != expected - 1 or self.reports:
                raise HarnessError(f"unexpected reports after {expected} iterations: "
                                   f"pending={sorted(self.reports)}")

    def diagnostics(self):
        with self.condition:
            return {"status": self.status, "errors": self.errors,
                    "pendingIterations": sorted(self.reports)}

    def close(self):
        if self.server is not None:
            self.server.shutdown()
            self.server.server_close()


def serve(title, out, field="input", trace=False, session=None):
    """Start the local server; returns (url, server) with server.session."""
    if field not in FIELDS:
        raise ValueError(f"unknown field {field}")
    if field in EDITOR_FIELDS and not os.path.exists(BUNDLE):
        raise HarnessError(f"{BUNDLE} missing; run `npm install && npm run build` "
                         "in tools/typing-race-e2e/editors")
    pages = {"/": ("text/html", page_html(title, field, trace).encode())}
    with open(os.path.join(HERE, "browser_harness.js"), "rb") as f:
        pages["/harness.js"] = ("text/javascript", f.read())
    if field in EDITOR_FIELDS:
        with open(BUNDLE, "rb") as f:
            pages["/editors.js"] = ("text/javascript", f.read())
    session = session or BrowserSession()

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            ctype, body = pages.get(self.path, (None, None))
            if body is None:
                self.send_response(404)
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("Content-Type", f"{ctype}; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            if self.path not in ("/line", "/status", "/error"):
                self.send_response(404)
                self.end_headers()
                return
            try:
                n = int(self.headers.get("Content-Length", 0))
                if not 0 <= n <= 4 * 1024 * 1024:
                    raise ValueError("oversized report")
                report = json.loads(self.rfile.read(n))
                with session.condition:
                    session.accept(self.path, report)
                    # Kept for inspecting raw output; the runner uses IDs, not
                    # this file's HTTP arrival order.
                    if self.path == "/line":
                        with open(out, "a", encoding="utf-8", errors="backslashreplace") as f:
                            f.write(format_line(report).replace("\n", "\\n") + "\n")
            except (ValueError, KeyError, TypeError, AttributeError) as error:
                session.accept("/error", {"message": f"bad {self.path} report: {error}"})
                self.send_response(400)
            else:
                self.send_response(204)
            self.end_headers()

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.session = session
    session.server = server
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{server.server_address[1]}/", server
