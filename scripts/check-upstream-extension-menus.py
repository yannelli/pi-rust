"""Open the five upstream extension menus that require an interactive Pi session."""

import argparse
import errno
import fcntl
import json
import math
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path


MENUS = {
    "pi-btw": ("0.63.8", "Start side thread"),
    "pi-notes": ("0.2.6", "Open a note"),
    "pi-notify": ("0.4.4", "Destinations"),
    "pi-codex-imagegen": ("0.2.3", "Codex internal image endpoint"),
    "pi-chat": ("0.1.13", "Create private room"),
}
ANSI = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))")
READY = re.compile(r"menu-audit-ready=(quickjs|node-[\d.]+)!")
OBSERVER = """export default function (pi) {
    pi.on("resources_discover", (_event, ctx) => {
        const runtime = process.versions.quickjs ? "quickjs" : "node-" + process.versions.node;
        ctx.ui.setStatus("menu-audit", "menu-audit-ready=" + runtime + "!");
    });
}
"""


def wait_for_text(fd, pattern, timeout):
    deadline = time.monotonic() + timeout
    output = bytearray()
    text = ""
    while time.monotonic() < deadline:
        if not select.select([fd], [], [], max(0, deadline - time.monotonic()))[0]:
            break
        try:
            chunk = os.read(fd, 65536)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            break
        if not chunk:
            break
        output.extend(chunk)
        text = ANSI.sub(b"", output).decode(errors="replace")
        match = pattern.search(text)
        if match:
            return match
        del output[:-262144]
    tail = re.sub(r"\s+", " ", text).strip()[-750:]
    raise RuntimeError(f"Did not see {pattern.pattern!r}. Last terminal output: {tail}")


def wait_for_exit(pid, fd):
    deadline = time.monotonic() + 6
    while time.monotonic() < deadline:
        exited, status = os.waitpid(pid, os.WNOHANG)
        if exited:
            return os.waitstatus_to_exitcode(status)
        if select.select([fd], [], [], 0.05)[0]:
            try:
                os.read(fd, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
    raise RuntimeError("Pi did not exit after SIGTERM")


def check_menu(binary, prefixes, node, name, timeout):
    version, label = MENUS[name]
    package = prefixes / name / "node_modules" / "@narumitw" / name
    manifest = json.loads((package / "package.json").read_text())
    if (manifest["name"], manifest["version"]) != (f"@narumitw/{name}", version):
        raise ValueError(f"Expected @narumitw/{name}@{version} at {package}")
    with tempfile.TemporaryDirectory(prefix="pi-menu-audit-") as tmp:
        directory = Path(tmp).resolve()
        for subdir in ("work", "home", "agent", "tmp", "xdg", "observer"):
            (directory / subdir).mkdir()
        env = {
            "HOME": str(directory / "home"),
            "PATH": os.defpath,
            "TMPDIR": str(directory / "tmp"),
            "PI_CODING_AGENT_DIR": str(directory / "agent"),
            "PI_OFFLINE": "1",
            "XDG_CONFIG_HOME": str(directory / "xdg"),
            "XDG_DATA_HOME": str(directory / "xdg"),
            "XDG_CACHE_HOME": str(directory / "xdg"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "TERM": "xterm-256color",
            "LANG": "en_US.UTF-8",
        }
        model = {"baseUrl": "http://127.0.0.1:1/v1", "api": "openai-completions",
                 "apiKey": "offline-fixture", "models": [{"id": "offline", "name": "Offline"}]}
        (directory / "agent/models.json").write_text(json.dumps({"providers": {"audit": model}}))
        (directory / "agent/settings.json").write_text(json.dumps({"quietStartup": True}))
        observer = directory / "observer" / "observe.ts"
        (observer.parent / "package.json").write_text('{"private":true}')
        observer.write_text(OBSERVER)
        args = [str(binary), "--offline", "--no-session", "--no-approve", "--model", "audit/offline",
                "-e", str(package), "-e", str(observer)]
        expected = "quickjs"
        if name == "pi-chat":
            if node is None:
                raise ValueError("Chat needs Node 22 or later. Supply --node PATH.")
            found = subprocess.check_output([node, "--version"], env=env, text=True, timeout=5).strip()
            expected = "node-" + found.removeprefix("v")
            env["PI_NODE_PATH"] = str(node)
            args.extend(["--extension-runtime", "auto"])
        if sys.platform == "darwin":
            policy = "(version 1)(allow default)(deny network-outbound)"
            if name == "pi-chat":
                policy += '(allow network-outbound (remote ip "localhost:*"))'
            args = ["/usr/bin/sandbox-exec", "-p", policy, *args]
        pid, fd = pty.fork()
        if pid == 0:
            try:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 42, 140, 0, 0))
                os.chdir(directory / "work")
                os.execve(args[0], args, env)
            finally:
                os._exit(127)
        reaped = False
        try:
            engine = wait_for_text(fd, READY, timeout).group(1)
            if engine != expected:
                raise RuntimeError(f"Selected {engine}, expected {expected}")
            os.write(fd, ("/" + name.removeprefix("pi-") + "\r").encode())
            wait_for_text(fd, re.compile(re.escape(label)), timeout)
            os.kill(pid, signal.SIGTERM)
            code = wait_for_exit(pid, fd)
            reaped = True
            if code != 0:
                raise RuntimeError(f"Pi exited with status {code}")
        finally:
            if not reaped:
                try:
                    os.killpg(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                os.waitpid(pid, 0)
            os.close(fd)
        return f"{engine}, {label!r}"


def main():
    parser = argparse.ArgumentParser(description=__doc__, epilog=(
        "Each package needs its own npm prefix, PREFIXES/pi-NAME/node_modules/@narumitw/pi-NAME. "
        "Use the pinned versions in docs/parity/extension-packages.md. The tests use disposable "
        "homes, an idle local model, and a POSIX pseudo-terminal. On macOS only the Node backend "
        "can make outbound connections to loopback for its host bridge."
    ))
    parser.add_argument("binary", type=Path, help="pi binary to check")
    parser.add_argument("prefixes", type=Path, help="directory containing the installed package prefixes")
    parser.add_argument("--package", choices=MENUS, action="append", help="run only this menu (repeatable)")
    parser.add_argument("--node", type=Path, help="Node executable for Chat (defaults to PI_NODE_PATH, then PATH)")
    parser.add_argument("--timeout", type=float, default=15, help="maximum seconds per readiness or menu check")
    args = parser.parse_args()
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be a positive, finite number")
    node = args.node or os.environ.get("PI_NODE_PATH") or shutil.which("node")
    failed = False
    for name in args.package or MENUS:
        try:
            detail = check_menu(args.binary.resolve(), args.prefixes.resolve(), Path(node).resolve() if node else None,
                                name, args.timeout)
            print(f"PASS {name}@{MENUS[name][0]} ({detail})", flush=True)
        except (OSError, ValueError, KeyError, subprocess.SubprocessError, RuntimeError) as error:
            print(f"FAIL {name}: {error}", file=sys.stderr, flush=True)
            failed = True
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
