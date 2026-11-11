# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Client-side pre-commit hook (`.lorehooks.toml`), driven through the real CLI."""

import json
import os
import subprocess
import sys
import time

import pytest
from error_types import LoreException
from lore_parsers import parse_jsonl

from lore import Lore

HOOK_REJECTED_EXIT_CODE = 100

HOOK_PY = """\
import os, shutil, sys, time
here = os.path.dirname(os.path.abspath(__file__))
tmp = os.environ["HOOK_TEST_DIR"]
shutil.copyfile(os.environ["LORE_HOOK_PAYLOAD"], os.path.join(tmp, "captured.json"))
mode = open(os.path.join(tmp, "behaviour")).read().strip()
if mode.startswith("sleep"):
    time.sleep(30)
if mode == "garbage":
    print("garbage")
sys.exit(1 if mode == "reject" else 0)
"""


def _toml_str(value: str) -> str:
    return json.dumps(value)


class HookRepo:
    def __init__(self, repo: Lore, tmp_dir: str):
        self.repo = repo
        self.tmp_dir = tmp_dir

    def behaviour(self, mode: str):
        with open(os.path.join(self.tmp_dir, "behaviour"), "w") as f:
            f.write(mode)

    @property
    def captured_path(self) -> str:
        return os.path.join(self.tmp_dir, "captured.json")

    def captured(self) -> dict:
        with open(self.captured_path, encoding="utf-8") as f:
            return json.load(f)

    def write_hooks(self, extra: str = "", command: list[str] | None = None):
        command = command or [sys.executable, "hook.py"]
        argv = ", ".join(_toml_str(c) for c in command)
        self.repo.write_files(
            {
                ".lorehooks.toml": f"version = 1\n[pre_commit]\ncommand = [{argv}]\n{extra}"
            }
        )


@pytest.fixture
def hook_repo(new_lore_repo, tmp_path):
    tmp_dir = str(tmp_path / "hookdata")
    os.makedirs(tmp_dir)
    repo: Lore = new_lore_repo(
        environment_vars={"LORE_TRUST_HOOKS": "1", "HOOK_TEST_DIR": tmp_dir}
    )
    hr = HookRepo(repo, tmp_dir)
    hr.behaviour("pass")
    repo.write_files({"hook.py": HOOK_PY})
    hr.write_hooks()
    repo.write_files(
        {"modify.txt": "one\n", "delete.txt": "gone\n", "old.txt": "old\n"}
    )
    repo.stage(scan=True, offline=True)
    repo.commit("setup", offline=True, no_verify=True)
    os.remove(hr.captured_path) if os.path.exists(hr.captured_path) else None
    return hr


def _stage_changes(repo: Lore):
    repo.write_files({"added.txt": "new\n", "modify.txt": "two\n"})
    repo.remove_file("delete.txt")
    repo.stage(scan=True, offline=True)
    # Staged after the scan, which would otherwise see the rename as an add and a delete.
    repo.move("old.txt", "moved.txt")
    repo.file_stage_move("old.txt", "moved.txt", offline=True)


def _history_len(repo: Lore) -> int:
    return len(repo.history(oneline=True, offline=True))


def test_hook_receives_payload(hook_repo: HookRepo):
    repo = hook_repo.repo
    _stage_changes(repo)
    repo.commit("GAME-1 x\n\nzażółć gęślą jaźń", offline=True)

    p = hook_repo.captured()
    assert p["version"] == 1 and p["hook"] == "pre-commit"
    assert p["operation"] == "commit"
    assert p["message"] == "GAME-1 x\n\nzażółć gęślą jaźń"
    assert p["branch"]
    assert isinstance(p["revision_number"], int)
    assert os.path.normcase(p["repository_root"]) == os.path.normcase(
        os.path.abspath(repo.path)
    )
    files = {f["path"]: f for f in p["files"]}
    assert files["added.txt"]["action"] == "add"
    assert files["modify.txt"]["action"] == "modify"
    assert files["delete.txt"]["action"] == "delete"
    assert files["moved.txt"]["action"] == "move"
    assert files["moved.txt"]["from_path"] == "old.txt"
    assert files["added.txt"]["from_path"] is None
    assert [f["path"] for f in p["files"]] == sorted(files)
    assert p["merge"] is False and p["dry_run"] is False


def test_hook_rejects(hook_repo: HookRepo):
    repo = hook_repo.repo
    hook_repo.behaviour("reject")
    _stage_changes(repo)
    before = _history_len(repo)
    with pytest.raises(LoreException) as exc:
        repo.commit("no ticket", offline=True)
    assert exc.value.returncode == HOOK_REJECTED_EXIT_CODE
    assert _history_len(repo) == before
    status = repo.status(offline=True)
    assert "added.txt" in status


def test_no_verify_flag(hook_repo: HookRepo):
    repo = hook_repo.repo
    hook_repo.behaviour("reject")
    _stage_changes(repo)
    repo.commit("bypass", offline=True, no_verify=True)
    assert not os.path.exists(hook_repo.captured_path)


def test_no_verify_env(hook_repo: HookRepo, new_lore_repo):
    repo = hook_repo.repo
    hook_repo.behaviour("reject")
    _stage_changes(repo)
    repo.environment_vars["LORE_NO_VERIFY"] = "1"
    repo.commit("bypass", offline=True)
    assert not os.path.exists(hook_repo.captured_path)


def test_untrusted_repo_warns_and_commits(hook_repo: HookRepo):
    repo = hook_repo.repo
    hook_repo.behaviour("reject")
    _stage_changes(repo)
    repo.environment_vars["LORE_TRUST_HOOKS"] = "0"
    output = repo.commit("untrusted", offline=True)
    assert "not trusted" in output
    assert not os.path.exists(hook_repo.captured_path)


def test_hook_timeout(hook_repo: HookRepo):
    repo = hook_repo.repo
    hook_repo.behaviour("sleep")
    hook_repo.write_hooks("timeout_secs = 1\n")
    _stage_changes(repo)
    started = time.monotonic()
    with pytest.raises(LoreException) as exc:
        repo.commit("slow", offline=True)
    assert time.monotonic() - started < 10
    assert "timed out" in str(exc.value)
    assert exc.value.returncode == HOOK_REJECTED_EXIT_CODE


def test_json_output_stays_parseable(hook_repo: HookRepo):
    repo = hook_repo.repo
    hook_repo.behaviour("garbage")
    _stage_changes(repo)
    # run() returns stdout and stderr joined, so look at stdout alone through subprocess.
    result = subprocess.run(
        [
            repo.lore_executable_path,
            "--repository",
            repo.path,
            "--offline",
            "--json",
            "commit",
            "GAME-2 json",
        ],
        capture_output=True,
        text=True,
        env={**repo._subprocess_env()},
        cwd=repo.path,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert "garbage" in result.stderr
    for line in result.stdout.strip().splitlines():
        if line.strip():
            json.loads(line)
    parse_jsonl(result.stdout, "revisionCommitRevision")


def test_amend_runs_hook(hook_repo: HookRepo):
    repo = hook_repo.repo
    _stage_changes(repo)
    repo.commit("GAME-3 first", offline=True)
    os.remove(hook_repo.captured_path)
    repo.revision_amend("GAME-3 amended", offline=True)
    p = hook_repo.captured()
    assert p["operation"] == "amend"
    assert p["files"] == []
    assert p["message"] == "GAME-3 amended"

    hook_repo.behaviour("reject")
    with pytest.raises(LoreException):
        repo.revision_amend("GAME-3 rejected", offline=True)
    messages = [r.message for r in repo.history(oneline=True, offline=True)]
    assert any("GAME-3 amended" in m for m in messages)
    assert not any("rejected" in m for m in messages)


def test_dry_run_runs_hook(hook_repo: HookRepo):
    repo = hook_repo.repo
    _stage_changes(repo)
    before = _history_len(repo)
    repo.commit("GAME-4 dry", offline=True, dry_run=True)
    assert hook_repo.captured()["dry_run"] is True
    assert _history_len(repo) == before


def test_missing_command_aborts(hook_repo: HookRepo):
    repo = hook_repo.repo
    hook_repo.write_hooks(command=["does-not-exist-xyz"])
    _stage_changes(repo)
    with pytest.raises(LoreException) as exc:
        repo.commit("x", offline=True)
    assert "could not be started" in str(exc.value)


def test_invalid_hooks_file_aborts(hook_repo: HookRepo):
    repo = hook_repo.repo
    hook_repo.write_hooks("bogus_key = 1\n")
    _stage_changes(repo)
    with pytest.raises(LoreException) as exc:
        repo.commit("x", offline=True)
    assert ".lorehooks.toml" in str(exc.value)


def test_no_hooks_file_changes_nothing(new_lore_repo):
    repo: Lore = new_lore_repo()
    repo.write_files({"a.txt": "a\n"})
    repo.stage(scan=True, offline=True)
    assert "Commit succeeded" in repo.commit("plain", offline=True)
