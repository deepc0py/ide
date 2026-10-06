#!/usr/bin/env bash
# Obtain VS Code (MIT) source at a pinned commit for building the extension host.
# Strategy: copy from the local read-only vendor clone at the pinned commit into a
# gitignored working dir. Falls back to `git clone` if the vendor copy is absent.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PIN_COMMIT="1522052c994045fe46753b85421d6f3ca340235e" # code-oss 1.142.0
VENDOR="/Users/studio/x/vendor/vscode"
DEST="$HERE/.vscode-src"

marker="$DEST/.pinned-commit"
if [[ -f "$marker" && "$(cat "$marker")" == "$PIN_COMMIT" ]]; then
  echo "vscode source already present at $PIN_COMMIT"
  exit 0
fi

rm -rf "$DEST"
mkdir -p "$DEST"

if [[ -d "$VENDOR/.git" ]]; then
  have="$(git -C "$VENDOR" rev-parse HEAD)"
  if [[ "$have" != "$PIN_COMMIT" ]]; then
    echo "WARNING: vendor vscode HEAD $have != pinned $PIN_COMMIT; copying current tree anyway" >&2
  fi
  echo "Copying vscode src from $VENDOR ..."
  # Only the pieces the extension-host bundle needs.
  rsync -a --delete \
    --include='src/***' \
    --include='product.json' \
    --include='package.json' \
    --exclude='*' \
    "$VENDOR/" "$DEST/"
else
  echo "Cloning vscode at $PIN_COMMIT ..."
  git clone --filter=blob:none https://github.com/microsoft/vscode.git "$DEST"
  git -C "$DEST" checkout "$PIN_COMMIT"
fi

echo "$PIN_COMMIT" > "$marker"
echo "vscode source ready at $DEST"
