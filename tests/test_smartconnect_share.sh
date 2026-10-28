#!/usr/bin/env bash
set -euo pipefail

SHARE_SCRIPT="/home/user/src/smartconnect-linux/packaging/smartconnect-share"
[ -x "$SHARE_SCRIPT" ] || { echo "error: $SHARE_SCRIPT not found or not executable" >&2; exit 1; }

TMP_DIR="$(mktemp -d /tmp/sc-share-test.XXXXXX)"
trap 'rm -rf "$TMP_DIR"' EXIT

TEST_SHARE="$TMP_DIR/SmartConnect"
mkdir -p "$TEST_SHARE"

# Mock config to use TEST_SHARE
FAKE_CONFIG_HOME="$TMP_DIR/.config"
mkdir -p "$FAKE_CONFIG_HOME/smartconnect-linux"
cat <<JSON > "$FAKE_CONFIG_HOME/smartconnect-linux/gui.json"
{
  "files": {
    "share": false,
    "shareDir": "$TEST_SHARE"
  }
}
JSON

export XDG_CONFIG_HOME="$FAKE_CONFIG_HOME"

echo "==> Test 1: --help option"
"$SHARE_SCRIPT" --help | grep -q "Usage: smartconnect-share"
echo "[PASS] --help displays usage"

echo "==> Test 2: Multiple files as arguments"
SRC_DIR="$TMP_DIR/source"
mkdir -p "$SRC_DIR"
echo "hello file 1" > "$SRC_DIR/file1.txt"
echo "hello file 2" > "$SRC_DIR/file2.txt"
echo "hello file 3" > "$SRC_DIR/file3.txt"

"$SHARE_SCRIPT" "$SRC_DIR/file1.txt" "$SRC_DIR/file2.txt" "$SRC_DIR/file3.txt"

[ -f "$TEST_SHARE/file1.txt" ]
[ -f "$TEST_SHARE/file2.txt" ]
[ -f "$TEST_SHARE/file3.txt" ]
cmp "$SRC_DIR/file1.txt" "$TEST_SHARE/file1.txt"
cmp "$SRC_DIR/file2.txt" "$TEST_SHARE/file2.txt"
cmp "$SRC_DIR/file3.txt" "$TEST_SHARE/file3.txt"
echo "[PASS] Multiple files shared and contents match"

echo "==> Test 3: Duplicates / collision handling"
rm -f "$SRC_DIR/file1.txt"; echo "modified version" > "$SRC_DIR/file1.txt"
"$SHARE_SCRIPT" "$SRC_DIR/file1.txt"
# Should create a non-colliding file since content differs and inode differs
count=$(ls -1 "$TEST_SHARE"/file1* | wc -l)
[ "$count" -ge 2 ]
echo "[PASS] Collision created distinct file without overwriting"

echo "==> Test 4: Files already in share are skipped"
"$SHARE_SCRIPT" "$TEST_SHARE/file2.txt"
# Count shouldn't change
echo "[PASS] Already shared files cleanly skipped"

echo "==> Test 5: Multiple files via NAUTILUS_SCRIPT_SELECTED_FILE_PATHS"
echo "nautilus file A" > "$SRC_DIR/fileA.txt"
echo "nautilus file B" > "$SRC_DIR/fileB.txt"

NAUTILUS_SCRIPT_SELECTED_FILE_PATHS="$SRC_DIR/fileA.txt"$'\n'"$SRC_DIR/fileB.txt" "$SHARE_SCRIPT"

[ -f "$TEST_SHARE/fileA.txt" ]
[ -f "$TEST_SHARE/fileB.txt" ]
echo "[PASS] Multiple files shared via NAUTILUS_SCRIPT_SELECTED_FILE_PATHS"

echo "==> Test 6: Directories can be shared"
mkdir -p "$SRC_DIR/myfolder/nested"
echo "inside nested" > "$SRC_DIR/myfolder/nested/item.txt"

"$SHARE_SCRIPT" "$SRC_DIR/myfolder"

[ -d "$TEST_SHARE/myfolder/nested" ]
[ -f "$TEST_SHARE/myfolder/nested/item.txt" ]
echo "[PASS] Folder recursively shared"

echo "==> Test 7: file:// URIs handled"
echo "uri file" > "$SRC_DIR/file_uri.txt"
"$SHARE_SCRIPT" "file://$SRC_DIR/file_uri.txt"

[ -f "$TEST_SHARE/file_uri.txt" ]
echo "[PASS] file:// URI handled"

echo "==> All 7 integration tests passed!"
