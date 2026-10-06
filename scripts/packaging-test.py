#!/usr/bin/env python3
"""Package layout regression using fake builds, never installed or executed.

This checks that a clean shell can find all four crate outputs. Actual Rust
compilation remains covered separately by the Rust suites and release build.
"""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class Packaging(unittest.TestCase):
    def test_clean_shell_uses_one_target_for_all_binaries(self):
        root = Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory(prefix="jamsys-package-") as tmp:
            fixture = Path(tmp) / "src"
            shutil.copytree(root, fixture, ignore=shutil.ignore_patterns(
                ".git", "target", "build", "__pycache__"))
            bindir = Path(tmp) / "bin"
            bindir.mkdir()
            cargo = bindir / "cargo"
            cargo.write_text('''#!/bin/sh
set -eu
name=$(basename "$PWD")
[ "$name" != jamsys-daemon ] || name=jamsysd
dest=${CARGO_TARGET_DIR:-$PWD/target}/release
mkdir -p "$dest"
printf 'test placeholder, not an executable application\\n' > "$dest/$name"
chmod 755 "$dest/$name"
''')
            cargo.chmod(0o755)
            env = dict(os.environ)
            env.pop("CARGO_TARGET_DIR", None)
            env.pop("BUILD_DIR", None)
            env["PATH"] = str(bindir) + os.pathsep + env["PATH"]
            result = subprocess.run(["bash", "packaging/build-deb.sh"], cwd=fixture,
                                    env=env, text=True, capture_output=True, timeout=60)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            package, = (fixture / "build").glob("*.deb")
            contents = subprocess.check_output(["dpkg-deb", "--contents", package], text=True)
            for name in ("usr/bin/jamsysd", "usr/libexec/jamsys-helper",
                         "usr/libexec/jamsys-kbd", "usr/libexec/jamsys-power",
                         "jamsys_ui/processes.py"):
                self.assertIn(name, contents)


if __name__ == "__main__":
    unittest.main()
