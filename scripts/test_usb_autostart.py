import importlib.util
from pathlib import Path
import plistlib
import unittest
from unittest.mock import patch
import subprocess
import tempfile

spec = importlib.util.spec_from_file_location("usb_autostart", Path(__file__).with_name("usb-autostart.py"))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class AgentTests(unittest.TestCase):
    def test_usb_job_is_event_triggered_and_preserves_config_and_xml_paths(self):
        app = Path("/Users/test/Applications/Trackpad & Companion.app")
        config = Path("/Users/test/my config & tuning.toml")
        job = installer.agent_contents(app, config, Path("/tmp/companion.log"), 0x048D, 0x8911)
        # Use a real plist round-trip to catch escaping and type errors.
        job = plistlib.loads(plistlib.dumps(job))
        self.assertEqual(job["ProgramArguments"], [
            str(app / "Contents/MacOS/companion"), "--config", str(config), "--usb-triggered",
        ])
        self.assertFalse(job["RunAtLoad"])
        self.assertFalse(job["KeepAlive"])
        self.assertNotIn("StartInterval", job)
        self.assertNotIn("StartCalendarInterval", job)
        match = job["LaunchEvents"]["com.apple.iokit.matching"]["trackpad-attached"]
        self.assertEqual(match["IOProviderClass"], "IOUSBHostDevice")
        self.assertEqual(match["idVendor"], 1165)
        self.assertEqual(match["idProduct"], 35089)
        self.assertTrue(match["IOMatchLaunchStream"])

    def test_permission_reopen_is_stopped_before_starting_managed_job(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder)
            lock = home / "Library/Caches/macos-trackpad-companion/instance.lock"
            lock.parent.mkdir(parents=True)
            lock.write_text("1234")
            executable = str(home / "Applications" / installer.APP_NAME / "Contents/MacOS/companion")
            with patch.object(installer.Path, "home", return_value=home), \
                    patch.object(installer.subprocess, "run", side_effect=[
                        subprocess.CompletedProcess([], 0, executable + "\n", ""),
                        subprocess.CompletedProcess([], 0),
                    ]) as run, \
                    patch.object(installer.os, "kill", side_effect=[None, ProcessLookupError()]) as kill:
                installer.restart(None)
                kill.assert_any_call(1234, installer.signal.SIGTERM)
                self.assertEqual(run.call_args.args[0][1], "kickstart")

    def test_recycled_lock_pid_or_separate_build_is_never_signalled(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder)
            lock = home / "Library/Caches/macos-trackpad-companion/instance.lock"
            lock.parent.mkdir(parents=True)
            lock.write_text("1234")
            with patch.object(installer.Path, "home", return_value=home), \
                    patch.object(installer.subprocess, "run", return_value=
                        subprocess.CompletedProcess([], 0, "/other/target/release/companion\n", "")), \
                    patch.object(installer.os, "kill") as kill:
                with self.assertRaisesRegex(RuntimeError, "separate Terminal"):
                    installer.restart(None)
                kill.assert_not_called()

    def test_failed_process_inspection_does_not_signal_or_claim_handover(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder)
            lock = home / "Library/Caches/macos-trackpad-companion/instance.lock"
            lock.parent.mkdir(parents=True)
            lock.write_text("1234")
            with patch.object(installer.Path, "home", return_value=home), \
                    patch.object(installer.subprocess, "run", return_value=
                        subprocess.CompletedProcess([], 1, "", "operation not permitted")) as run, \
                    patch.object(installer.os, "kill") as kill:
                with self.assertRaisesRegex(RuntimeError, "could not inspect"):
                    installer.restart(None)
                kill.assert_not_called()
                self.assertEqual(run.call_count, 1)

    def test_invalid_lock_cannot_target_a_process_group(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder)
            lock = home / "Library/Caches/macos-trackpad-companion/instance.lock"
            lock.parent.mkdir(parents=True)
            with patch.object(installer.Path, "home", return_value=home), \
                    patch.object(installer.subprocess, "run") as run, \
                    patch.object(installer.os, "kill") as kill:
                for contents in ["", "not-a-pid", "0", "-1"]:
                    with self.subTest(contents=contents):
                        lock.write_text(contents)
                        with self.assertRaisesRegex(RuntimeError, "invalid companion"):
                            installer.restart(None)
                run.assert_not_called()
                kill.assert_not_called()

    def test_owner_exiting_after_inspection_does_not_prevent_managed_start(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder)
            lock = home / "Library/Caches/macos-trackpad-companion/instance.lock"
            lock.parent.mkdir(parents=True)
            lock.write_text("1234")
            executable = str(home / "Applications" / installer.APP_NAME / "Contents/MacOS/companion")
            with patch.object(installer.Path, "home", return_value=home), \
                    patch.object(installer.subprocess, "run", side_effect=[
                        subprocess.CompletedProcess([], 0, executable + "\n", ""),
                        subprocess.CompletedProcess([], 0),
                    ]) as run, \
                    patch.object(installer.os, "kill", side_effect=ProcessLookupError()):
                installer.restart(None)
                self.assertEqual(run.call_args.args[0][1], "kickstart")


if __name__ == "__main__":
    unittest.main()
