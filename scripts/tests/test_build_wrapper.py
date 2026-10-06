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

STUB_ARGV_PREFIX = "STUB_ARGV:"
# The prefix is baked into the stub source at write time; referencing a
# test-process variable there would be a NameError at stub runtime. The cmd
# launcher keeps the Python string single-quoted inside its double quotes,
# while the sh launcher wraps the same code in single quotes and therefore
# needs the double-quoted variant.
PY_CODE_NT = "import sys,json;print('" + STUB_ARGV_PREFIX + "'+json.dumps(sys.argv[1:]))"
PY_CODE_POSIX = 'import sys,json;print("' + STUB_ARGV_PREFIX + '"+json.dumps(sys.argv[1:]))'


def _run_wrapper(argv: list[str]) -> list[str]:
    """Run this repo's wrapper with a stub cargo and return the final argv."""
    with tempfile.TemporaryDirectory(prefix="wrapper_stub_") as tmp:
        stub_dir = Path(tmp)
        env = dict(os.environ)
        env["PATH"] = str(stub_dir) + os.pathsep + env.get("PATH", "")
        if os.name == "nt":
            # cmd forwards %* verbatim; Python re-splits it into argv.
            (stub_dir / "cargo.cmd").write_text(
                "@echo off\r\npython -c \"" + PY_CODE_NT + "\" %*\r\n",
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
                "#!/bin/sh\nexec python3 -c '" + PY_CODE_POSIX + "' \"$@\"\n",
                encoding="ascii",
            )
            cargo.chmod(0o755)
            cmd = ["bash", str(REPO / "scripts" / "build.sh"), *argv]
        # errors="replace": the wrappers print CJK banners; a runner codepage
        # mismatch must not crash the harness (that would be a false red).
        proc = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            env=env,
            timeout=180,
        )
        combined = proc.stdout + proc.stderr
        if proc.returncode != 0:
            raise AssertionError(
                f"wrapper exited {proc.returncode}; tail of output:\n{combined[-2000:]}"
            )
        for line in combined.splitlines():
            line = line.strip()
            if STUB_ARGV_PREFIX in line:
                parsed = json.loads(line.split(STUB_ARGV_PREFIX, 1)[1])
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


def test_global_option_before_subcommand_still_gets_testing():
    """`--locked test` is valid cargo: global options precede the subcommand.

    The subcommand used to be read off the first token only, so this shape
    skipped the injection entirely and the run died as a wall of "module is
    private" errors - readable as a code problem, not as a missing feature flag.
    """
    final = _run_wrapper(["--locked", "test"])
    assert final == ["--locked", "test", "--features", "testing"], final


def test_features_after_double_dash_does_not_suppress_injection():
    """`--` starts the arguments forwarded to the test binary / rustc.

    A `--features` living there was never a request to cargo, so treating it as
    one silenced the injection for the whole run.
    """
    final = _run_wrapper(["test", "--", "--features"])
    assert final == ["test", "--features", "testing", "--", "--features"], final


def test_merged_features_form_is_not_reinjected():
    """`--features=x` and `-Fxyz` are the same request as `--features x`.

    Comparing tokens for exact equality missed both forms, so the wrapper added
    a second `--features`: cargo unions them, so nothing fails loudly - the
    argv the caller wrote is silently not the argv cargo received.
    """
    assert _run_wrapper(["test", "--features=testing"]) == ["test", "--features=testing"]
    assert _run_wrapper(["test", "-Ftesting"]) == ["test", "-Ftesting"]


def test_package_selection_does_not_inject_root_only_feature():
    """`testing` lives on the root package only.

    `test -p weflow-client` selects the SDK crate, whose suite runs against the
    public API and needs no feature. Injecting `testing` there made cargo refuse
    with "does not contain this feature: testing" - the same unreadable wall of
    errors the wrapper exists to prevent.
    """
    assert _run_wrapper(["test", "--locked", "-p", "weflow-client"]) == [
        "test",
        "--locked",
        "-p",
        "weflow-client",
    ]


def test_merged_package_form_does_not_inject_either():
    """`--package=x` asks for the same thing as `--package x`.

    The guard compared tokens with `-eq`, so only the separate form was
    recognised and the merged one re-injected `testing` - the exact refusal the
    guard exists to prevent, and the same submission that already handles the
    merged `--features=x` form.
    """
    assert _run_wrapper(["test", "--package=weflow-client"]) == ["test", "--package=weflow-client"]
    assert _run_wrapper(["test", "-pweflow-client"]) == ["test", "-pweflow-client"]
