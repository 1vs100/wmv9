#!/bin/bash
# Check a directory for genuine MS encoder DLLs vs Wine builtins.
# Usage: tools/check_dlls.sh [dir, default ~/wmv9-decomp/orig]
set -u
DIR="${1:-$HOME/wmv9-decomp/orig}"
WANT="wmvencod.dll wmvdmod.dll wmadmod.dll wmvdecod.dll wmvcore.dll wmasf.dll"
echo "Scanning: $DIR"
[ -d "$DIR" ] || { echo "MISSING dir: $DIR"; echo "See docs/acquire-dlls.md"; exit 1; }
for f in $WANT; do
  p="$DIR/$f"
  if [ ! -f "$p" ]; then echo "MISSING $f"; continue; fi
  echo "== $f =="
  sha256sum "$p" | cut -c1-64 | sed 's/^/  sha256: /'
  if command -v winedump >/dev/null 2>&1; then
    winedump -j version "$p" 2>&1 | grep -i -E "builtin|Version|Machine" | head -n 5 | sed 's/^/  /'
  fi
  strings "$p" 2>/dev/null | grep -a -m3 -E "CLSID_CWMV9|CLSID_CWMA|WMV3|WVC1|WMAPro|Wine builtin" | sed 's/^/  str: /'
done
echo "Done. Genuine MS DLLs must NOT say 'Wine builtin DLL'."
