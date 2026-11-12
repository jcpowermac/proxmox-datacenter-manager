#!/usr/bin/env bash
#
# sync-upstream.sh — rebase the Omada fork onto the latest stock PDM.
#
# The fork is built on a stock PDM release (currently 1.1.9). When upstream
# ships a new release/tag, this script:
#   1. fetches the stock PDM "upstream" remote,
#   2. shows exactly which upstream commits would be pulled in,
#   3. backs up the current omada branch,
#   4. rebases omada onto the chosen upstream base,
#   5. on success, offers a build+test check.
#
# Usage:
#   scripts/sync-upstream.sh                 # interactive: report + rebase omada -> upstream/master
#   scripts/sync-upstream.sh --dry-run       # fetch + report only, no rebase
#   scripts/sync-upstream.sh --base <rev>    # rebase onto <rev> (default upstream/master), e.g. a tag
#   scripts/sync-upstream.sh --no-test       # skip the post-rebuild test check
#
# Env overrides:
#   UPSTREAM_URL   stock PDM repo (default: the Vcs-Git from debian/control)
#   UPSTREAM       remote name        (default: upstream)
#   FORK_BRANCH    branch to rebase   (default: omada)

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

UPSTREAM_URL="${UPSTREAM_URL:-git://git.proxmox.com/git/proxmox-datacenter-manager.git}"
UPSTREAM="${UPSTREAM:-upstream}"
FORK_BRANCH="${FORK_BRANCH:-omada}"
DRY_RUN=0
BASE=""
RUN_TEST=1

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --base)    shift; BASE="${1:?--base needs a rev}" ;;
    --no-test) RUN_TEST=0 ;;
    -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
  shift
done

hr() { printf '\n\033[1m%s\033[0m\n' "$*"; }

hr "1. upstream remote"
if git remote get-url "$UPSTREAM" >/dev/null 2>&1; then
  echo "  $UPSTREAM -> $(git remote get-url "$UPSTREAM")"
else
  echo "  adding $UPSTREAM -> $UPSTREAM_URL"
  git remote add "$UPSTREAM" "$UPSTREAM_URL"
fi
git fetch --tags "$UPSTREAM"

# Default base: latest upstream master.
if [ -z "$BASE" ]; then
  BASE="$UPSTREAM/master"
fi
if ! git rev-parse --verify --quiet "$BASE" >/dev/null; then
  echo "  base '$BASE' not found; available upstream refs:" >&2
  git for-each-ref --format='    %(refname:short)' "refs/remotes/$UPSTREAM" | head -20 >&2
  exit 1
fi

hr "2. what is new upstream"
# The fork's upstream base is the merge-base of the fork branch and upstream.
CUR_BASE="$(git merge-base "$FORK_BRANCH" "$BASE" 2>/dev/null || true)"
echo "  fork branch : $FORK_BRANCH ($(git rev-parse --short "$FORK_BRANCH"))"
echo "  new base    : $BASE ($(git rev-parse --short "$BASE"))"
if [ -n "$CUR_BASE" ]; then
  echo "  current base: $(git rev-parse --short "$CUR_BASE")"
  echo "  upstream commits to pull in:"
  git log --oneline "$CUR_BASE..$BASE" | sed 's/^/    /' || true
  n=$(git rev-list --count "$CUR_BASE..$BASE")
  echo "  total: $n commit(s)"
else
  echo "  (no merge-base found; histories may be unrelated)"
fi

if [ "$DRY_RUN" = 1 ]; then
  hr "dry-run: no rebase performed"
  exit 0
fi

hr "3. backup"
STAMP="$(date +%Y%m%d-%H%M%S)"
BACKUP="$FORK_BRANCH-backup-$STAMP"
git branch -f "$BACKUP" "$FORK_BRANCH"
echo "  backed up $FORK_BRANCH -> $BACKUP"

hr "4. rebase $FORK_BRANCH onto $BASE"
CUR_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$CUR_BRANCH" != "$FORK_BRANCH" ]; then
  echo "  checking out $FORK_BRANCH (was on $CUR_BRANCH)"
  git checkout "$FORK_BRANCH"
fi

# Expected conflict hot-spots: small registration lines in shared files.
echo "  running: git rebase $BASE"
if git rebase "$BASE"; then
  hr "rebase clean"
else
  hr "conflicts — resolve, then 'git rebase --continue'"
  echo "  conflicting files:"
  git diff --name-only --diff-filter=U | sed 's/^/    /'
  echo
  echo "  to finish : git add <files> && git rebase --continue"
  echo "  to bail   : git rebase --abort   (restores $BACKUP state)"
  exit 3
fi

hr "5. post-rebase check"
echo "  fork branch now: $(git log --oneline -1 "$FORK_BRANCH")"
echo "  base is now    : $(git merge-base "$FORK_BRANCH" "$BASE" | cut -c1-12)"
if [ "$RUN_TEST" = 1 ]; then
  echo
  echo "  run the test suite before merging (adjust env for your build box):"
  echo "    cargo test -p pdm-api-types -p pdm-config -p server"
  echo "  Then fast-forward nothing; 'omada' already points at the rebased history."
  echo "  If you want to discard: git reset --hard $BACKUP"
fi
hr "done"
echo "  backup branch: $BACKUP  (delete with: git branch -D $BACKUP)"
