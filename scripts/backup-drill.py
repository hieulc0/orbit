"""Run the image deployment qualification with a real isolated pg_dump/restore."""
import runpy
import sys
from pathlib import Path

if __name__ == "__main__":
    sys.argv.append("--backup")
    runpy.run_path(str(Path(__file__).with_name("deployment-smoke.py")), run_name="__main__")
