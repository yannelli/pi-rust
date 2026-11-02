"""`prepare <run dir>` checks sources and packages and builds a run copy; `verify <run dir>` checks a
finished run (loaded files, unchanged copy, at least 2 cases per member, identical runs). Neither starts Node."""

import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

PREP = Path(__file__).resolve().parent.parent
UPSTREAM = Path("/tmp/t17-upstream-git-35dfa")
PACKAGE_CACHE = Path("/tmp/t17-upstream")
COMMIT = "35dfa757378bd7a8570670e99c2687bdd1039a06"
NODE = Path("/Users/ryanyannelli/.nvm/versions/node/v22.23.3/bin/node")
TIMEOUT_SECONDS = 120


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def blob(path):
    data = Path(path).read_bytes()
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def git(*args):
    return subprocess.run(["git", "-C", str(UPSTREAM), *args], capture_output=True, text=True, check=True, timeout=30).stdout


def upstream_state():
    """HEAD, status, and the node_modules link of the protected checkout; none of them may change."""
    return {"head": git("rev-parse", "HEAD").strip(), "status": git("status", "--porcelain"),
            "nodeModulesLink": os.readlink(UPSTREAM / "node_modules")}


def check_packages(packages, root):
    """Every package file under `root` matches the manifest sha256 (copied from the read-only cache)."""
    for relative, digest in packages["files"].items():
        if sha256(root / relative) != digest:
            raise SystemExit(f"package mismatch: {root / relative}")


def run_copy_files(run_dir):
    return {str(path.relative_to(run_dir / "copy")): sha256(path)
            for path in sorted((run_dir / "copy").rglob("*")) if path.is_file()}


def check_sources(manifest):
    """Every prep upstream file matches the manifest and the 35dfa tree entry of that path."""
    for relative, entry in manifest["files"].items():
        path = PREP / "upstream" / relative
        tree = git("rev-parse", f"{COMMIT}:{relative}").strip()
        if blob(path) != entry["blob"] or entry["blob"] != tree or sha256(path) != entry["sha256"]:
            raise SystemExit(f"source mismatch: {relative}")


def prep_files():
    return {str(path.relative_to(PREP)): sha256(path) for path in sorted(PREP.rglob("*")) if path.is_file()}


def prepare(run_dir):
    run_dir = Path(run_dir)
    if run_dir.exists():
        raise SystemExit(f"{run_dir} exists; use a new run directory")
    manifest_path = PREP / "data" / "upstream-manifest.json"
    manifest = json.loads(manifest_path.read_text())
    packages = json.loads((PREP / "data" / "packages-manifest.json").read_text())
    before = upstream_state()
    if before["head"] != COMMIT or before["status"]:
        raise SystemExit("protected checkout is not a clean 35dfa757 tree")
    check_sources(manifest)
    check_packages(packages, PREP)
    check_packages(packages, PACKAGE_CACHE)
    run_dir.mkdir(parents=True)
    run_dir = run_dir.resolve()
    shutil.copytree(PREP / "upstream", run_dir / "copy")
    shutil.copytree(PREP / "node_modules", run_dir / "copy" / "node_modules")
    check_packages(packages, run_dir / "copy")
    for name in ["out-1", "out-2"]:
        (run_dir / name).mkdir()
    copied = run_copy_files(run_dir)
    if any(copied[rel] != entry["sha256"] for rel, entry in manifest["files"].items()):
        raise SystemExit("run copy differs from the manifest")
    env = {
        "PATH": f"{NODE.parent}:/usr/bin:/bin",
        "TZ": "UTC",
        "LANG": "C",
        "ORACLE_COPY": str(run_dir / "copy"),
        "ORACLE_DIR": str(PREP.resolve()),
    }
    commands = []
    for index in (1, 2):
        out = run_dir / f"out-{index}"
        for name in ["home", "agent"]:
            (out / name).mkdir()
        run_env = {**env, "HOME": str(out / "home"), "USERPROFILE": str(out / "home"),
                   "PI_CODING_AGENT_DIR": str(out / "agent"), "ORACLE_LOADED_LOG": str(out / "loaded.jsonl")}
        argv = [str(NODE), "--import", str(PREP.resolve() / "oracle" / "register.mjs"),
                str(PREP.resolve() / "oracle" / "run.mjs"), str(out / "work"), str(out / "golden.json")]
        commands.append({"env": run_env, "argv": argv, "cwd": str(out), "timeoutSeconds": TIMEOUT_SECONDS})
        (out / "work").mkdir()
    after = upstream_state()
    if before != after:
        raise SystemExit("upstream checkout changed during prepare")
    record = {"commit": COMMIT, "upstreamBefore": before, "upstreamAfter": after, "prepFiles": prep_files(),
              "runCopy": copied, "commands": commands}
    (run_dir / "run-inputs.json").write_text(json.dumps(record, indent=1) + "\n")
    print(json.dumps(commands, indent=1))


def verify(run_dir):
    run_dir = Path(run_dir).resolve()
    record = json.loads((run_dir / "run-inputs.json").read_text())
    if prep_files() != record["prepFiles"]:
        raise SystemExit("prep files changed after prepare")
    if upstream_state() != record["upstreamBefore"]:
        raise SystemExit("upstream checkout changed")
    if run_copy_files(run_dir) != record["runCopy"]:
        raise SystemExit("run copy changed during the run")
    packages = json.loads((PREP / "data" / "packages-manifest.json").read_text())["files"]
    manifest = json.loads((PREP / "data" / "upstream-manifest.json").read_text())["files"]
    outputs = []
    for index in (1, 2):
        out = run_dir / f"out-{index}"
        loaded = [json.loads(line) for line in (out / "loaded.jsonl").read_text().splitlines()]
        files = {item["file"] for item in loaded}
        if "packages/coding-agent/src/core/settings-manager.ts" not in files:
            raise SystemExit("settings-manager.ts was not loaded from the run copy")
        known = {**{rel: entry["sha256"] for rel, entry in manifest.items()}, **packages}
        if any(known.get(item["file"]) != item["sha256"] for item in loaded):
            raise SystemExit("a loaded file is not a manifest file")
        golden = json.loads((out / "golden.json").read_text())
        if any(rel not in packages for rel in golden["commonJsLoaded"]):
            raise SystemExit("a CommonJS module is not a manifest package file")
        text = (out / "golden.json").read_text()
        coverage = json.loads((out / "golden.json.coverage.json").read_text())
        empty = [name for name, ids in coverage.items() if len(set(ids)) < 2]
        if len(coverage) != 139 or empty:
            raise SystemExit(f"coverage incomplete: {empty}")
        outputs.append(text)
    if outputs[0] != outputs[1]:
        raise SystemExit("the two runs differ")
    summary = {"goldenSha256": sha256(run_dir / "out-1" / "golden.json"),
               "coverageSha256": sha256(run_dir / "out-1" / "golden.json.coverage.json"),
               "loadedFiles": sorted(files), "identicalRuns": True}
    (run_dir / "verify.json").write_text(json.dumps(summary, indent=1) + "\n")
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    if len(sys.argv) != 3 or sys.argv[1] not in {"prepare", "verify"}:
        raise SystemExit("usage: prepare_run.py prepare|verify <run dir>")
    {"prepare": prepare, "verify": verify}[sys.argv[1]](sys.argv[2])
