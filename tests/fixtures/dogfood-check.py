"""Operator-owned offline check wrapper for a disposable, pinned Orbit checkout."""
import os
from pathlib import Path
import subprocess
import sys

toolchain, cache, build = map(Path, sys.argv[1:])
readme = Path("README.md").read_text()
assert readme.count("## Reproducibility check (dogfood fixture)") == 1
changed = subprocess.check_output(["git", "diff", "HEAD", "--name-only"], text=True).splitlines()
assert changed == ["README.md"], changed
environment = dict(os.environ, CARGO_HOME=str(cache), CARGO_TARGET_DIR=str(build),
                   RUSTC=str(toolchain / "rustc"), RUSTDOC=str(toolchain / "rustdoc"),
                   PATH=str(toolchain) + os.pathsep + os.environ["PATH"])
for args in [["fmt", "--all", "--", "--check"], ["test", "--locked", "--offline"]]:
    subprocess.run([str(toolchain / "cargo"), *args], env=environment, check=True, timeout=480)
print("Pinned Orbit candidate: README-only patch, formatting and regular tests passed.")
