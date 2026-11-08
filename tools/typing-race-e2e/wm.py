"""Window focus control for run.py: sway / scroll IPC or KWin scripting.

Windows are matched by a substring of their title. Each backend provides
focused_title(), wait_for(title), focus_title(title), focus_other(title),
save_focus() and restore_focus(token).
"""
import glob
import json
import os
import shutil
import subprocess
import tempfile
import time


class SwayWm:
    name = "sway"

    def __init__(self, cmd):
        self.cmd = cmd

    @classmethod
    def detect(cls):
        runtime = os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
        socks = sorted(glob.glob(f"{runtime}/scroll-ipc.*.sock"), key=os.path.getmtime)
        cmds = []
        if shutil.which("scrollmsg") and socks:
            cmds.append(["scrollmsg", "-s", socks[-1]])
        if shutil.which("swaymsg") and os.environ.get("SWAYSOCK"):
            cmds.append(["swaymsg"])
        # A stale SWAYSOCK / socket can outlive its session: probe it.
        for cmd in cmds:
            if subprocess.run(cmd + ["-t", "get_version"], capture_output=True).returncode == 0:
                return cls(cmd)
        return None

    def _nodes(self):
        out = subprocess.run(self.cmd + ["-t", "get_tree"], capture_output=True, check=True)
        stack = [json.loads(out.stdout)]
        while stack:
            n = stack.pop()
            yield n
            stack.extend(n.get("nodes", []) + n.get("floating_nodes", []))

    def _focus_id(self, con_id):
        subprocess.run(self.cmd + [f"[con_id={con_id}] focus"], capture_output=True, check=False)

    def focused_title(self):
        n = next((n for n in self._nodes() if n.get("focused")), None)
        return n.get("name") if n else None

    def wait_for(self, title, timeout=20):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if any(title in (n.get("name") or "") for n in self._nodes()):
                return True
            time.sleep(0.2)
        return False

    def focus_title(self, title):
        n = next((n for n in self._nodes() if title in (n.get("name") or "")), None)
        if n:
            self._focus_id(n["id"])

    def focus_other(self, title):
        n = next((n for n in self._nodes()
                  if n.get("pid") and title not in (n.get("name") or "")), None)
        if n:
            self._focus_id(n["id"])
        return n is not None

    def save_focus(self):
        return next((n["id"] for n in self._nodes() if n.get("focused")), None)

    def restore_focus(self, token):
        if token is not None:
            self._focus_id(token)


class KWinWm:
    """Runs one-shot KWin scripts; each reports back through callDBus to an
    object exported on this process's own session-bus connection."""

    name = "kwin"
    IFACE = "org.daklak.E2E"
    XML = f"""<node><interface name="{IFACE}">
      <method name="Report"><arg type="s" direction="in"/></method>
    </interface></node>"""

    def __init__(self):
        from gi.repository import Gio, GLib
        self.Gio, self.GLib = Gio, GLib
        self.conn = Gio.bus_get_sync(Gio.BusType.SESSION)
        self.replies = []
        node = Gio.DBusNodeInfo.new_for_xml(self.XML)
        self.conn.register_object("/e2e", node.interfaces[0], self._on_call, None, None)
        self.seq = 0

    @classmethod
    def detect(cls):
        if "KDE" not in os.environ.get("XDG_CURRENT_DESKTOP", "") and \
                not subprocess.run(["pgrep", "-x", "kwin_wayland"],
                                   capture_output=True).returncode == 0:
            return None
        try:
            return cls()
        except Exception:  # noqa: BLE001 — no gi / no session bus
            return None

    def _on_call(self, conn, sender, path, iface, method, params, invocation):
        self.replies.append(params.unpack()[0])
        invocation.return_value(None)

    def _scripting(self, path, iface, method, args, ret):
        GLib = self.GLib
        return self.conn.call_sync(
            "org.kde.KWin", path, iface, method, args,
            GLib.VariantType(ret) if ret else None, 0, 3000, None)

    def _run(self, body):
        """Run JS `body` (which must call report(string) once); return it."""
        GLib = self.GLib
        self.seq += 1
        plugin = f"daklak-e2e-{os.getpid()}-{self.seq}"
        js = (f'function report(s) {{ callDBus("{self.conn.get_unique_name()}", "/e2e", '
              f'"{self.IFACE}", "Report", String(s)); }}\n'
              f"(function () {{\n{body}\n}})();\n")
        fd, path = tempfile.mkstemp(suffix=".js", prefix="daklak-e2e-")
        with os.fdopen(fd, "w") as f:
            f.write(js)
        self.replies.clear()
        try:
            sid = self._scripting("/Scripting", "org.kde.kwin.Scripting", "loadScript",
                                  GLib.Variant("(ss)", (path, plugin)), "(i)").unpack()[0]
            self._scripting(f"/Scripting/Script{sid}", "org.kde.kwin.Script", "run",
                            None, None)
            ctx = GLib.MainContext.default()
            deadline = time.time() + 3
            while not self.replies and time.time() < deadline:
                ctx.iteration(False)
                time.sleep(0.005)
        finally:
            self._scripting("/Scripting", "org.kde.kwin.Scripting", "unloadScript",
                            GLib.Variant("(s)", (plugin,)), "(b)")
            os.unlink(path)
        if not self.replies:
            raise RuntimeError("KWin script did not report back")
        return self.replies[0]

    @staticmethod
    def _js_str(s):
        return json.dumps(s)

    def focused_title(self):
        r = self._run("const w = workspace.activeWindow; report(w ? w.caption : '');")
        return r or None

    def wait_for(self, title, timeout=20):
        deadline = time.time() + timeout
        q = self._js_str(title)
        while time.time() < deadline:
            if self._run(f"report(workspace.windowList().some(w => w.caption.includes({q})));") == "true":
                return True
            time.sleep(0.3)
        return False

    def focus_title(self, title):
        q = self._js_str(title)
        self._run(f"""for (const w of workspace.windowList())
            if (w.caption.includes({q})) {{ workspace.activeWindow = w; return report(1); }}
          report(0);""")

    def focus_other(self, title):
        q = self._js_str(title)
        return self._run(f"""for (const w of workspace.windowList())
            if (w.normalWindow && !w.minimized && !w.caption.includes({q}))
              {{ workspace.activeWindow = w; return report(1); }}
          report(0);""") == "1"

    def save_focus(self):
        r = self._run("const w = workspace.activeWindow; report(w ? w.internalId.toString() : '');")
        return r or None

    def restore_focus(self, token):
        if token:
            q = self._js_str(token)
            self._run(f"""for (const w of workspace.windowList())
                if (w.internalId.toString() === {q}) {{ workspace.activeWindow = w; break; }}
              report(1);""")


def detect():
    return SwayWm.detect() or KWinWm.detect()
