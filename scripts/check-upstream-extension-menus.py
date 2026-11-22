"""Open the five upstream extension menus that require an interactive Pi session."""

import argparse
import errno
import json
import math
import os
import queue
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

if os.name == "nt":
    import ctypes
    from ctypes import wintypes
else:
    import fcntl
    import pty
    import select
    import struct
    import termios


MENUS = {
    "pi-btw": ("0.63.8", "Start side thread"),
    "pi-notes": ("0.2.6", "Open a note"),
    "pi-notify": ("0.4.4", "Destinations"),
    "pi-codex-imagegen": ("0.2.3", "Codex internal image endpoint"),
    "pi-chat": ("0.1.13", "Create private room"),
}
COLUMNS, ROWS = 140, 42
ANSI = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))")
READY = re.compile(r"menu-audit-ready=(quickjs|node-[\d.]+)!")
OBSERVER = """export default function (pi) {
    pi.on("resources_discover", (_event, ctx) => {
        const runtime = process.versions.quickjs ? "quickjs" : "node-" + process.versions.node;
        ctx.ui.setStatus("menu-audit", "menu-audit-ready=" + runtime + "!");
    });
}
"""


def executable(path):
    """`path`, or `path.exe` on Windows when the suffix was left out."""
    windows_binary = path.with_name(path.name + ".exe")
    return windows_binary if os.name == "nt" and not path.exists() and windows_binary.exists() else path


def wait_for_text(terminal, pattern, timeout):
    deadline = time.monotonic() + timeout
    output = bytearray()
    text = ""
    while time.monotonic() < deadline:
        chunk = terminal.read(max(0, deadline - time.monotonic()))
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


class PosixTerminal:
    """Pi on a POSIX pseudoterminal. `stop` sends SIGTERM."""

    def __init__(self, args, cwd, env):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            try:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLUMNS, 0, 0))
                os.chdir(cwd)
                os.execve(args[0], args, env)
            finally:
                os._exit(127)
        self.reaped = False

    def read(self, timeout):
        """Output bytes, `b""` after `timeout`, or `None` at end of output."""
        if not select.select([self.fd], [], [], timeout)[0]:
            return b""
        try:
            return os.read(self.fd, 65536) or None
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            return None

    def write(self, data):
        os.write(self.fd, data)

    def stop(self):
        os.kill(self.pid, signal.SIGTERM)
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            exited, status = os.waitpid(self.pid, os.WNOHANG)
            if exited:
                self.reaped = True
                return os.waitstatus_to_exitcode(status)
            self.read(0.05)
        raise RuntimeError("Pi did not exit after SIGTERM")

    def close(self):
        if not self.reaped:
            try:
                os.killpg(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(self.pid, 0)
        os.close(self.fd)


class WindowsTerminal:
    """Pi on a Windows pseudoconsole. `stop` closes the menu and enters `/quit`."""

    def __init__(self, args, cwd, env):
        k32 = self.k32 = ctypes.WinDLL("kernel32", use_last_error=True)
        HANDLE, DWORD, BOOL = wintypes.HANDLE, wintypes.DWORD, wintypes.BOOL
        PHANDLE, PDWORD, PVOID = ctypes.POINTER(HANDLE), ctypes.POINTER(DWORD), ctypes.c_void_p

        class COORD(ctypes.Structure):
            _fields_ = [("X", wintypes.SHORT), ("Y", wintypes.SHORT)]

        class STARTUPINFOEXW(ctypes.Structure):
            _fields_ = [("cb", DWORD), ("lpReserved", wintypes.LPWSTR), ("lpDesktop", wintypes.LPWSTR),
                        ("lpTitle", wintypes.LPWSTR)] + [(name, DWORD) for name in (
                            "dwX", "dwY", "dwXSize", "dwYSize", "dwXCountChars", "dwYCountChars", "dwFillAttribute",
                            "dwFlags")] + [("wShowWindow", wintypes.WORD), ("cbReserved2", wintypes.WORD),
                                           ("lpReserved2", PVOID), ("hStdInput", HANDLE), ("hStdOutput", HANDLE),
                                           ("hStdError", HANDLE), ("lpAttributeList", PVOID)]

        class PROCESS_INFORMATION(ctypes.Structure):
            _fields_ = [("hProcess", HANDLE), ("hThread", HANDLE), ("dwProcessId", DWORD), ("dwThreadId", DWORD)]

        for name, restype, argtypes in (
            ("CreatePipe", BOOL, [PHANDLE, PHANDLE, PVOID, DWORD]),
            ("CreatePseudoConsole", ctypes.c_long, [COORD, HANDLE, HANDLE, DWORD, PHANDLE]),
            ("ClosePseudoConsole", None, [HANDLE]),
            ("InitializeProcThreadAttributeList", BOOL, [PVOID, DWORD, DWORD, ctypes.POINTER(ctypes.c_size_t)]),
            ("UpdateProcThreadAttribute", BOOL, [PVOID, DWORD, ctypes.c_size_t, PVOID, ctypes.c_size_t, PVOID, PVOID]),
            ("DeleteProcThreadAttributeList", None, [PVOID]),
            ("CreateProcessW", BOOL, [wintypes.LPCWSTR, wintypes.LPWSTR, PVOID, PVOID, BOOL, DWORD, PVOID,
                                      wintypes.LPCWSTR, ctypes.POINTER(STARTUPINFOEXW),
                                      ctypes.POINTER(PROCESS_INFORMATION)]),
            ("ReadFile", BOOL, [HANDLE, PVOID, DWORD, PDWORD, PVOID]),
            ("WriteFile", BOOL, [HANDLE, PVOID, DWORD, PDWORD, PVOID]),
            ("WaitForSingleObject", DWORD, [HANDLE, DWORD]),
            ("GetExitCodeProcess", BOOL, [HANDLE, PDWORD]),
            ("CloseHandle", BOOL, [HANDLE]),
        ):
            function = getattr(k32, name)
            function.restype, function.argtypes = restype, argtypes

        def check(succeeded, name):
            if not succeeded:
                raise ctypes.WinError(ctypes.get_last_error(), name)

        console_in, self.input, self.output, console_out = HANDLE(), HANDLE(), HANDLE(), HANDLE()
        check(k32.CreatePipe(ctypes.byref(console_in), ctypes.byref(self.input), None, 0), "CreatePipe")
        check(k32.CreatePipe(ctypes.byref(self.output), ctypes.byref(console_out), None, 0), "CreatePipe")
        self.console = HANDLE()
        result = k32.CreatePseudoConsole(COORD(COLUMNS, ROWS), console_in, console_out, 0, ctypes.byref(self.console))
        k32.CloseHandle(console_in)
        k32.CloseHandle(console_out)
        if result != 0:
            raise OSError(f"CreatePseudoConsole failed with HRESULT {result & 0xFFFFFFFF:#010x}")

        size = ctypes.c_size_t()
        k32.InitializeProcThreadAttributeList(None, 1, 0, ctypes.byref(size))
        attributes = ctypes.create_string_buffer(size.value)
        check(k32.InitializeProcThreadAttributeList(attributes, 1, 0, ctypes.byref(size)), "InitializeProcThreadAttributeList")
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE = 0x00020016
        check(k32.UpdateProcThreadAttribute(attributes, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, self.console,
                                            ctypes.sizeof(HANDLE), None, None), "UpdateProcThreadAttribute")
        startup = STARTUPINFOEXW(cb=ctypes.sizeof(STARTUPINFOEXW), lpAttributeList=ctypes.addressof(attributes))
        # Without null standard handles the child inherits this script's redirected handles
        # instead of the pseudoconsole.
        startup.dwFlags = 0x00000100  # STARTF_USESTDHANDLES
        process = PROCESS_INFORMATION()
        environment = ctypes.create_unicode_buffer("".join(f"{key}={value}\0" for key, value in sorted(env.items())))
        EXTENDED_STARTUPINFO_PRESENT, CREATE_UNICODE_ENVIRONMENT = 0x00080000, 0x00000400
        try:
            check(k32.CreateProcessW(None, subprocess.list2cmdline(args), None, None, False,
                                     EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT, environment,
                                     str(cwd), ctypes.byref(startup), ctypes.byref(process)), "CreateProcessW")
        finally:
            k32.DeleteProcThreadAttributeList(attributes)
        k32.CloseHandle(process.hThread)
        self.process, self.pid, self.exit_code = process.hProcess, process.dwProcessId, None
        self.chunks, self.ended = queue.Queue(), False
        self.reader = threading.Thread(target=self._pump, daemon=True)
        self.reader.start()

    def _pump(self):
        buffer, count = ctypes.create_string_buffer(65536), wintypes.DWORD()
        while self.k32.ReadFile(self.output, buffer, len(buffer), ctypes.byref(count), None) and count.value:
            self.chunks.put(buffer.raw[:count.value])
        self.chunks.put(None)

    def read(self, timeout):
        """Output bytes, `b""` after `timeout`, or `None` at end of output."""
        if self.ended:
            return None
        try:
            chunk = self.chunks.get(timeout=timeout)
        except queue.Empty:
            return b""
        self.ended = chunk is None
        return chunk

    def write(self, data):
        written = wintypes.DWORD()
        if not self.k32.WriteFile(self.input, data, len(data), ctypes.byref(written), None) or written.value != len(data):
            raise ctypes.WinError(ctypes.get_last_error(), "WriteFile")

    def _wait(self, timeout):
        if self.k32.WaitForSingleObject(self.process, int(timeout * 1000)) != 0:
            return None
        code = wintypes.DWORD()
        self.k32.GetExitCodeProcess(self.process, ctypes.byref(code))
        self.exit_code = code.value
        return self.exit_code

    def stop(self):
        # Windows has no SIGTERM for a pseudoconsole client, so leave the menu and quit.
        self.write(b"\x1b")
        time.sleep(0.5)
        self.write(b"/quit\r")
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            code = self._wait(0.05)
            if code is not None:
                return code
            self.read(0)
        raise RuntimeError("Pi did not exit after /quit")

    def close(self):
        if self.exit_code is None and self._wait(0) is None:
            # Also end the Node extension host that Pi started.
            taskkill = Path(os.environ["SystemRoot"]) / "System32" / "taskkill.exe"
            subprocess.run([str(taskkill), "/F", "/T", "/PID", str(self.pid)], capture_output=True, timeout=10)
            self._wait(5)
        self.k32.ClosePseudoConsole(self.console)
        self.reader.join(timeout=5)
        for handle in (self.input, self.output, self.process):
            self.k32.CloseHandle(handle)


def check_menu(binary, prefixes, node, name, timeout):
    version, label = MENUS[name]
    package = prefixes / name / "node_modules" / "@narumitw" / name
    manifest = json.loads((package / "package.json").read_text(encoding="utf-8"))
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
        if os.name == "nt":
            system_root = os.environ["SystemRoot"]
            env.update({
                "PATH": os.pathsep.join([str(Path(system_root) / "System32"), system_root]),
                "PATHEXT": os.environ.get("PATHEXT", ".COM;.EXE;.BAT;.CMD"),
                "SystemRoot": system_root,
                "USERPROFILE": str(directory / "home"),
                "APPDATA": str(directory / "home" / "AppData" / "Roaming"),
                "LOCALAPPDATA": str(directory / "home" / "AppData" / "Local"),
                "TEMP": str(directory / "tmp"),
                "TMP": str(directory / "tmp"),
            })
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
            found = subprocess.check_output([node, "--version"], env=env, text=True, timeout=30).strip()
            expected = "node-" + found.removeprefix("v")
            env["PI_NODE_PATH"] = str(node)
            args.extend(["--extension-runtime", "auto"])
        if sys.platform == "darwin":
            policy = "(version 1)(allow default)(deny network-outbound)"
            if name == "pi-chat":
                policy += '(allow network-outbound (remote ip "localhost:*"))'
            args = ["/usr/bin/sandbox-exec", "-p", policy, *args]
        terminal = (WindowsTerminal if os.name == "nt" else PosixTerminal)(args, directory / "work", env)
        try:
            engine = wait_for_text(terminal, READY, timeout).group(1)
            if engine != expected:
                raise RuntimeError(f"Selected {engine}, expected {expected}")
            terminal.write(("/" + name.removeprefix("pi-") + "\r").encode())
            wait_for_text(terminal, re.compile(re.escape(label)), timeout)
            code = terminal.stop()
            if code != 0:
                raise RuntimeError(f"Pi exited with status {code}")
        finally:
            terminal.close()
        return f"{engine}, {label!r}"


def main():
    parser = argparse.ArgumentParser(description=__doc__, epilog=(
        "Each package needs its own npm prefix, PREFIXES/pi-NAME/node_modules/@narumitw/pi-NAME. "
        "Use the pinned versions in docs/parity/extension-packages.md. The tests use disposable "
        "homes, an idle local model, and a POSIX pseudoterminal or a Windows pseudoconsole. On macOS "
        "only the Node backend can make outbound connections to loopback for its host bridge."
    ))
    parser.add_argument("binary", type=Path, help="pi binary to check")
    parser.add_argument("prefixes", type=Path, help="directory containing the installed package prefixes")
    parser.add_argument("--package", choices=MENUS, action="append", help="run only this menu (repeatable)")
    parser.add_argument("--node", type=Path, help="Node executable for Chat (defaults to PI_NODE_PATH, then PATH)")
    parser.add_argument("--timeout", type=float, default=60, help="maximum seconds per readiness or menu check")
    args = parser.parse_args()
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be a positive, finite number")
    node = args.node or os.environ.get("PI_NODE_PATH") or shutil.which("node")
    failed = False
    for name in args.package or MENUS:
        try:
            detail = check_menu(executable(args.binary).resolve(), args.prefixes.resolve(), Path(node).resolve() if node else None,
                                name, args.timeout)
            print(f"PASS {name}@{MENUS[name][0]} ({detail})", flush=True)
        except (OSError, ValueError, KeyError, subprocess.SubprocessError, RuntimeError) as error:
            print(f"FAIL {name}: {error}", file=sys.stderr, flush=True)
            failed = True
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
