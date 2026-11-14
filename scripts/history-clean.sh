#!/usr/bin/env bash
# scripts/history-clean.sh
# Prepares a clean git history for smartconnect-linux by filtering out:
# 1. .notes/ (historical local path leaks)
# 2. docs/assets/*.png (uncompressed heavy bitmaps)
#
# NOTE: This script does NOT modify the working checkout or push anything to remotes.
# It operates in an isolated temporary mirror clone and verifies the result.

set -euo pipefail

DRY_RUN=0

usage() {
  cat <<'EOF'
Usage: scripts/history-clean.sh [options]

Prepares a sanitized git history in an isolated temporary clone.

Options:
  -n, --dry-run   Perform clone, filter-repo, and verification, then clean up temp directory
  -h, --help      Show this help message
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    -n|--dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'Unknown option: %s\n\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Verify git-filter-repo is installed
FILTER_REPO_CMD=""
if command -v git-filter-repo >/dev/null 2>&1; then
  FILTER_REPO_CMD="git-filter-repo"
elif command -v uv >/dev/null 2>&1 && uv tool run git-filter-repo --version >/dev/null 2>&1; then
  FILTER_REPO_CMD="uv tool run git-filter-repo"
else
  echo "[x] git-filter-repo not found." >&2
  echo "    Install with: uv tool install git-filter-repo  OR  pip install git-filter-repo" >&2
  exit 1
fi

TEMP_CLONE="$(mktemp -d -t smartconnect-history-clean-XXXXXX)"
cleanup() {
  if [ "$DRY_RUN" -eq 1 ] && [ -d "$TEMP_CLONE" ]; then
    rm -rf "$TEMP_CLONE"
  fi
}
trap cleanup EXIT

echo "==> Step 1: Making fresh clone with --no-local..."
git clone --no-local "$REPO_ROOT" "$TEMP_CLONE"

echo "==> Step 2: Running git-filter-repo to purge .notes/ and docs/assets/*.png..."
(
  cd "$TEMP_CLONE"
  $FILTER_REPO_CMD --invert-paths --path .notes --path-glob 'docs/assets/*.png'
)

echo "==> Step 3: Verifying sanitized repository history..."

# Check 1: Ensure no commits touch .notes or docs/assets/*.png
REMAINING_PATHS=$(git -C "$TEMP_CLONE" log --all --full-history -- .notes 'docs/assets/*.png')
if [ -n "$REMAINING_PATHS" ]; then
  echo "[x] Verification failed: remaining commits found touching .notes or docs/assets/*.png:" >&2
  echo "$REMAINING_PATHS" >&2
  exit 1
fi
echo "[ok] Zero historical commits touching .notes/ or docs/assets/*.png"

# Check 2: Grep commit patches for personal path leaks (excluding this verification script itself)
CURRENT_USER="${USER:-$(id -un 2>/dev/null || whoami 2>/dev/null || echo "")}"
USER_HOME="${HOME:-/home/${CURRENT_USER}}"
# Literal (-F) patterns: a regex built from $USER/$HOME breaks on a name with regex
# metacharacters, and a silently widened or failed match defeats the whole check.
LEAK_GREP=(-F -e "$USER_HOME")
if [ -n "$CURRENT_USER" ] && [ "$CURRENT_USER" != "root" ] && [ "$CURRENT_USER" != "user" ]; then
  LEAK_GREP+=(-e "/home/${CURRENT_USER}")
fi
REMAINING_LEAKS=$(git -C "$TEMP_CLONE" log -p --all -- . ':!scripts/history-clean.sh' | grep "${LEAK_GREP[@]}" || true)
if [ -n "$REMAINING_LEAKS" ]; then
  echo "[x] Verification failed: personal path still found in commit diffs:" >&2
  echo "$REMAINING_LEAKS" >&2
  exit 1
fi
echo "[ok] Zero instances of personal home paths found across full commit graph"

ORIG_SIZE=$(du -sh "$REPO_ROOT/.git" | cut -f1)
CLEAN_SIZE=$(du -sh "$TEMP_CLONE/.git" | cut -f1)
echo "[ok] Repository .git size reduced from $ORIG_SIZE to $CLEAN_SIZE"

if [ "$DRY_RUN" -eq 1 ]; then
  echo ""
  echo "==> Dry run complete. Verification passed. Temp clone cleaned up."
  exit 0
fi

echo ""
echo "================================================================================"
echo " SANITIZED REPOSITORY READY AT:"
echo "   $TEMP_CLONE"
echo "================================================================================"
echo ""
echo "REMAINING MANUAL STEPS TO GO PUBLIC SAFELY:"
echo ""
echo "Important Note on GitHub PR Refs:"
echo "  GitHub retains all pull request head commits under hidden 'refs/pull/*' refs."
echo "  Force-pushing to an existing repository will NOT remove old commits from GitHub's"
echo "  internal object database or web cache if they were part of PR #5 or PR #6."
echo ""
echo "Recommended Route (Clean Public Launch):"
echo "  1. Rename or delete the existing private repository on GitHub (or create a fresh"
echo "     repo, e.g. https://github.com/vemru/smartconnect-linux)."
echo "     * NOTE: Existing GitHub PRs (#5, #6) and PR comments will be lost."
echo ""
echo "  2. Add replacement mock screenshots (< 300 KB each, per docs/assets/README.md)"
echo "     to the sanitized tree if desired."
echo ""
echo "  3. Push the sanitized repository to GitHub:"
echo "       cd \"$TEMP_CLONE\""
echo "       git remote add origin git@github.com:vemru/smartconnect-linux.git"
echo "       git push -u origin master --tags"
echo ""
echo "  4. In GitHub Repository Settings:"
echo "       - Enable Secret Scanning & Push Protection"
echo "       - Enable Private Vulnerability Reporting"
echo "       - Set Master branch protection rules (require passing CI checks)"
echo "       - Set Repository Description and Topics"
echo "       - Toggle visibility to Public"
echo "================================================================================"
