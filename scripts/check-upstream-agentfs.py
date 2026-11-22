#!/usr/bin/env python3
"""Exercise the published AgentFS package through pi RPC and a loopback model."""

import argparse
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time


FILE = "audit/roundtrip.txt"
TOOLS = (
    ("write", {"path": FILE, "content": "first value"}),
    ("read", {"path": FILE}),
    ("edit", {"path": FILE, "edits": [{"oldText": "first", "newText": "updated"}]}),
    ("read", {"path": FILE}),
    ("bash", {"command": "cat /" + FILE}),
)
GUARD = "AgentFS blocks ! and !! host commands; use the bash tool instead"
OBSERVER = """export default function (pi) {
    pi.registerCommand("audit-agentfs", { handler: (_, ctx) => {
        const tools = pi.getAllTools().filter(t => ["read", "write", "edit", "bash"].includes(t.name));
        ctx.ui.notify(JSON.stringify({ node: process.versions.node, quickjs: process.versions.quickjs,
            tools: tools.map(t => [t.name, t.sourceInfo?.path]) }));
    } });
}"""


def check(condition, failure):
    if not condition:
        raise ValueError(failure)


class MockModel(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        if self.path != "/v1/chat/completions" or self.server.calls > len(TOOLS):
            return self.send_error(400, "Unexpected model request")
        self.rfile.read(int(self.headers["Content-Length"]))
        index = self.server.calls
        self.server.calls += 1
        if index < len(TOOLS):
            name, args = TOOLS[index]
            tool = {"name": name, "arguments": json.dumps(args)}
            delta = {"tool_calls": [{"index": 0, "id": f"audit{index}", "type": "function", "function": tool}]}
            finish = "tool_calls"
        else:
            delta = {"content": "Audit complete"}
            finish = "stop"
        chunks = [{"index": 0, "delta": delta}, {"index": 0, "delta": {}, "finish_reason": finish}]
        body = "".join("data: " + json.dumps({"choices": [chunk]}) + "\n\n" for chunk in chunks)
        body = (body + "data: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def exercise(pi, node_version, entry):
    deadline = time.monotonic() + 100
    lines = queue.Queue()
    records = []

    def read_stdout():
        try:
            for line in pi.stdout:
                lines.put(line)
        finally:
            lines.put(None)

    threading.Thread(target=read_stdout, daemon=True).start()

    def until(matches, allow_eof=False):
        while True:
            try:
                line = lines.get(timeout=max(0, deadline - time.monotonic()))
            except queue.Empty:
                raise TimeoutError("Timed out waiting for pi RPC") from None
            if line is None:
                if allow_eof:
                    return
                raise ValueError(f"pi closed RPC early (exit {pi.poll()})")
            record = json.loads(line)
            records.append(record)
            if record.get("notifyType") == "error":
                raise ValueError(record.get("message"))
            if matches(record):
                return record

    def request(key, rpc_type, **args):
        pi.stdin.write(json.dumps({"id": key, "type": rpc_type, **args}) + "\n")
        pi.stdin.flush()
        result = until(lambda r: r.get("type") == "response" and r.get("id") == key)
        check(result["success"], result)
        return result["data"]

    status = request("sources", "prompt", message="/audit-agentfs")
    check(status["agentInvoked"] is False, status)
    notices = [r["message"] for r in records if r.get("method") == "notify"]
    observed = next((json.loads(n) for n in notices if n.startswith('{"node":')), None)
    check(observed and observed["node"] == node_version and not observed.get("quickjs"),
          f"Auto runtime did not select Node {node_version}: {notices}")
    sources = sorted((name, str(Path(source).resolve())) for name, source in observed["tools"])
    expected = sorted((name, str(entry)) for name in {"read", "write", "edit", "bash"})
    check(sources == expected, f"Unexpected tool sources: {sources}")
    guard = request("guard", "bash", command="echo audit", excludeFromContext=True)
    check(guard["exitCode"] == 1 and guard["output"] == GUARD, guard)
    started = request("flow", "prompt", message="Exercise the AgentFS tools")
    check(started["agentInvoked"] is True, started)
    until(lambda r: r.get("type") == "agent_end")
    pi.stdin.close()
    until(lambda _: False, allow_eof=True)
    pi.wait(timeout=8)
    check(pi.returncode == 0, f"pi exited with {pi.returncode}")

    errors = [r for r in records if r.get("type") == "extension_error"]
    check(len(errors) == 1 and errors[0]["event"] == "user_bash" and errors[0]["error"] == GUARD
          and Path(errors[0]["extensionPath"]).resolve() == entry, f"Unexpected extension errors: {errors}")
    results = [r for r in records if r.get("type") == "tool_execution_end"]
    check([(r["toolName"], r["isError"]) for r in results] == [(name, False) for name, _ in TOOLS], results)
    write, first_read, edit, second_read, bash = [r["result"] for r in results]
    check(write["content"][0]["text"] == f"Successfully wrote 11 bytes to /{FILE}", write)
    check(first_read["structuredContent"] == "first value", first_read)
    check(edit["details"]["diff"] == "-1 first value\n+1 updated value", edit)
    check(second_read["structuredContent"] == "updated value", second_read)
    check(bash["structuredContent"] == {"output": "updated value", "exit_code": 0, "truncated": False}, bash)


def probe(binary, packages, node, node_version, directory, server):
    package = packages / "pi-agentfs/node_modules/@narumitw/pi-agentfs"
    manifest = json.loads((package / "package.json").read_text())
    check((manifest["name"], manifest["version"]) == ("@narumitw/pi-agentfs", "0.2.6"), "Expected pi-agentfs@0.2.6")
    [entry] = [(package / path).resolve(strict=True) for path in manifest["pi"]["extensions"]]
    agent, work = directory / "agent", directory / "work"
    agent.mkdir()
    work.mkdir()
    model = {"baseUrl": f"http://127.0.0.1:{server.server_port}/v1", "api": "openai-completions",
             "apiKey": "offline-fixture", "models": [{"id": "offline"}]}
    (agent / "models.json").write_text(json.dumps({"providers": {"audit": model}}))
    observer, database = directory / "observe.ts", directory / "workspace.db"
    observer.write_text(OBSERVER)
    env = {k: str(directory) for k in ("HOME", "USERPROFILE", "TMPDIR", "TMP", "TEMP", "XDG_CONFIG_HOME",
                                        "XDG_CACHE_HOME", "XDG_DATA_HOME")}
    env.update(PATH=os.environ["PATH"], PI_NODE_PATH=node, PI_CODING_AGENT_DIR=str(agent), PI_OFFLINE="1",
               GIT_CONFIG_NOSYSTEM="1", TERM="dumb")
    if os.name == "nt":
        env["SystemRoot"] = os.environ["SystemRoot"]
    args = [binary, "--mode", "rpc", "--offline", "--no-session", "--no-approve", "--model", "audit/offline",
            "--extension-runtime", "auto", "-e", package, "-e", observer, "--agentfs-path", database]
    with tempfile.TemporaryFile("w+", encoding="utf-8") as stderr:
        with subprocess.Popen(args, cwd=work, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=stderr, text=True, encoding="utf-8", start_new_session=os.name == "posix") as pi:
            try:
                exercise(pi, node_version, entry)
            except BaseException:
                if pi.poll() is None:
                    if os.name == "posix":
                        os.killpg(pi.pid, signal.SIGKILL)
                    else:
                        pi.kill()
                    pi.wait(timeout=5)
                stderr.seek(0)
                sys.stderr.write(stderr.read(4000))
                raise
    check(server.calls == len(TOOLS) + 1, f"Unexpected model request count: {server.calls}")
    check(not os.path.lexists(work / FILE), f"Found a host file at {work / FILE}")
    with closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=2)) as db:
        contents = db.execute("""
            SELECT data.data FROM fs_dentry dir
            JOIN fs_dentry file ON file.parent_ino = dir.ino
            JOIN fs_data data ON data.ino = file.ino
            WHERE dir.parent_ino = 1 AND dir.name = 'audit' AND file.name = 'roundtrip.txt'
            ORDER BY data.chunk_index
        """).fetchall()
        check(b"".join(row[0] for row in contents) == b"updated value", contents)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pi", type=Path)
    parser.add_argument("package_prefixes", type=Path)
    args = parser.parse_args()
    node = shutil.which(os.environ.get("PI_NODE_PATH") or "node")
    check(node, "Set PI_NODE_PATH or add Node 22+ to PATH")
    version = subprocess.check_output([node, "-p", "process.versions.node"], text=True, timeout=5).strip()
    check(int(version.split(".")[0]) >= 22, f"Node 22+ required, found {version}")
    with tempfile.TemporaryDirectory(prefix="pi-agentfs-") as directory:
        with ThreadingHTTPServer(("127.0.0.1", 0), MockModel) as server:
            server.calls = 0
            threading.Thread(target=server.serve_forever, daemon=True).start()
            try:
                probe(args.pi.resolve(strict=True), args.package_prefixes.resolve(strict=True), node,
                      version, Path(directory).resolve(), server)
            finally:
                server.shutdown()
    print(f"PASS pi-agentfs@0.2.6 on Node {version} (auto). Five tool results, SQLite contents and host guard verified.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError, sqlite3.Error) as error:
        sys.exit(f"FAIL pi-agentfs: {error}")
