"""Moved to corpus/generate.py (ledger 005). Kept as a wrapper until wave two because plans 001 and 002 cite this path."""
import runpy; from pathlib import Path; runpy.run_path(str(Path(__file__).resolve().parents[2] / "corpus" / "generate.py"), run_name="__main__")
