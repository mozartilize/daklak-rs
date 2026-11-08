import argparse
import os
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))

import run  # noqa: E402


class LaunchTest(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)

    def launch(self, target, **kw):
        with mock.patch.object(run.subprocess, "Popen") as popen:
            run.launch(target, "title", "/out", self.tmpdir.name, "input", **kw)
        args, kwargs = popen.call_args
        return args[0], kwargs

    def test_gtk_targets_pick_version_and_busy_loop(self):
        entry = os.path.join(run.HERE, "gtk_entry.py")
        argv, _ = self.launch("gtk")
        self.assertEqual(argv[1:], [entry, "title", "/out"])
        argv, _ = self.launch("gtk3", busy=True)
        self.assertEqual(argv[1:], [entry, "--gtk3", "--busy", "title", "/out"])

    def test_firefox_native_trace_logs_into_the_given_directory(self):
        with mock.patch.object(run.browser_page, "serve", return_value=("http://x", None)):
            trace = os.path.join(self.tmpdir.name, "trace")
            argv, kwargs = self.launch("firefox", native_trace=trace)
            kwargs["stderr"].close()
            self.assertEqual(argv[0], "firefox")
            self.assertEqual(kwargs["env"]["WAYLAND_DEBUG"], "client")
            self.assertEqual(kwargs["env"]["MOZ_LOG"], "IMEHandler:5,sync")
            self.assertEqual(kwargs["env"]["MOZ_LOG_FILE"], os.path.join(trace, "ime.log"))
            self.assertEqual(kwargs["stderr"].name, os.path.join(trace, "wayland.log"))

    def test_firefox_without_native_trace_keeps_the_normal_environment(self):
        with mock.patch.object(run.browser_page, "serve", return_value=("http://x", None)):
            _, kwargs = self.launch("firefox")
        self.assertNotIn("env", kwargs)


class AppOptionsTest(unittest.TestCase):
    def parse(self, *argv):
        ap = argparse.ArgumentParser()
        ap.add_argument("-t", "--target")
        run.add_app_options(ap)
        args = ap.parse_args(argv)
        run.check_app_options(ap, args)
        return args

    def test_options_are_refused_on_other_targets(self):
        for argv in (["-t", "foot", "--busy"], ["-t", "chromium", "--native-trace", "d"]):
            with self.assertRaises(SystemExit), mock.patch("sys.stderr"):
                self.parse(*argv)
        self.assertTrue(self.parse("-t", "gtk3", "--busy").busy)


if __name__ == "__main__":
    unittest.main()
