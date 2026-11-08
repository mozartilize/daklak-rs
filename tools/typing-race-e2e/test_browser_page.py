"""Harness tests: no compositor, real browser or uinput keyboard required."""
import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import urllib.error
import urllib.request

import browser_page
import run


def status(version=0, ready=True):
    return {"version": version, "ready": ready,
            "state": {"mounted": True, "settled": True, "editable": True, "focused": ready,
                      "selection": {"anchor": 0, "focus": 0}}}


class ServerTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.out = Path(self.directory.name) / "out.txt"
        self.out.write_text("")
        self.url, self.server = browser_page.serve("test", str(self.out), trace=True)
        self.session = self.server.session

    def tearDown(self):
        self.session.close()
        self.directory.cleanup()

    def post(self, endpoint, report):
        data = json.dumps(report).encode()
        request = urllib.request.Request(self.url + endpoint.lstrip("/"), data=data)
        with urllib.request.urlopen(request, timeout=2) as response:
            self.assertEqual(response.status, 204)

    def test_utf16_string_is_not_lost_to_auxiliary_utf8_output(self):
        # JavaScript strings can contain a lone surrogate. The UTF-8 display
        # file may escape it, but the authoritative report must stay exact.
        text = "a\ud83d"
        self.post("/line", {"iteration": 0, "model": text, "dom": text})
        self.assertEqual(self.session.wait_report(0)["model"], text)
        self.assertEqual(self.out.read_text(), "a\\ud83d\n")
        self.session.verify_complete(1)

    def test_reports_are_associated_by_id_not_http_arrival(self):
        self.post("/line", {"iteration": 1, "model": "phúc", "dom": "phúc"})
        self.post("/line", {"iteration": 0, "model": "hạnh", "dom": "hạnh"})
        self.assertEqual(self.session.wait_report(0)["model"], "hạnh")
        self.assertEqual(self.session.wait_report(1)["model"], "phúc")

    def test_old_status_cannot_restore_readiness_after_focus_loss(self):
        self.post("/status", status(0))
        self.session.wait_ready(timeout=0.02)
        self.post("/status", status(2, False))
        self.post("/status", status(1))
        with self.assertRaisesRegex(browser_page.HarnessError, "editor readiness"):
            self.session.wait_ready(timeout=0.02)
        self.assertEqual(self.out.read_text(), "")

    def test_readiness_requires_settled_editor_and_selection(self):
        invalid = status()
        invalid["state"]["settled"] = False
        with self.assertRaises(urllib.error.HTTPError) as caught:
            self.post("/status", invalid)
        self.assertEqual(caught.exception.code, 400)
        with self.assertRaisesRegex(browser_page.HarnessError, "ready without"):
            self.session.wait_ready(timeout=0.02)

    def test_duplicate_report_is_an_error_not_a_second_line(self):
        report = {"iteration": 0, "model": "hạnh", "dom": "hạnh"}
        self.post("/line", report)
        self.assertEqual(self.session.wait_report(0)["model"], "hạnh")
        with self.assertRaises(urllib.error.HTTPError) as caught:
            self.post("/line", report)
        self.assertEqual(caught.exception.code, 400)
        self.assertEqual(self.out.read_text(), "hạnh\n")
        with self.assertRaisesRegex(browser_page.HarnessError, "duplicate iteration"):
            self.session.wait_report(1, timeout=0.02)

    def test_completion_rejects_extra_or_late_error_reports(self):
        self.post("/line", {"iteration": 0, "model": "hạnh", "dom": "hạnh"})
        self.session.wait_report(0)
        self.session.verify_complete(1)
        self.post("/line", {"iteration": 1, "model": "", "dom": ""})
        with self.assertRaisesRegex(browser_page.HarnessError, "unexpected reports"):
            self.session.verify_complete(1)
        self.post("/error", {"message": "clear failed"})
        with self.assertRaisesRegex(browser_page.HarnessError, "clear failed"):
            self.session.verify_complete(1)

    def test_missing_and_malformed_reports_do_not_become_empty_success(self):
        with self.assertRaisesRegex(browser_page.HarnessError, "iteration 0"):
            self.session.wait_report(0, timeout=0.02)
        with self.assertRaises(urllib.error.HTTPError):
            self.post("/line", {"iteration": 0, "model": "hạnh"})
        self.assertEqual(self.out.read_text(), "")
        with self.assertRaises(browser_page.HarnessError):
            self.session.wait_report(0, timeout=0.02)

    def test_cascading_errors_preserve_the_initial_failure(self):
        self.post("/error", {"message": "editor script failed"})
        for i in range(12):
            self.post("/error", {"message": f"follow-up {i}"})
        with self.assertRaisesRegex(browser_page.HarnessError, "editor script failed"):
            self.session.wait_ready(timeout=0.02)
        self.assertTrue(0 < len(self.session.errors) <= 10)
        self.assertEqual(self.session.errors[-1]["message"], "follow-up 11")

    def test_assets_and_instrumentation_are_local_and_trace_is_opt_in(self):
        for endpoint in ["", "harness.js"]:
            with urllib.request.urlopen(self.url + endpoint, timeout=2) as response:
                self.assertEqual(response.status, 200)
        for field in browser_page.FIELDS:
            html = browser_page.page_html("test", field)
            self.assertIn("trace: false", html)
            self.assertIn("/harness.js", html)
            self.assertIn("trace: true", browser_page.page_html("test", field, True))


class RunnerTests(unittest.TestCase):
    def test_trace_serialization_preserves_clipped_utf16_and_full_unicode(self):
        # Splitting a 2-unit emoji at a 1024-unit preview boundary leaves a
        # high surrogate; the actual submitted model still contains the emoji.
        preview = "a" * 1023 + "\ud83d"
        full = "a" * 1023 + "🙂"
        report = {"iteration": 0, "model": full, "dom": full,
                  "trace": {"preview": preview}}
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / "failure.browser.json")
            run.write_browser_trace(path, browser_page.BrowserSession(), {"seed": 3}, report)
            decoded = json.loads(Path(path).read_text(encoding="utf-8"))
        self.assertEqual(decoded["report"]["trace"]["preview"], preview)
        self.assertEqual(decoded["report"]["model"], full)

    def invoke(self, reports=(), error=None, comp=None):
        process = mock.Mock(pid=123456789)
        process.poll.return_value = None
        process.wait.return_value = 0
        original_exists = run.os.path.exists
        session_ref = {}

        def launch(*args):
            session = args[6]
            session_ref["session"] = session
            if error:
                session.accept("/error", {"message": error})
            else:
                session.accept("/status", status())
            return process

        def submit_reports(*_args):
            for report in reports:
                session_ref["session"].accept("/line", report)

        output = io.StringIO()
        with contextlib.ExitStack() as stack:
            stack.enter_context(mock.patch.object(run.sys, "argv", [
                "run.py", "-t", "firefox", "-n", "1", "--browser-timeout", "0.02", "hanhj",
            ]))
            stack.enter_context(mock.patch.object(run.wm, "detect", return_value=comp))
            stack.enter_context(mock.patch.object(run, "daklak_pid", return_value=123456789))
            stack.enter_context(mock.patch.object(run, "daklak_log_path", return_value=None))
            stack.enter_context(mock.patch.object(run, "UInput"))
            stack.enter_context(mock.patch.object(run, "launch", side_effect=launch))
            stack.enter_context(mock.patch.object(run.os, "killpg"))
            stack.enter_context(mock.patch.object(run.os.path, "exists", side_effect=lambda path:
                path == "/proc/123456789" or original_exists(path)))
            stack.enter_context(mock.patch.object(run.time, "sleep"))
            play = stack.enter_context(mock.patch.object(run, "play", side_effect=submit_reports))
            stack.enter_context(contextlib.redirect_stdout(output))
            stack.enter_context(contextlib.redirect_stderr(output))
            result = run.main()
        return result, output.getvalue(), play.call_count

    def test_missing_window_is_setup_failure_and_restores_focus(self):
        comp = mock.Mock()
        comp.name = "test"
        comp.save_focus.return_value = "previous"
        comp.wait_for.return_value = False
        result, output, played = self.invoke(comp=comp)
        self.assertEqual(result, 2)
        self.assertIn("target window never appeared", output)
        self.assertEqual(played, 0)
        comp.restore_focus.assert_called_once_with("previous")

    def test_script_failure_aborts_before_first_keystroke(self):
        result, output, played = self.invoke(error="editor initialization failed")
        self.assertEqual(result, 2)
        self.assertIn("editor initialization failed", output)
        self.assertEqual(played, 0)

    def test_missing_enter_report_is_harness_failure_even_without_expect(self):
        result, output, played = self.invoke()
        self.assertEqual(result, 2)
        self.assertIn("timeout waiting for iteration 0", output)
        self.assertEqual(played, 1)

    def test_extra_enter_report_is_not_ignored_at_end_of_run(self):
        result, output, played = self.invoke([
            {"iteration": 0, "model": "hạnh", "dom": "hạnh"},
            {"iteration": 1, "model": "", "dom": ""},
        ])
        self.assertEqual(result, 2)
        self.assertIn("unexpected reports", output)
        self.assertEqual(played, 1)

    def test_model_dom_mismatch_fails_even_without_expect(self):
        result, output, played = self.invoke([
            {"iteration": 0, "model": "hạnh", "dom": "hạn"},
        ])
        self.assertEqual(result, 1)
        self.assertIn("FAIL", output)
        self.assertIn("MODEL=", output)
        self.assertEqual(played, 1)


if __name__ == "__main__":
    unittest.main()
