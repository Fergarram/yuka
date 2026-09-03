#!/bin/sh
#
# Build mt-inject (Linux guest). The repo is read-only on the 9p share, so the
# binary goes to $HOME rather than next to the source.
#
set -e
src="$(cd "$(dirname "$0")" && pwd)/mt-inject.c"
out="${1:-$HOME/mt-inject}"
gcc -O2 -Wall "$src" -o "$out" -lm
echo "built $out"
