"""Bounded integration command transport. No Foundry store files are read."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

spec = json.loads(sys.argv[1])
result = {"passed": False, "exit_code": None, "report": "", "artifacts": []}
workspace = None


def run(args, cwd=None, timeout=120):
    completed = subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    if completed.returncode:
        raise RuntimeError("{} exited {}: {}".format(args[0], completed.returncode, completed.stderr[-4096:]))
    return completed.stdout


def git(path, *args):
    return run(["git", "-C", str(path), *args]).strip()


def current(entry):
    path = Path(entry["path"]).expanduser().resolve(strict=True)
    branch = entry["branch"]
    if entry.get("skip"):
        raise RuntimeError("registered project is blocked: " + entry["name"])
    if git(path, "status", "--porcelain", "--untracked-files=all"):
        raise RuntimeError("registered trunk is dirty: " + entry["name"])
    if git(path, "symbolic-ref", "--short", "HEAD") != branch:
        raise RuntimeError("registered checkout is not on its trunk: " + entry["name"])
    git(path, "fetch", "origin")
    git(path, "merge", "--ff-only", "origin/" + branch)
    sha = git(path, "rev-parse", "HEAD")
    if sha != git(path, "rev-parse", "origin/" + branch):
        raise RuntimeError("registered trunk is ahead of origin: " + entry["name"])
    expected = spec.get("expected", {}).get(entry["name"])
    if expected and sha != expected:
        raise RuntimeError("host trunk differs from steering trunk: " + entry["name"])
    return path, sha


def remote_idle(entries):
    if spec.get("registry") is not None:
        return
    pacing = json.loads(run(["foundry", "--addr", "http://127.0.0.1:50051", "pacing", "show", "--json"]))
    projects = set(spec["projects"])
    repositories = {entry.get("repo") or entry["name"] for entry in entries if entry["name"] in projects}
    for item in pacing["running_items"]:
        if item["project"] in projects or item["repository"] in repositories or item["project"] == "system":
            raise RuntimeError("gate host has an active execution or cleanup owner: " + item["id"])


try:
    registry = spec.get("registry")
    if registry is None:
        registry = json.loads(run(["foundry", "--addr", "http://127.0.0.1:50051", "registry", "list", "--json"]))
    entries = registry if isinstance(registry, list) else registry["projects"]
    remote_idle(entries)
    selected = []
    identities = {}
    for name in spec["projects"]:
        if Path(name).name != name or name in (".", ".."):
            raise RuntimeError("invalid registered project name")
        entry = next((e for e in entries if e["name"] == name), None)
        if entry is None:
            raise RuntimeError("project is not registered on gate host: " + name)
        path, sha = current(entry)
        selected.append((name, path, sha))
        identities[name] = sha
    result["report"] = "Current trunks: " + json.dumps(identities, sort_keys=True)
    if spec["mode"] == "sync":
        result["passed"] = True
        result["exit_code"] = 0
    else:
        evidence_root = Path(spec.get("evidence_root") or Path.home() / ".foundry" / "initiatives" / "integration-evidence")
        evidence = evidence_root / spec["id"]
        evidence.mkdir(parents=True, exist_ok=False)
        workspace = Path(tempfile.mkdtemp(prefix="foundry-integration-")).resolve()
        for name, path, sha in selected:
            copy = workspace / name
            run(["git", "clone", "--quiet", "--no-hardlinks", "--no-checkout", str(path), str(copy)])
            git(copy, "checkout", "--quiet", "--detach", sha)
            # A disposable checkout must never push to the registered repository.
            git(copy, "remote", "remove", "origin")
        artifact_paths = []
        for artifact in spec["artifacts"]:
            relative = Path(artifact)
            if relative.is_absolute() or ".." in relative.parts:
                raise RuntimeError("artifact must be relative to integration root: " + artifact)
            path = (workspace / relative).resolve(strict=True)
            if not path.is_relative_to(workspace):
                raise RuntimeError("artifact escapes disposable integration root: " + artifact)
            artifact_paths.append((artifact, path))
        capture = subprocess.run(["foundry", "capture", "--log-dir", str(evidence), "--", "sh", "-c", spec["command"]], cwd=workspace, capture_output=True, text=True, timeout=2400)
        result["exit_code"] = capture.returncode
        capture_text = capture.stdout + capture.stderr
        result["report"] += "\nCapture evidence: " + str(evidence) + "\n" + capture_text[-8192:]
        expected_line = "sh: exit {}".format(capture.returncode)
        capture_proven = capture_text.startswith(expected_line + "\n") and any(evidence.glob("*.stdout.log")) and any(evidence.glob("*.stderr.log"))
        artifact_proven = all(path.exists() and path.resolve().is_relative_to(workspace) for _, path in artifact_paths)
        for _, path, _ in selected:
            git(path, "fetch", "origin")
        remote_idle(entries)
        unchanged = all(git(path, "rev-parse", "HEAD") == sha and not git(path, "status", "--porcelain", "--untracked-files=all") and git(path, "rev-parse", "origin/" + next(e["branch"] for e in entries if e["name"] == name)) == sha for name, path, sha in selected)
        result["passed"] = capture.returncode == 0 and capture_proven and artifact_proven and unchanged
        if not capture_proven:
            result["report"] += "\nCapture exit/report/log proof is missing."
        if not artifact_proven:
            result["report"] += "\nRequired artifacts disappeared or escaped the workspace."
        if not unchanged:
            result["report"] += "\nStale: registered trunks changed during integration."
        result["artifacts"] = [artifact for artifact, _ in artifact_paths if artifact_proven]
        (evidence / "result.json").write_text(json.dumps(result, indent=2))
except Exception as error:
    result["passed"] = False
    result["report"] += "\n" + str(error)
finally:
    if workspace is not None:
        shutil.rmtree(workspace)
print(json.dumps(result))
