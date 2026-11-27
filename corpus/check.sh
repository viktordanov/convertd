#!/bin/sh
# Runs `corpus/generate.py --check` with the pinned packages of
# corpus/requirements.txt. CI calls it after
# actions/setup-python with Python 3.13.
#
# Usage: corpus/check.sh [python]   (default: python3.13)
#
# It makes a virtual environment in a new temporary directory, installs
# the pins with --require-hashes, regenerates the corpus there and
# compares every file with expected.json. It removes the environment
# when it exits. Exit status: 0 when every file matches, 1 on a
# difference, 2 on a wrong Python.
set -eu

python="${1:-python3.13}"
root="$(cd "$(dirname "$0")/.." && pwd)"

version="$("$python" -c 'import sys; print("%d.%d" % sys.version_info[:2])')"
if [ "$version" != "3.13" ]; then
    echo "corpus/check.sh: needs Python 3.13, but $python is $version" >&2
    exit 2
fi

venv="$(mktemp -d "${TMPDIR:-/tmp}/corpus-check.XXXXXX")"
trap 'rm -rf -- "$venv"' EXIT INT TERM

"$python" -m venv "$venv"
"$venv/bin/python" -I -m pip install --quiet --disable-pip-version-check \
    --require-hashes --no-deps -r "$root/corpus/requirements.txt"
"$venv/bin/python" -I "$root/corpus/generate.py" --check
