"""The model generator must use the build wrapper and fail on stale input."""
import importlib.util
import subprocess
from pathlib import Path
from types import SimpleNamespace

import pytest

SPEC = importlib.util.spec_from_file_location("sdk_regen", Path(__file__).resolve().parents[2] / "clients/python/scripts/regen.py")
regen = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(regen)


@pytest.mark.parametrize("platform, wrapper", [("win32", "build.ps1"), ("linux", "build.sh")])
def test_live_spec_uses_wrapper_and_parses_banner(monkeypatch, platform, wrapper):
    calls = []
    def run(command, **kwargs):
        calls.append(command)
        assert kwargs["check"] is True
        return SimpleNamespace(stdout='toolchain banner\n{\n"openapi": "3.0.3"\n}\n')
    monkeypatch.setattr(regen.sys, "platform", platform)
    monkeypatch.setattr(regen.subprocess, "run", run)
    assert regen.build_spec() == {"openapi": "3.0.3"}
    assert any(arg.endswith(wrapper) for arg in calls[0])
    assert calls[0][-2:] == ["--", "--dump-spec"]


def test_build_failure_never_falls_back_to_golden(monkeypatch):
    def fail(*args, **kwargs):
        raise subprocess.CalledProcessError(1, args[0])
    monkeypatch.setattr(regen.subprocess, "run", fail)
    with pytest.raises(subprocess.CalledProcessError):
        regen.build_spec()
