#!/usr/bin/env python3
"""Regenerate clients/python/src/weflow_sdk/generated from the server's
OpenAPI description.

Source of truth is the server's live OpenAPI document, emitted by the Rust
regenerator through the repository build wrapper. Build or document failures
stop generation; golden placeholders are never used as model definitions.

The normalized spec is committed next to the generated client, and CI
asserts that rerunning this script produces no diff. Edit nothing under
generated/ by hand.

Usage:
    python scripts/regen.py            # regenerate (needs openapi-generator)
    python scripts/regen.py --check    # exit 1 if a regen would change spec or tree
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
OUT = Path(__file__).resolve().parents[1] / "src" / "weflow_sdk" / "generated"
PKG = OUT.parent.name          # the generated package directory (its import root)
PKGDIR = Path(__file__).resolve().parents[1]      # clients/python
CONFIG = PKGDIR / "openapitools.json"             # pins the generator jar version
# The wrapper version is pinned in the npx call itself: the floating default
# would let a new wrapper (or its default jar) change generation without any
# repo change - exactly what the no-diff gate must be able to blame.
WRAPPER = "@openapitools/openapi-generator-cli@2.41.0"


def build_spec() -> dict:
    """Read the live OpenAPI document through the required Rust build wrapper."""
    if sys.platform == "win32":
        command = ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass",
                   "-File", str(REPO / "scripts" / "build.ps1")]
    else:
        command = ["bash", str(REPO / "scripts" / "build.sh")]
    command += ["run", "--locked", "-p", "weflow-regen", "--", "--dump-spec"]
    result = subprocess.run(command, cwd=str(REPO), check=True,
                            capture_output=True, text=True, encoding="utf-8", errors="replace")
    # The wrapper emits its toolchain banner before the JSON document.
    lines = result.stdout.splitlines()
    for index, line in enumerate(lines):
        if line.strip() == "{":
            return json.loads("\n".join(lines[index:]))
    raise RuntimeError("build wrapper returned no OpenAPI document")


def tree_digest(root: Path) -> str:
    # Byte-compiled artifacts are excluded deliberately: a local pytest run
    # drops __pycache__/*.pyc INSIDE the generated tree (the scratch
    # regeneration never makes them), and without this filter the digest
    # gate reports a false "stale tree" on a byte-identical checkout.
    sha = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        if "__pycache__" in path.parts or path.suffix == ".pyc":
            continue
        sha.update(str(path.relative_to(root)).encode())
        sha.update(path.read_bytes())
    return sha.hexdigest()


def normalize_generated(root: Path) -> int:
    """Strip trailing whitespace and trailing blank lines from generated files.

    The generator leaves trailing spaces inside docstrings and a blank line at the
    end of several files. This repository checks whitespace with ``git diff
    --check`` against the empty tree, so a committed file is inspected forever -
    not only in the change that introduced it: a single unnormalized regeneration
    turns the whole-repository check red and stays red until the bytes change.
    Normalizing inside the pipeline keeps every regeneration clean without editing
    ``generated/`` by hand, which the module docstring forbids.

    Returns the number of files rewritten.
    """
    changed = 0
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        raw = path.read_bytes()
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            continue
        lines = [line.rstrip() for line in text.splitlines()]
        while lines and not lines[-1]:
            lines.pop()
        fixed = ("\n".join(lines) + "\n").encode("utf-8") if lines else b""
        if fixed != raw:
            path.write_bytes(fixed)
            changed += 1
    return changed


def write_spec(spec, path):
    text = json.dumps(spec, indent=2, ensure_ascii=False, sort_keys=True) + "\n"
    path.write_text(text, encoding="utf-8")
    return text


def generate_tree(spec_path, dest_root):
    # Run the generator from spec_path into dest_root/PKG, keeping only the
    # library face, rewriting the absolute imports the generator emits
    # (nested under our package they would resolve to the handwritten one),
    # and normalizing whitespace. Returns the number of files normalized.
    with tempfile.TemporaryDirectory() as tmp:
        tmp_out = Path(tmp) / "gen"
        cmd = ["npx.cmd" if sys.platform == "win32" else "npx",
               "--yes", WRAPPER,
               "--openapitools", str(CONFIG),
               "generate",
               "-i", str(spec_path),
               "-g", "python",
               "-o", str(tmp_out),
               "--package-name", PKG,
               "--library", "httpx"]
        subprocess.run(cmd, check=True, cwd=str(PKGDIR))
        shutil.copytree(tmp_out / PKG, dest_root / PKG)
    pkg_dir = dest_root / PKG
    for py_file in pkg_dir.rglob("*.py"):
        text = py_file.read_text(encoding="utf-8")
        fixed = (
            text
            .replace("from " + PKG + ".", "from " + PKG + ".generated." + PKG + ".")
            .replace("from " + PKG + " import",
                     "from " + PKG + ".generated." + PKG + " import")
            .replace("import " + PKG + ".", "import " + PKG + ".generated." + PKG + ".")
        )
        if fixed != text:
            py_file.write_text(fixed, encoding="utf-8")
    return normalize_generated(dest_root)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true",
                        help="fail if a regeneration would change spec or the tree")
    args = parser.parse_args()

    spec = build_spec()
    fresh_spec = json.dumps(spec, indent=2, ensure_ascii=False, sort_keys=True) + "\n"

    if args.check:
        # The digest half of --check actually runs the generator (unlike the
        # old spec-only check), so CI needs node/npx + a JRE. Fail with a
        # readable line instead of a subprocess traceback.
        import shutil as _shutil
        npx = "npx.cmd" if sys.platform == "win32" else "npx"
        if _shutil.which(npx) is None:
            print("regen --check needs npx (Node.js) on PATH to regenerate the tree",
                  file=sys.stderr)
            return 1
        # Two comparisons, because "the spec did not change" is NOT the gate
        # it looks like: model output and whitespace can drift while the
        # description stays byte-identical (generator version, templates).
        # The whole tree is regenerated into a scratch dir and compared by
        # digest; spec equality alone could not see any of that.
        committed_spec = (OUT / "spec.json").read_text(encoding="utf-8")
        if committed_spec != fresh_spec:
            print("generated spec is stale: rerun scripts/regen.py and commit",
                  file=sys.stderr)
            return 1
        with tempfile.TemporaryDirectory() as tmp:
            tmp_spec = Path(tmp) / "spec.json"
            tmp_spec.write_text(fresh_spec, encoding="utf-8")
            gen_root = Path(tmp) / "tree"
            gen_root.mkdir()
            generate_tree(tmp_spec, gen_root)
            fresh_digest = tree_digest(gen_root / PKG)
        committed_digest = tree_digest(OUT / PKG)
        if fresh_digest != committed_digest:
            print("generated tree is stale: rerun scripts/regen.py and commit\n"
                  "  committed    " + committed_digest + "\n"
                  "  regenerated  " + fresh_digest, file=sys.stderr)
            return 1
        print("generated spec and tree are up to date:", OUT, fresh_digest[:12])
        return 0

    OUT.mkdir(parents=True, exist_ok=True)
    spec_path = OUT / "spec.json"
    write_spec(spec, spec_path)

    for child in OUT.iterdir():
        if child.name != "spec.json":
            shutil.rmtree(child) if child.is_dir() else child.unlink()
    normalized = generate_tree(spec_path, OUT)
    print("regenerated: " + str(OUT) + " (normalized " + str(normalized) + " files)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
