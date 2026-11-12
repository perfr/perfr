#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Checks release.sh with shellcheck, then runs it against a temporary
# origin: dry-run, a real tag, and the cases that must refuse.
# No network and no GitHub credentials.

set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
RELEASE=$ROOT/tools/release/release.sh
CHECK_TAG=$ROOT/tools/release/check_tag.sh
TEST_NAME=""
output=""
status=0
MOCK_BIN=""
MOCK_LOG=""

fail() {
  printf 'FAIL: %s: %s\n' "$TEST_NAME" "$*" >&2
  exit 1
}

it() {
  TEST_NAME=$1
  printf '  %s\n' "$1"
}

assert_ok() {
  if [ "$status" -ne 0 ]; then
    fail "expected success, got $status"$'\n'"$output"
  fi
}

assert_fail() {
  if [ "$status" -eq 0 ]; then
    fail "expected failure"$'\n'"$output"
  fi
}

assert_has() {
  case "$output" in
    *"$1"*) ;;
    *) fail "missing [$1]"$'\n'"$output" ;;
  esac
}

assert_lacks() {
  case "$output" in
    *"$1"*) fail "unexpected [$1]"$'\n'"$output" ;;
  esac
}

if ! command -v shellcheck >/dev/null 2>&1; then
  echo "shellcheck is not installed" >&2
  exit 1
fi

echo "shellcheck"
shellcheck "$RELEASE"
shellcheck "$CHECK_TAG"
shellcheck "$0"

# shellcheck disable=SC1090,SC1091
source "$RELEASE"

it "semver orders releases above prereleases and rc numbers numerically"
version_gt "1.2.4" "1.2.3" || fail "patch"
version_gt "1.3.0" "1.2.9" || fail "minor"
version_gt "2.0.0" "1.9.9" || fail "major"
version_gt "1.0.0" "1.0.0-rc.1" || fail "release > rc"
version_gt "1.0.0-rc.10" "1.0.0-rc.2" || fail "rc.10 > rc.2"
version_gt "1.0.0-rc.2" "1.0.0-rc.1" || fail "rc.2 > rc.1"
version_gt "1.0.0-rc.1" "1.0.0-alpha" || fail "rc > alpha"
version_gt "1.0.0-rc.1.1" "1.0.0-rc.1" || fail "longer prerelease"
if version_gt "1.0.0-rc.2" "1.0.0-rc.10"; then
  fail "rc.2 should not beat rc.10"
fi
if version_gt "1.0.0" "1.0.0"; then
  fail "equal versions are not greater"
fi
if version_gt "1.0.0-rc.1" "1.0.0"; then
  fail "rc should not beat its release"
fi
is_exact_version "0.1.1" || fail "0.1.1"
is_exact_version "0.1.1-rc.3" || fail "rc"
if is_exact_version "01.2.3"; then
  fail "leading zero"
fi
if is_exact_version "1.2.3-beta.1"; then
  fail "only rc suffixes are created"
fi
is_orderable_version "1.2.3-beta.1" || fail "beta still orders"
if is_orderable_version "dev"; then
  fail "dev is not a version"
fi

write_tree() {
  local dir=$1 ver=$2
  mkdir -p \
    "$dir/crates/perfr-api/python/perfr_api" \
    "$dir/crates/perfr/python/perfr"
  cat >"$dir/crates/perfr-api/Cargo.toml" <<EOF
[package]
name = "perfr-api"
version = "$ver"
EOF
  cat >"$dir/crates/perfr-api/python/pyproject.toml" <<EOF
[project]
name = "perfr-api"
version = "$ver"
EOF
  printf '__version__ = "%s"\n' "$ver" >"$dir/crates/perfr-api/python/perfr_api/__init__.py"
  cat >"$dir/crates/perfr/Cargo.toml" <<EOF
[package]
name = "perfr"
version = "$ver"
EOF
  cat >"$dir/crates/perfr/python/pyproject.toml" <<EOF
[project]
name = "perfr"
version = "$ver"
dependencies = ["perfr-api==$ver"]
EOF
  printf '__version__ = "%s"\n' "$ver" >"$dir/crates/perfr/python/perfr/__init__.py"
  write_lock "$dir/Cargo.lock" "$ver"
}

# The lock is cut from this repository's Cargo.lock: its header and its
# perfr and perfr-api entries, with only their versions replaced. A lock
# written out by hand here said "version = 3" and kept every test green
# while release.sh refused the lock the repository really has. ndk-context
# is the one invented entry: an unrelated package at a version perfr has
# also had, which a rewrite must leave alone.
write_lock() {
  local out=$1 ver=$2
  awk -v ver="$ver" '
    BEGIN { RS = ""; FS = "\n" }
    NR == 1 {
      print
      print ""
      print "[[package]]"
      print "name = \"ndk-context\""
      print "version = \"0.1.1\""
      print "source = \"registry+https://github.com/rust-lang/crates.io-index\""
      next
    }
    $2 == "name = \"perfr\"" || $2 == "name = \"perfr-api\"" {
      print ""
      for (i = 1; i <= NF; i++) {
        if ($i ~ /^version = "/) {
          print "version = \"" ver "\""
        } else {
          print $i
        }
      }
    }
  ' "$ROOT/Cargo.lock" >"$out"
  if [ "$(grep -c '^\[\[package\]\]$' "$out")" -ne 3 ] ||
    ! grep -q '^version = [0-9]*$' "$out"; then
    fail "could not cut the header and the perfr and perfr-api entries out of $ROOT/Cargo.lock"
  fi
}

make_repo() {
  local dir=$1 ver=$2 bare
  write_tree "$dir" "$ver"
  git -C "$dir" init -b main >/dev/null
  git -C "$dir" config user.email test@example.com
  git -C "$dir" config user.name Test
  git -C "$dir" config commit.gpgsign false
  git -C "$dir" config tag.gpgSign false
  git -C "$dir" add -A
  git -C "$dir" commit -m "init fixture" >/dev/null
  bare=${dir}.git
  git init --bare -b main "$bare" >/dev/null
  git -C "$dir" remote add origin "$bare"
  git -C "$dir" push -q -u origin main
  git -C "$dir" tag v1.9.9
  git -C "$dir" tag nightly-2026-01-01
  git -C "$dir" tag perfr-vnope
  git -C "$dir" tag perfr-v0.1.0
  git -C "$dir" tag perfr-v0.1.1-rc.2
  git -C "$dir" push -q origin \
    v1.9.9 nightly-2026-01-01 perfr-vnope perfr-v0.1.0 perfr-v0.1.1-rc.2
  git -C "$dir" commit --allow-empty -m "after tags" >/dev/null
  git -C "$dir" push -q origin main
}

install_mock() {
  MOCK_BIN=$(mktemp -d)
  MOCK_LOG=$MOCK_BIN/gh.log
  cat >"$MOCK_BIN/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ -n "${GH_MOCK_LOG:-}" ]; then
  printf '%s\n' "$*" >>"$GH_MOCK_LOG"
fi
mode=${GH_MOCK_MODE:-green}
cmd=$1
shift || true
case "$cmd" in
  auth)
    if [ "$mode" = "noauth" ]; then
      echo "not logged in" >&2
      exit 1
    fi
    exit 0
    ;;
  repo)
    printf '%s\n' "test/perfr"
    ;;
  api)
    url=""
    for arg in "$@"; do
      case "$arg" in
        repos/*) url=$arg ;;
      esac
    done
    case "$url" in
      */check-runs)
        case "$mode" in
          green)
            printf 'check\tlinux\tcompleted\tsuccess\n'
            printf 'check\toptional\tcompleted\tskipped\n'
            ;;
          fail) printf 'check\tlinux\tcompleted\tfailure\n' ;;
          pending) printf 'check\twindows\tqueued\t\n' ;;
          none) ;;
          *) printf 'check\tlinux\tcompleted\tsuccess\n' ;;
        esac
        ;;
      */status)
        case "$mode" in
          green) printf 'status\tci/tests\tsuccess\n' ;;
          fail) printf 'status\tci/tests\tfailure\n' ;;
          pending) printf 'status\tci/tests\tpending\n' ;;
          none) ;;
          *) printf 'status\tci/tests\tsuccess\n' ;;
        esac
        ;;
      *)
        echo "mock gh api unexpected: $url" >&2
        exit 1
        ;;
    esac
    ;;
  run)
    tag=${GH_MOCK_TAG:-}
    if [ -z "$tag" ]; then
      exit 0
    fi
    wf=""
    prev=""
    for arg in "$@"; do
      if [ "$prev" = "--workflow" ]; then
        wf=$arg
      fi
      prev=$arg
    done
    case "$wf" in
      release-rust.yml) id=123 ;;
      python-wheels.yml) id=456 ;;
      *) id=789 ;;
    esac
    printf '%s\thttps://github.com/test/perfr/actions/runs/%s\n' "$tag" "$id"
    ;;
  pr)
    printf '%s\n' "https://github.com/test/perfr/pull/7"
    ;;
  *)
    echo "mock gh unexpected command: $cmd" >&2
    exit 1
    ;;
esac
EOF
  chmod +x "$MOCK_BIN/gh"
}

run_release() {
  local repo=$1
  shift
  set +e
  # RELEASE_BASH, when set, is the interpreter (bash 3.2, the macOS bash).
  output=$(
    cd "$repo" &&
      GIT_AUTHOR_NAME=Wrong \
        GIT_AUTHOR_EMAIL=wrong@example.com \
        GIT_COMMITTER_NAME=Wrong \
        GIT_COMMITTER_EMAIL=wrong@example.com \
        GH_MOCK_LOG="$MOCK_LOG" \
        GH_MOCK_MODE="${GH_MOCK_MODE:-green}" \
        GH_MOCK_TAG="${GH_MOCK_TAG:-}" \
        PERFR_RELEASE_POLL_ATTEMPTS="${PERFR_RELEASE_POLL_ATTEMPTS:-1}" \
        PERFR_RELEASE_POLL_SLEEP="${PERFR_RELEASE_POLL_SLEEP:-0}" \
        PATH="$MOCK_BIN:$PATH" \
        ${RELEASE_BASH:+"$RELEASE_BASH"} "$RELEASE" "$@" 2>&1
  )
  status=$?
  set -e
}

TMP=$(mktemp -d)
trap 'rm -rf "$TMP" "$MOCK_BIN"' EXIT
install_mock

it "accepts the Cargo.lock this repository has"
set +e
output=$(cd "$ROOT" && require_version_files 2>&1)
status=$?
set -e
assert_ok
(cd "$ROOT" && lock_package_version perfr >/dev/null) || fail "no perfr entry in the real lock"
(cd "$ROOT" && lock_package_version perfr-api >/dev/null) || fail "no perfr-api entry in the real lock"

it "rewrites only the perfr and perfr-api versions of the real Cargo.lock"
REAL_LOCK=$TMP/real-lock
mkdir "$REAL_LOCK"
cp "$ROOT/Cargo.lock" "$REAL_LOCK/Cargo.lock"
(cd "$REAL_LOCK" && update_cargo_lock 98.76.54) || fail "update_cargo_lock failed on the real lock"
[ "$(cd "$REAL_LOCK" && lock_package_version perfr)" = "98.76.54" ] || fail "lock perfr"
[ "$(cd "$REAL_LOCK" && lock_package_version perfr-api)" = "98.76.54" ] || fail "lock api"
lock_diff=$(diff "$ROOT/Cargo.lock" "$REAL_LOCK/Cargo.lock" | grep '^[<>]' || true)
[ "$(printf '%s\n' "$lock_diff" | grep -c '^< version = "')" -eq 2 ] ||
  fail "expected two replaced version lines"$'\n'"$lock_diff"
[ "$(printf '%s\n' "$lock_diff" | grep -cx '> version = "98.76.54"')" -eq 2 ] ||
  fail "expected two new version lines"$'\n'"$lock_diff"
[ "$(printf '%s\n' "$lock_diff" | grep -c '')" -eq 4 ] ||
  fail "the rewrite touched other lines"$'\n'"$lock_diff"

it "refuses a Cargo.lock format it was not checked against"
NEWER_LOCK=$TMP/newer-lock
write_tree "$NEWER_LOCK" "0.1.0"
sed 's/^version = [0-9]*$/version = 5/' "$NEWER_LOCK/Cargo.lock" >"$NEWER_LOCK/Cargo.lock.new"
mv "$NEWER_LOCK/Cargo.lock.new" "$NEWER_LOCK/Cargo.lock"
grep -qx 'version = 5' "$NEWER_LOCK/Cargo.lock" || fail "fixture has no lock format line"
set +e
output=$(cd "$NEWER_LOCK" && require_version_files 2>&1)
status=$?
set -e
assert_fail
assert_has "refusing to edit it"

it "no tracked file is stored with line endings its attributes forbid"
# Git reports such a file as modified in every fresh checkout, nobody can
# make it clean without committing it, and release.sh refuses a tree that
# is not clean. text=auto is left out: it does not convert a file that is
# already stored with CRLF.
if git -C "$ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  unclean=$(git -C "$ROOT" ls-files --eol |
    grep -E '^i/(crlf|mixed) +w/[^ ]* +attr/text[[:space:]]' || true)
  [ -z "$unclean" ] || fail "stored with CRLF but marked text"$'\n'"$unclean"
fi

REPO=$TMP/repo
make_repo "$REPO" "0.1.0"

it "dry-run patch ignores legacy tags and prerelease suffixes"
GH_MOCK_MODE=green
run_release "$REPO" --dry-run --patch
assert_ok
assert_has "latest: perfr-v0.1.1-rc.2"
assert_has "version: 0.1.0 -> 0.1.2"
assert_has "tag: perfr-v0.1.2"
assert_has "after tags"
assert_has "dry-run: would commit \"Release perfr 0.1.2\" as Pierric Gimmig <pierric.gimmig@gmail.com>"
assert_has "dry-run: not committing, tagging, or pushing."
assert_has "ignoring perfr-vnope"
assert_lacks "v1.9.9"
git -C "$REPO" tag --list 'perfr-v0.1.2' | grep -q . && fail "dry-run created a tag"
git -C "$REPO" status --porcelain | grep -q . && fail "dry-run dirtied the tree"

it "minor, major, and rc numbers"
run_release "$REPO" --dry-run --minor
assert_ok
assert_has "version: 0.1.0 -> 0.2.0"
run_release "$REPO" --dry-run --major
assert_ok
assert_has "version: 0.1.0 -> 1.0.0"
run_release "$REPO" --dry-run --patch --rc
assert_ok
assert_has "tag: perfr-v0.1.2-rc.1"
assert_has "prerelease, not Latest"
run_release "$REPO" --dry-run --version 0.1.1 --rc
assert_ok
assert_has "version: 0.1.0 -> 0.1.1-rc.3"
assert_has "tag: perfr-v0.1.1-rc.3"
run_release "$REPO" --dry-run --version 0.1.1
assert_ok
assert_has "version: 0.1.0 -> 0.1.1"
assert_has "GitHub will mark this Latest."
run_release "$REPO" --dry-run --version 0.1.1-rc.4
assert_ok
assert_has "tag: perfr-v0.1.1-rc.4"

it "refuses a downgrade, a repeat, and a bad flag"
run_release "$REPO" --dry-run --version 0.1.1-rc.1
assert_fail
assert_has "not newer than the latest tag"
run_release "$REPO" --dry-run --version 0.1.0
assert_fail
assert_has "not newer"
run_release "$REPO" --patch --minor
assert_fail
assert_has "only one of"
run_release "$REPO" --dry-run --version 0.1.2-rc.1 --rc
assert_fail
assert_has "not both"
run_release "$REPO" --dry-run --version 01.2.3
assert_fail
assert_has "leading zeros"
run_release "$REPO" --help
assert_ok
assert_has "usage:"

it "an empty answer selects patch"
run_release "$REPO" --dry-run <<EOF

EOF
assert_ok
assert_has "version: 0.1.0 -> 0.1.2"

it "refuses a dirty tree and another branch, even with --force"
echo stray >"$REPO/stray.txt"
run_release "$REPO" --dry-run --patch --force
assert_fail
assert_has "working tree is not clean"
assert_has "stray.txt"
rm "$REPO/stray.txt"
git -C "$REPO" checkout -q -b feature
run_release "$REPO" --dry-run --patch
assert_fail
assert_has "not main"
git -C "$REPO" checkout -q main
git -C "$REPO" branch -D feature >/dev/null

it "refuses a committed version drift"
DRIFT=$TMP/drift
make_repo "$DRIFT" "0.1.0"
printf '__version__ = "9.9.9"\n' >"$DRIFT/crates/perfr/python/perfr/__init__.py"
git -C "$DRIFT" add crates/perfr/python/perfr/__init__.py
git -C "$DRIFT" commit -q -m "drift"
git -C "$DRIFT" push -q origin main
run_release "$DRIFT" --dry-run --patch
assert_fail
assert_has "do not match"
assert_has "__version__"

it "refuses when local main is not origin/main"
DIVERGED=$TMP/diverged
make_repo "$DIVERGED" "0.1.0"
git -C "$DIVERGED" commit --allow-empty -m "only on origin" >/dev/null
git -C "$DIVERGED" push -q origin main
git -C "$DIVERGED" reset --hard HEAD~1 >/dev/null
run_release "$DIVERGED" --dry-run --patch
assert_fail
assert_has "local main is not origin/main"
assert_has "behind"
assert_has "will not merge, rebase, or reset"

it "refuses failing, pending, and missing CI unless --force"
GH_MOCK_MODE=fail
run_release "$REPO" --dry-run --patch
assert_fail
assert_has "not green"
assert_has "linux (failure)"
GH_MOCK_MODE=pending
run_release "$REPO" --dry-run --patch
assert_fail
assert_has "pending:"
assert_has "windows (queued)"
GH_MOCK_MODE=none
run_release "$REPO" --dry-run --patch
assert_fail
assert_has "no successful check"
GH_MOCK_MODE=fail
run_release "$REPO" --dry-run --patch --force
assert_ok
assert_has "not green"
assert_has "--force skips only the CI check"
GH_MOCK_MODE=noauth
run_release "$REPO" --dry-run --patch --force
assert_fail
assert_has "not authenticated"
GH_MOCK_MODE=green

it "n does not commit or tag"
before=$(git -C "$REPO" rev-parse HEAD)
run_release "$REPO" --patch <<EOF
n
EOF
assert_fail
assert_has "confirmation was not y"
assert_has "Nothing was committed or tagged"
[ "$(git -C "$REPO" rev-parse HEAD)" = "$before" ] || fail "HEAD moved"
git -C "$REPO" status --porcelain | grep -q . && fail "tree dirty after n"

it "tags a release and refuses to repeat that tag"
GH_MOCK_TAG=perfr-v0.1.2
run_release "$REPO" --patch <<EOF
y
EOF
assert_ok
assert_has "https://github.com/test/perfr/actions/runs/123"
assert_has "https://github.com/test/perfr/actions/runs/456"
assert_has "https://github.com/test/perfr/releases/tag/perfr-v0.1.2"
meta=$(git -C "$REPO" log -1 --format='%an|%ae|%cn|%ce|%s')
[ "$meta" = "Pierric Gimmig|pierric.gimmig@gmail.com|Pierric Gimmig|pierric.gimmig@gmail.com|Release perfr 0.1.2" ] ||
  fail "commit identity: $meta"
[ "$(git -C "$REPO" cat-file -t perfr-v0.1.2)" = "tag" ] || fail "tag is not annotated"
got=$(git -C "$REPO" for-each-ref refs/tags/perfr-v0.1.2 --format='%(taggername)|%(taggeremail)|%(contents:subject)')
[ "$got" = "Pierric Gimmig|<pierric.gimmig@gmail.com>|Release perfr 0.1.2" ] ||
  fail "tagger: $got"
git -C "$REPO" ls-remote --tags origin refs/tags/perfr-v0.1.2 | grep -q perfr-v0.1.2 ||
  fail "tag was not pushed"
grep -q 'version = "0.1.2"' "$REPO/crates/perfr/Cargo.toml" || fail "perfr cargo version"
grep -q 'version = "0.1.2"' "$REPO/crates/perfr-api/Cargo.toml" || fail "api cargo version"
grep -q '__version__ = "0.1.2"' "$REPO/crates/perfr/python/perfr/__init__.py" || fail "svc version"
grep -q '__version__ = "0.1.2"' "$REPO/crates/perfr-api/python/perfr_api/__init__.py" || fail "api version"
grep -q 'perfr-api==0.1.2' "$REPO/crates/perfr/python/pyproject.toml" || fail "pin"
grep -q 'name = "ndk-context"' "$REPO/Cargo.lock" || fail "lock lost ndk"
grep -A1 'name = "ndk-context"' "$REPO/Cargo.lock" | grep -q 'version = "0.1.1"' ||
  fail "unrelated lock version changed"
grep -A1 'name = "perfr"$' "$REPO/Cargo.lock" | grep -q 'version = "0.1.2"' || fail "lock perfr"
grep -A1 'name = "perfr-api"' "$REPO/Cargo.lock" | grep -q 'version = "0.1.2"' || fail "lock api"
run_release "$REPO" --dry-run --version 0.1.2
assert_fail
assert_has "not newer than the latest tag"
run_release "$REPO" --dry-run --patch
assert_ok
assert_has "version: 0.1.2 -> 0.1.3"

it "a tag that exists on origin is refused after fetch"
ONLY_REMOTE=$TMP/only-remote
make_repo "$ONLY_REMOTE" "0.1.0"
git -C "$ONLY_REMOTE" tag perfr-v0.2.0
git -C "$ONLY_REMOTE" push -q origin refs/tags/perfr-v0.2.0
git -C "$ONLY_REMOTE" tag -d perfr-v0.2.0 >/dev/null
run_release "$ONLY_REMOTE" --dry-run --version 0.2.0
assert_fail
assert_has "not newer than the latest tag perfr-v0.2.0"
set +e
output=$(cd "$ONLY_REMOTE" && git tag -d perfr-v0.2.0 >/dev/null && refuse_if_tag_exists perfr-v0.2.0 2>&1)
status=$?
set -e
assert_fail
assert_has "already exists"
assert_has "on origin"

it "a local tag that was never pushed is refused and the push command is printed"
LOCAL_ONLY=$TMP/local-only
make_repo "$LOCAL_ONLY" "0.1.0"
git -C "$LOCAL_ONLY" tag perfr-v0.2.0
set +e
output=$(cd "$LOCAL_ONLY" && refuse_if_tag_exists perfr-v0.2.0 2>&1)
status=$?
set -e
assert_fail
assert_has "already exists"
assert_has "locally but not on origin"
assert_has "git push origin refs/tags/perfr-v0.2.0"

it "protected main opens a pull request and does not tag"
PROTECTED=$TMP/protected
make_repo "$PROTECTED" "0.1.0"
cat >"${PROTECTED}.git/hooks/pre-receive" <<'EOF'
#!/bin/bash
while read -r _old _new ref; do
  case "$ref" in
    refs/heads/main)
      echo "remote: error: GH006: Protected branch update failed for refs/heads/main." >&2
      echo "remote: error: Changes must be made through a pull request." >&2
      exit 1
      ;;
  esac
done
EOF
chmod +x "${PROTECTED}.git/hooks/pre-receive"
GH_MOCK_TAG=perfr-v0.1.2
run_release "$PROTECTED" --patch <<EOF
y
EOF
assert_fail
assert_has "https://github.com/test/perfr/pull/7"
assert_has "No tag was created"
assert_has "tools/release/release.sh --version 0.1.2"
git -C "$PROTECTED" rev-parse --verify --quiet refs/tags/perfr-v0.1.2 >/dev/null &&
  fail "protected run created a tag"
git -C "${PROTECTED}.git" rev-parse --verify --quiet refs/heads/main >/dev/null
local_main=$(git -C "$PROTECTED" rev-parse main)
origin_main=$(git -C "${PROTECTED}.git" rev-parse refs/heads/main)
[ "$local_main" != "$origin_main" ] || fail "main was pushed despite the hook"
git -C "${PROTECTED}.git" rev-parse --verify --quiet refs/heads/release/perfr-v0.1.2 >/dev/null ||
  fail "release branch was not pushed"
grep -q 'Release perfr 0.1.2' "$MOCK_LOG" || fail "pr title was not passed to gh"
[ "$(git -C "$PROTECTED" log -1 --format='%an <%ae> %s')" = \
  "Pierric Gimmig <pierric.gimmig@gmail.com> Release perfr 0.1.2" ] ||
  fail "protected commit author"

it "a non-fast-forward rejection does not open a pull request"
NONFF=$TMP/nonff
make_repo "$NONFF" "0.1.0"
cat >"${NONFF}.git/hooks/pre-receive" <<'EOF'
#!/bin/bash
while read -r _old _new ref; do
  case "$ref" in
    refs/heads/main)
      echo "! [rejected] main -> main (non-fast-forward)" >&2
      echo "error: failed to push some refs" >&2
      exit 1
      ;;
  esac
done
EOF
chmod +x "${NONFF}.git/hooks/pre-receive"
: >"$MOCK_LOG"
run_release "$NONFF" --patch <<EOF
y
EOF
assert_fail
assert_has "git push origin main failed"
assert_has "non-fast-forward"
assert_lacks "Pull request:"
git -C "$NONFF" rev-parse --verify --quiet refs/tags/perfr-v0.1.2 >/dev/null &&
  fail "non-ff run created a tag"
[ "$(git -C "$NONFF" log -1 --format='%s')" = "Release perfr 0.1.2" ] ||
  fail "release commit should stay on local main"

it "no previous tag can release the stamped version or bump it"
FRESH=$TMP/fresh
write_tree "$FRESH" "0.1.0"
git -C "$FRESH" init -b main >/dev/null
git -C "$FRESH" config user.email test@example.com
git -C "$FRESH" config user.name Test
git -C "$FRESH" config commit.gpgsign false
git -C "$FRESH" config tag.gpgSign false
git -C "$FRESH" add -A
git -C "$FRESH" commit -m "init fixture" >/dev/null
git init --bare -b main "$FRESH.git" >/dev/null
git -C "$FRESH" remote add origin "$FRESH.git"
git -C "$FRESH" push -q -u origin main
git -C "$FRESH" tag v1.2.3
git -C "$FRESH" tag nightly-1
git -C "$FRESH" push -q origin v1.2.3 nightly-1
run_release "$FRESH" --dry-run --version 0.1.0
assert_ok
assert_has "latest: (no perfr-vX.Y.Z tag)"
assert_has "version: 0.1.0 -> 0.1.0"
assert_has "already match 0.1.0; no commit"
run_release "$FRESH" --dry-run --patch --rc
assert_ok
assert_has "tag: perfr-v0.1.1-rc.1"
run_release "$FRESH" --dry-run --version 0.0.9
assert_fail
assert_has "older than the version already in the tree"

it "a tag must name the version stamped in the tree"
TAGGED=$TMP/tagged
write_tree "$TAGGED" "0.1.1"
status=0
output=$("$CHECK_TAG" perfr-v0.1.1 "$TAGGED" 2>&1) || status=$?
assert_ok
# The tag that was pushed by hand onto a tree still stamped 0.1.1.
status=0
output=$("$CHECK_TAG" perfr-v0.1.2 "$TAGGED" 2>&1) || status=$?
assert_fail
assert_has "does not match the stamped version 0.1.1"
status=0
output=$("$CHECK_TAG" v0.1.1 "$TAGGED" 2>&1) || status=$?
assert_fail
write_tree "$TAGGED" "0.2.0-rc.1"
status=0
output=$("$CHECK_TAG" perfr-v0.2.0-rc.1 "$TAGGED" 2>&1) || status=$?
assert_ok
status=0
output=$("$CHECK_TAG" perfr-v0.2.0 "$TAGGED" 2>&1) || status=$?
assert_fail

echo "ok"
