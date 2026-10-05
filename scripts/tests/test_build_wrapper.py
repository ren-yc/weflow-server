"""Guard the build wrapper's argument passthrough.

The wrapper injects `--features testing` for test/clippy invocations. A
regression there can silently turn the full test run into a zero-match run
(the caller's first argument injected a second time acts as a filter word),
which is exactly the failure mode the repo's gate discipline warns about: a
green gate that never ran what it claims to have run.

These tests never invoke the real cargo. The stub found first on PATH hands
the final argv to Python as one JSON line, and the assertions inspect it
directly, so they run anywhere Python runs.
"""

import json
import os
import subprocess
import tempfile
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent.parent
REPO = SCRIPTS.parent

PY_CODE = "import sys,json;print(json.dumps(sys.argv[1:]))"


def _run_wrapper(argv: list[str]) -> list[str]:
    """Run this repo's wrapper with a stub cargo and return the final argv."""
    with tempfile.TemporaryDirectory(prefix="wrapper_stub_") as tmp:
        stub_dir = Path(tmp)
        env = dict(os.environ)
        env["PATH"] = str(stub_dir) + os.pathsep + env.get("PATH", "")
        if os.name == "nt":
            # cmd forwards %* verbatim; Python re-splits it into argv.
            (stub_dir / "cargo.cmd").write_text(
                "@echo off\r\npython -c \"" + PY_CODE + "\" %*\r\n",
                encoding="ascii",
            )
            vcvars = stub_dir / "vcvars64.bat"
            vcvars.write_text("@rem stub\r\n", encoding="ascii")
            env["WEFLOW_VCVARS" if REPO.name.startswith("weflow") else "QQFLOW_VCVARS"] = str(vcvars)
            cmd = [
                "powershell",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(REPO / "scripts" / "build.ps1"),
                *argv,
            ]
        else:
            cargo = stub_dir / "cargo"
            cargo.write_text(
                "#!/bin/sh\nexec python3 -c '" + PY_CODE + "' \"$@\"\n",
                encoding="ascii",
            )
            cargo.chmod(0o755)
            cmd = ["bash", str(REPO / "scripts" / "build.sh"), *argv]
        proc = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=180)
        combined = proc.stdout + proc.stderr
        if proc.returncode != 0:
            raise AssertionError(
                f"wrapper exited {proc.returncode}; tail of output:\n{combined[-2000:]}"
            )
        for line in combined.splitlines():
            line = line.strip()
            if line.startswith("["):
                parsed = json.loads(line)
                assert all(isinstance(x, str) for x in parsed), parsed
                return parsed
        raise AssertionError(f"stub cargo was not invoked; tail of output:\n{combined[-2000:]}")


def test_single_test_argument_is_not_reinjected():
    """A bare `test` must reach cargo once, not come back as a filter word."""
    final = _run_wrapper(["test"])
    assert final == ["test", "--features", "testing"], final


def test_trailing_arguments_are_preserved_after_injection():
    final = _run_wrapper(["test", "--locked"])
    assert final == ["test", "--features", "testing", "--locked"], final


def test_clippy_all_targets_gets_testing_feature():
    final = _run_wrapper(["clippy", "--all-targets", "--locked", "--", "-D", "warnings"])
    assert final == [
        "clippy",
        "--features",
        "testing",
        "--all-targets",
        "--locked",
        "--",
        "-D",
        "warnings",
    ], final


def test_explicit_features_are_left_alone():
    final = _run_wrapper(["test", "--locked", "--features", "testing"])
    assert final == ["test", "--locked", "--features", "testing"], final


def test_build_subcommand_is_not_touched():
    final = _run_wrapper(["build", "--locked"])
    assert final == ["build", "--locked"], final


def test_no_arguments_at_all():
    final = _run_wrapper([])
    assert final == [], final
