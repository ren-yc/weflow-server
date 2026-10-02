#!/usr/bin/env python3
"""Regenerate clients/python/src/weflow_sdk/generated from the server's
OpenAPI description.

Source of truth is the committed golden snapshot's body (the same bytes the
server served when the snapshot was pinned), normalized to OpenAPI 3.0 the
same way the Rust regen tool does it:

  1. placeholder-masked volatile values (the golden masks a few string
     properties) are restored to plain string schemas;
  2. `type: [T, null]` becomes `T` + `nullable: true` (3.1 -> 3.0);
  3. `oneOf: [T, {type: null}]` - utoipa's spelling of `Option<T>` - keeps
     only the non-null arm (the handlers omit the key entirely when absent);
  4. path-template placeholders get matching `parameters` declarations (the
     server emits them since the same batch; kept here so regeneration also
     works from a description fetched over HTTP).

The normalized spec is committed next to the generated client, and CI
asserts that rerunning this script produces no diff. Edit nothing under
generated/ by hand.

Usage:
    python scripts/regen.py            # regenerate (needs openapi-generator)
    python scripts/regen.py --check    # exit 1 if a regen would change files
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
GOLDEN = REPO / "tests" / "golden" / "openapi.json"
OUT = Path(__file__).resolve().parents[1] / "src" / "weflow_sdk" / "generated"


def normalize(node):
    """3.1-to-3.0 normalization, in place. Deterministic."""
    if isinstance(node, list):
        for item in node:
            normalize(item)
        return
    if not isinstance(node, dict):
        return
    types = node.get("type")
    if isinstance(types, list) and len(types) == 2 and "null" in types:
        rest = [t for t in types if t != "null"]
        node["nullable"] = True
        node["type"] = rest[0] if len(rest) == 1 else rest
    arms = node.get("oneOf")
    if isinstance(arms, list):
        kept = [a for a in arms if a.get("type") != "null"]
        if len(kept) != len(arms):
            if len(kept) == 1:
                only = kept[0]
                node.pop("oneOf")
                node.update(only)
            else:
                node["oneOf"] = kept
    for value in list(node.values()):
        normalize(value)


def build_spec() -> dict:
    """Get the normalized 3.0 spec.

    Preferred source: ``cargo run -p weflow-regen -- --dump-spec`` - the
    same normalization the Rust client's generator consumes, taken from the
    live ``document()`` rather than the golden snapshot (whose placeholder
    masking would burn wrong types - e.g. an integer watermark restored as
    string - into generated models). Falls back to the snapshot when cargo
    is unavailable, with a loud warning: the fallback can mis-type masked
    properties.
    """
    import subprocess

    try:
        raw = subprocess.run(
            ["cargo", "run", "--locked", "-p", "weflow-regen", "--", "--dump-spec"],
            cwd=str(REPO),
            check=True, capture_output=True, text=True,
        ).stdout
        return json.loads(raw)
    except (OSError, subprocess.CalledProcessError) as exc:
        print(
            "WARNING: falling back to the golden snapshot; volatile-masked"
            " properties will be typed as strings. Reason: "
            + str(exc),
            file=sys.stderr,
        )
    import re

    doc = json.loads(GOLDEN.read_text(encoding="utf-8"))["body"]

    def restore(node):
        if isinstance(node, list):
            for item in node:
                restore(item)
            return
        if not isinstance(node, dict):
            return
        for key, value in list(node.items()):
            if value == "<volatile>":
                node[key] = {"type": "string"}
            else:
                restore(value)

    restore(doc)
    normalize(doc)
    doc["openapi"] = "3.0.3"
    for path, item in doc.get("paths", {}).items():
        names = re.findall(r"\{(\w+)\}", path)
        if not names:
            continue
        for op in item.values():
            if not isinstance(op, dict):
                continue
            op["parameters"] = [
                {
                    "name": name,
                    "in": "path",
                    "required": True,
                    "style": "simple",
                    "schema": {"type": "string"},
                }
                for name in names
            ]
    return doc

def tree_digest(root: Path) -> str:
    sha = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if path.is_file():
            sha.update(str(path.relative_to(root)).encode())
            sha.update(path.read_bytes())
    return sha.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true",
                        help="fail if a regeneration would change files")
    args = parser.parse_args()

    spec = build_spec()
    if args.check:
        # The no-diff gate compares the committed spec against a fresh build;
        # the generated client is derived from that spec by the pinned
        # generator version, so spec equality implies client equality.
        committed = (OUT / "spec.json").read_text(encoding="utf-8")
        fresh = json.dumps(spec, indent=2, ensure_ascii=False, sort_keys=True) + "\n"
        if committed != fresh:
            print("generated spec is stale: rerun scripts/regen.py and commit", file=sys.stderr)
            return 1
        print("generated spec is up to date:", OUT / "spec.json")
        return 0

    OUT.mkdir(parents=True, exist_ok=True)
    spec_path = OUT / "spec.json"
    spec_path.write_text(json.dumps(spec, indent=2, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")

    with tempfile.TemporaryDirectory() as tmp:
        tmp_out = Path(tmp) / "gen"
        cmd = [
            sys.executable, "-m", "openapi_generator_cli"
            if False else "npx.cmd" if sys.platform == "win32" else "npx",
        ]
        # npx invocation kept explicit: the wrapper resolves the pinned
        # generator version through .openapi-generator/ in the package dir.
        cmd = ["npx.cmd" if sys.platform == "win32" else "npx",
               "--yes", "@openapitools/openapi-generator-cli", "generate",
               "-i", str(spec_path),
               "-g", "python",
               "-o", str(tmp_out),
               "--package-name", "weflow_sdk",
               "--library", "httpx"]
        subprocess.run(cmd, check=True, cwd=str(OUT))
        # Keep only the library face: models, api, client plumbing. Docs,
        # tests, CI recipes and the generator's own pyproject are dropped -
        # the handwritten layer owns those.
        if OUT.exists():
            for child in OUT.iterdir():
                if child.name != "spec.json":
                    shutil.rmtree(child) if child.is_dir() else child.unlink()
        shutil.copytree(tmp_out / "weflow_sdk", OUT / "weflow_sdk")
        # The generator emits the package with absolute imports (``from
        # weflow_sdk...``). Nested under our package those resolve to the
        # handwritten package and crash; rewrite them to the generated
        # subpackage.
        for py_file in (OUT / "weflow_sdk").rglob("*.py"):
            text = py_file.read_text(encoding="utf-8")
            fixed = (
                text
                .replace("from weflow_sdk.", "from weflow_sdk.generated.weflow_sdk.")
                .replace("from weflow_sdk import", "from weflow_sdk.generated.weflow_sdk import")
                .replace("import weflow_sdk.", "import weflow_sdk.generated.weflow_sdk.")
            )
            if fixed != text:
                py_file.write_text(fixed, encoding="utf-8")
    print("regenerated:", OUT)
    return 0


if __name__ == "__main__":
    sys.exit(main())
