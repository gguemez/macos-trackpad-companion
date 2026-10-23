#!/usr/bin/env python3
"""Prepare/install a per-user, USB-triggered companion. No root required."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import signal
import subprocess
import time

LABEL = "net.guemez.trackpad-companion.usb"
APP_NAME = "Trackpad Companion USB.app"


def destinations():
    home = Path.home()
    return (
        home / "Applications" / APP_NAME,
        home / "Library/LaunchAgents" / (LABEL + ".plist"),
        home / "Library/Logs/macos-trackpad-companion.usb.log",
    )


def agent_contents(app, config, log, vid, pid):
    return {
        "Label": LABEL,
        "AssociatedBundleIdentifiers": ["net.guemez.trackpad-companion"],
        "ProgramArguments": [
            str(app / "Contents/MacOS/companion"), "--config", str(config),
            "--usb-triggered",
        ],
        "RunAtLoad": False,
        "KeepAlive": False,
        "ThrottleInterval": 1,
        "ProcessType": "Interactive",
        "LimitLoadToSessionType": "Aqua",
        "StandardOutPath": str(log),
        "StandardErrorPath": str(log),
        "LaunchEvents": {
            "com.apple.iokit.matching": {
                "trackpad-attached": {
                    "IOProviderClass": "IOUSBHostDevice",
                    "idVendor": vid,
                    "idProduct": pid,
                    "IOMatchLaunchStream": True,
                },
            },
        },
    }


def binary_hash(app):
    return hashlib.sha256((app / "Contents/MacOS/companion").read_bytes()).hexdigest()


def prepare(args):
    source = args.app.expanduser().resolve(strict=True)
    config = args.config.expanduser().resolve(strict=True)
    output = args.output.expanduser().resolve()
    if output.exists():
        raise RuntimeError("prepared directory already exists; choose a new --output")
    info = plistlib.loads((source / "Contents/Info.plist").read_bytes())
    if info.get("CFBundleIdentifier") != "net.guemez.trackpad-companion":
        raise RuntimeError("unexpected application bundle identity")
    exe = source / "Contents/MacOS/companion"
    help_text = subprocess.check_output([str(exe), "--help"], text=True)
    if "--usb-triggered" not in help_text:
        raise RuntimeError("build a companion with USB-triggered support first")
    subprocess.run(["/usr/bin/codesign", "--verify", "--deep", str(source)], check=True)
    output.mkdir(parents=True)
    shutil.copytree(source, output / "Application.app")
    app, agent, log = destinations()
    job = agent_contents(app, config, log, args.vid, args.pid)
    (output / "LaunchAgent.plist").write_bytes(plistlib.dumps(job))
    manifest = {
        "application": str(app), "agent": str(agent), "log": str(log),
        "config": str(config), "vid": args.vid, "pid": args.pid,
        "sha256": binary_hash(output / "Application.app"),
    }
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(manifest, indent=2))


def service():
    return "gui/{}/{}".format(os.getuid(), LABEL)


def install(args):
    prepared = args.prepared.expanduser().resolve(strict=True)
    manifest = json.loads((prepared / "manifest.json").read_text())
    source = prepared / "Application.app"
    app, agent, log = destinations()
    expected_paths = (str(app), str(agent), str(log))
    if tuple(manifest[k] for k in ("application", "agent", "log")) != expected_paths:
        raise RuntimeError("prepared files belong to another user or installation")
    if binary_hash(source) != manifest["sha256"]:
        raise RuntimeError("prepared executable changed; prepare again")
    subprocess.run(["/usr/bin/codesign", "--verify", "--deep", str(source)], check=True)
    config = Path(manifest["config"])
    if not config.is_file():
        raise RuntimeError("configuration file has moved or disappeared")
    job = agent_contents(app, config, log, manifest["vid"], manifest["pid"])
    if plistlib.loads((prepared / "LaunchAgent.plist").read_bytes()) != job:
        raise RuntimeError("prepared launch agent changed; prepare again")
    login_agent = agent.parent / "net.guemez.trackpad-companion.plist"
    if login_agent.exists():
        raise RuntimeError("disable the app's Start at Login option first")
    if app.exists():
        info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
        if info.get("CFBundleIdentifier") != "net.guemez.trackpad-companion":
            raise RuntimeError("refusing to replace an unrelated application")
    subprocess.run(["/bin/launchctl", "bootout", service()], capture_output=True)
    app.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source, app, dirs_exist_ok=True)
    agent.parent.mkdir(parents=True, exist_ok=True)
    log.parent.mkdir(parents=True, exist_ok=True)
    temporary = agent.with_suffix(".plist.tmp")
    temporary.write_bytes(plistlib.dumps(job))
    temporary.chmod(0o644)
    temporary.replace(agent)
    subprocess.run(["/usr/bin/plutil", "-lint", str(agent)], check=True)
    subprocess.run([
        "/bin/launchctl", "bootstrap", "gui/{}".format(os.getuid()), str(agent),
    ], check=True)
    print("USB autostart enabled for {:04x}:{:04x}.".format(manifest["vid"], manifest["pid"]))
    print("Application:", app)
    print("Config:", config)
    print("Log:", log)


def uninstall(_args):
    # Keep the installed app and user's config. Removing this one launch
    # agent disables activation and stops its managed instance.
    _app, agent, _log = destinations()
    subprocess.run(["/bin/launchctl", "bootout", service()], capture_output=True)
    if agent.exists():
        agent.unlink()
    print("USB autostart disabled; app and configuration retained.")


def restart(_args):
    # Privacy settings may reopen the .app through Launch Services without
    # its launchd arguments. Stop that exact installed executable before
    # handing control back to the USB job. Never signal a recycled PID or
    # a separate Terminal/development build.
    app, _agent, _log = destinations()
    executable = str(app / "Contents/MacOS/companion")
    lock = Path.home() / "Library/Caches/macos-trackpad-companion/instance.lock"
    if lock.exists():
        try:
            pid = int(lock.read_text().strip())
        except ValueError as error:
            raise RuntimeError("invalid companion lock; wait for startup to finish and retry") from error
        if pid <= 1:
            raise RuntimeError("invalid companion PID; refusing to signal it")
        result = subprocess.run([
            "/bin/ps", "-p", str(pid), "-o", "args=",
        ], capture_output=True, text=True)
        if result.returncode != 0 and result.stderr.strip():
            raise RuntimeError("could not inspect the lock owner: " + result.stderr.strip())
        command = result.stdout.strip()
        if result.returncode == 0 and command:
            if command != executable and not command.startswith(executable + " "):
                raise RuntimeError("quit the separate Terminal/development companion first")
            print("Stopping the installed companion gracefully (PID {}).".format(pid))
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                # It exited after inspection; proceed with the managed job.
                pass
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError("companion has not exited; it was not force-killed")
    subprocess.run(["/bin/launchctl", "kickstart", service()], check=True)
    print("Started the configured USB job with its config and disconnect handling.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    stage = commands.add_parser("prepare", help="stage reviewable files in the workspace")
    stage.add_argument("--app", type=Path, required=True)
    stage.add_argument("--config", type=Path, required=True)
    stage.add_argument("--vid", type=lambda s: int(s, 0), required=True)
    stage.add_argument("--pid", type=lambda s: int(s, 0), required=True)
    stage.add_argument("--output", type=Path, required=True)
    stage.set_defaults(run=prepare)
    activate = commands.add_parser("install", help="install the prepared app and user launch agent")
    activate.add_argument("--prepared", type=Path, required=True)
    activate.set_defaults(run=install)
    remove = commands.add_parser("uninstall", help="disable USB activation and stop its managed instance")
    remove.set_defaults(run=uninstall)
    handover = commands.add_parser("restart", help="restart through launchd after granting app permissions")
    handover.set_defaults(run=restart)
    status = commands.add_parser("status", help="show launchd state without starting the companion")
    status.set_defaults(run=lambda _: subprocess.run(["/bin/launchctl", "print", service()]))
    args = parser.parse_args()
    if args.command == "prepare" and not (0 <= args.vid <= 65535 and 0 <= args.pid <= 65535):
        parser.error("VID/PID must fit in 16 bits")
    try:
        args.run(args)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, "Error: {}\n".format(error))


if __name__ == "__main__":
    main()
