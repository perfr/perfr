#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Cut a perfr release by pushing an annotated perfr-vX.Y.Z tag (or
# perfr-vX.Y.Z-rc.N). That push is what starts release-rust.yml and
# python-wheels.yml. This script never merges, rebases, resets, force-pushes,
# or deletes a tag. See docs/releasing.md.
#
#   tools/release/release.sh                 # ask: patch / minor / major
#   tools/release/release.sh --patch --rc
#   tools/release/release.sh --version 0.2.0
#   tools/release/release.sh --dry-run
#
# Versions that must move together (build_wheels.sh refuses a drift):
#   crates/perfr-api/Cargo.toml                          version
#   crates/perfr-api/python/pyproject.toml               version
#   crates/perfr/python/pyproject.toml                   version and perfr-api==
#   crates/perfr-api/python/perfr_api/__init__.py        __version__
#   crates/perfr/python/perfr/__init__.py                __version__
# crates/perfr/Cargo.toml is included because `perfr --version` and
# `perfr update` read CARGO_PKG_VERSION. Cargo.lock's perfr and perfr-api
# package entries are updated so --locked builds keep working. Other crates
# are not release versions. The install scripts do not embed a version.

# Bash 3.2 (the bash macOS ships) is enough: no associative arrays, no
# mapfile, no ${var,,}. LC_ALL=C keeps version compares byte-wise.
if [ -z "${BASH_VERSION:-}" ]; then
  echo "error: tools/release/release.sh must be run with bash" >&2
  exit 1
fi

set -euo pipefail
export LC_ALL=C
export GIT_TERMINAL_PROMPT=0
export GH_PROMPT_DISABLED=1

RELEASE_AUTHOR_NAME="Pierric Gimmig"
RELEASE_AUTHOR_EMAIL="pierric.gimmig@gmail.com"

POLL_ATTEMPTS="${PERFR_RELEASE_POLL_ATTEMPTS:-6}"
POLL_SLEEP="${PERFR_RELEASE_POLL_SLEEP:-2}"

die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

warn() {
  printf 'warning: %s\n' "$1" >&2
}

usage() {
  cat <<'EOF'
usage: tools/release/release.sh [options]

Cut a perfr release. The release is an annotated tag perfr-vX.Y.Z, or
perfr-vX.Y.Z-rc.N with --rc. Pushing that tag starts release-rust.yml
and python-wheels.yml. docs/releasing.md describes the workflows.

Options:
  --patch            bump the patch number (the interactive default)
  --minor            bump the minor number and set patch to 0
  --major            bump the major number and set minor and patch to 0
  --version X.Y.Z    use this version (X.Y.Z-rc.N is also accepted)
  --rc               publish perfr-vX.Y.Z-rc.N instead of a stable tag
  --force            skip the CI-is-green check, and nothing else
  --dry-run          run every check and print the summary; do not
                     commit, tag, or push, and do not ask for confirmation
  -h, --help         show this help

The bump applies to the X.Y.Z of the latest perfr-v* tag, ignoring a
prerelease suffix and ignoring legacy v1.* and nightly-* tags. The latest
tag perfr-v1.2.3-rc.2 with --patch is 1.2.4. Pass --version 1.2.3 to
publish that stable release, or --version 1.2.3 --rc to continue at rc.3.
With no perfr-v* tag yet, the bump starts from the version already stamped
in the tree; pass --version to tag that version without bumping it.

The script stops unless the worktree is clean, HEAD is main, and local
main equals origin/main after `git fetch --tags origin`. It asks for an
explicit y before it commits, tags, or pushes. If main rejects the push,
it opens a pull request and does not create the tag.
EOF
}

# Run git commit / git tag as the release author without touching git config.
as_release_author() {
  GIT_AUTHOR_NAME=$RELEASE_AUTHOR_NAME \
    GIT_AUTHOR_EMAIL=$RELEASE_AUTHOR_EMAIL \
    GIT_COMMITTER_NAME=$RELEASE_AUTHOR_NAME \
    GIT_COMMITTER_EMAIL=$RELEASE_AUTHOR_EMAIL \
    "$@"
}

is_canonical_number() {
  local n=$1
  # Digits only. A leading zero is allowed solely for the number 0.
  # In a case pattern, [0-9]* is "a digit, then anything", so the
  # non-digit rejection above it has to come first.
  case "$n" in
    "" | *[!0-9]*) return 1 ;;
  esac
  case "$n" in
    0 | [1-9]*) return 0 ;;
    *) return 1 ;;
  esac
}

# Identifiers in a prerelease: semver dot-separated, no leading zeros.
is_pre_ident() {
  local ident=$1
  case "$ident" in
    "" | *[!0-9A-Za-z-]*) return 1 ;;
  esac
  case "$ident" in
    *[!0-9]*) return 0 ;;
  esac
  is_canonical_number "$ident"
}

# X.Y.Z or X.Y.Z-rc.N, the only versions this script will create.
is_exact_version() {
  local v=$1 major minor patch rest n
  case "$v" in
    *-*)
      rest=${v#*-}
      v=${v%%-*}
      case "$rest" in
        rc.*) ;;
        *) return 1 ;;
      esac
      n=${rest#rc.}
      is_canonical_number "$n" || return 1
      ;;
  esac
  case "$v" in
    *.*.*) ;;
    *) return 1 ;;
  esac
  major=${v%%.*}
  patch=${v##*.}
  minor=${v#*.}
  minor=${minor%.*}
  is_canonical_number "$major" || return 1
  is_canonical_number "$minor" || return 1
  is_canonical_number "$patch" || return 1
}

# Any perfr-v* tag we are willing to order: X.Y.Z with an optional
# semver prerelease. Legacy v1.* and nightly-* never match this.
is_orderable_version() {
  local v=$1 major minor patch pre ident
  case "$v" in
    *-*)
      pre=${v#*-}
      v=${v%%-*}
      ;;
    *) pre="" ;;
  esac
  case "$v" in
    *.*.*) ;;
    *) return 1 ;;
  esac
  major=${v%%.*}
  patch=${v##*.}
  minor=${v#*.}
  minor=${minor%.*}
  case "$minor" in
    *.*) return 1 ;;
  esac
  is_canonical_number "$major" || return 1
  is_canonical_number "$minor" || return 1
  is_canonical_number "$patch" || return 1
  if [ -z "$pre" ]; then
    return 0
  fi
  while [ -n "$pre" ]; do
    case "$pre" in
      *.*)
        ident=${pre%%.*}
        pre=${pre#*.}
        ;;
      *)
        ident=$pre
        pre=""
        ;;
    esac
    is_pre_ident "$ident" || return 1
  done
}

# Sets SV_MAJ SV_MIN SV_PAT SV_PRE. Callers copy the values out immediately.
split_ver() {
  local v=$1
  SV_PRE=""
  case "$v" in
    *-*)
      SV_PRE=${v#*-}
      v=${v%%-*}
      ;;
  esac
  SV_MAJ=${v%%.*}
  SV_PAT=${v##*.}
  SV_MIN=${v#*.}
  SV_MIN=${SV_MIN%.*}
}

ident_gt() {
  local a=$1 b=$2 a_num=1 b_num=1
  case "$a" in
    *[!0-9]*) a_num=0 ;;
  esac
  case "$b" in
    *[!0-9]*) b_num=0 ;;
  esac
  if [ "$a_num" -eq 1 ] && [ "$b_num" -eq 1 ]; then
    [ "$a" -gt "$b" ]
    return
  fi
  if [ "$a_num" -eq 1 ]; then
    return 1
  fi
  if [ "$b_num" -eq 1 ]; then
    return 0
  fi
  [[ "$a" > "$b" ]]
}

pre_gt() {
  local a=$1 b=$2 ai bi
  while [ -n "$a" ] || [ -n "$b" ]; do
    if [ -z "$a" ]; then
      return 1
    fi
    if [ -z "$b" ]; then
      return 0
    fi
    case "$a" in
      *.*)
        ai=${a%%.*}
        a=${a#*.}
        ;;
      *)
        ai=$a
        a=""
        ;;
    esac
    case "$b" in
      *.*)
        bi=${b%%.*}
        b=${b#*.}
        ;;
      *)
        bi=$b
        b=""
        ;;
    esac
    if [ "$ai" = "$bi" ]; then
      continue
    fi
    ident_gt "$ai" "$bi"
    return
  done
  return 1
}

# True when $1 is strictly greater than $2 (semver, prerelease < release).
version_gt() {
  local a=$1 b=$2
  local a_maj a_min a_pat a_pre b_maj b_min b_pat b_pre
  split_ver "$a"
  a_maj=$SV_MAJ
  a_min=$SV_MIN
  a_pat=$SV_PAT
  a_pre=$SV_PRE
  split_ver "$b"
  b_maj=$SV_MAJ
  b_min=$SV_MIN
  b_pat=$SV_PAT
  b_pre=$SV_PRE
  if [ "$a_maj" -ne "$b_maj" ]; then
    [ "$a_maj" -gt "$b_maj" ]
    return
  fi
  if [ "$a_min" -ne "$b_min" ]; then
    [ "$a_min" -gt "$b_min" ]
    return
  fi
  if [ "$a_pat" -ne "$b_pat" ]; then
    [ "$a_pat" -gt "$b_pat" ]
    return
  fi
  if [ -z "$a_pre" ] && [ -z "$b_pre" ]; then
    return 1
  fi
  if [ -z "$a_pre" ]; then
    return 0
  fi
  if [ -z "$b_pre" ]; then
    return 1
  fi
  pre_gt "$a_pre" "$b_pre"
}

numeric_triple() {
  split_ver "$1"
  printf '%s.%s.%s\n' "$SV_MAJ" "$SV_MIN" "$SV_PAT"
}

bump_triple() {
  local kind=$1
  local maj=$2 min=$3 pat=$4
  case "$kind" in
    patch) pat=$((pat + 1)) ;;
    minor)
      min=$((min + 1))
      pat=0
      ;;
    major)
      maj=$((maj + 1))
      min=0
      pat=0
      ;;
    *) die "internal: bad bump $kind" ;;
  esac
  printf '%s.%s.%s\n' "$maj" "$min" "$pat"
}

# First quoted value on a line that starts with $2. The prefix includes the
# opening quote, for example: version = "
quoted_after() {
  local file=$1 prefix=$2 line val
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      "$prefix"*)
        val=${line#"$prefix"}
        val=${val%%\"*}
        printf '%s\n' "$val"
        return 0
        ;;
    esac
  done <"$file"
  return 1
}

lock_package_version() {
  local package=$1 line name="" val
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      '[[package]]') name="" ;;
      'name = "perfr"') name=perfr ;;
      'name = "perfr-api"') name=perfr-api ;;
      'name = "'*) name=other ;;
      'version = "'*)
        if [ "$name" = "$package" ]; then
          val=${line#version = \"}
          val=${val%\"}
          printf '%s\n' "$val"
          return 0
        fi
        name=""
        ;;
    esac
  done <Cargo.lock
  return 1
}

replace_exact_line() {
  local file=$1 old=$2 new=$3
  local tmp found=0 line
  tmp=$(mktemp)
  while IFS= read -r line || [ -n "$line" ]; do
    if [ "$found" -eq 0 ] && [ "$line" = "$old" ]; then
      printf '%s\n' "$new"
      found=1
    else
      printf '%s\n' "$line"
    fi
  done <"$file" >"$tmp"
  if [ "$found" -ne 1 ]; then
    rm -f "$tmp"
    printf 'error: %s has no exact line: %s\n' "$file" "$old" >&2
    return 1
  fi
  cat "$tmp" >"$file"
  rm -f "$tmp"
}

update_cargo_lock() {
  local new=$1
  local tmp line name="" perfr_n=0 api_n=0
  tmp=$(mktemp)
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      '[[package]]')
        name=""
        printf '%s\n' "$line"
        ;;
      'name = "perfr"')
        name=perfr
        printf '%s\n' "$line"
        ;;
      'name = "perfr-api"')
        name=perfr-api
        printf '%s\n' "$line"
        ;;
      'name = "'*)
        name=other
        printf '%s\n' "$line"
        ;;
      'version = "'*)
        if [ "$name" = "perfr" ]; then
          printf 'version = "%s"\n' "$new"
          perfr_n=$((perfr_n + 1))
        elif [ "$name" = "perfr-api" ]; then
          printf 'version = "%s"\n' "$new"
          api_n=$((api_n + 1))
        else
          printf '%s\n' "$line"
        fi
        name=""
        ;;
      *) printf '%s\n' "$line" ;;
    esac
  done <Cargo.lock >"$tmp"
  if [ "$perfr_n" -ne 1 ] || [ "$api_n" -ne 1 ]; then
    rm -f "$tmp"
    printf 'error: Cargo.lock should contain one perfr and one perfr-api version (found %s and %s)\n' \
      "$perfr_n" "$api_n" >&2
    return 1
  fi
  cat "$tmp" >Cargo.lock
  rm -f "$tmp"
}

# One path per line. Every path is a single token, so word-splitting is safe.
version_files() {
  cat <<'EOF'
crates/perfr-api/Cargo.toml
crates/perfr-api/python/pyproject.toml
crates/perfr-api/python/perfr_api/__init__.py
crates/perfr/Cargo.toml
crates/perfr/python/pyproject.toml
crates/perfr/python/perfr/__init__.py
Cargo.lock
EOF
}

require_version_files() {
  local f
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    if [ ! -f "$f" ]; then
      die "missing $f"
    fi
  done <<EOF
$(version_files)
EOF
  if ! grep -q '^version = 3$' Cargo.lock; then
    die "Cargo.lock is not lockfile version 3; refusing to edit it"
  fi
}

# Prints one version, or dies listing every location when they disagree.
read_stamped_version() {
  local api_cargo api_py api_init svc_cargo svc_py svc_init svc_pin lock_svc lock_api
  api_cargo=$(quoted_after crates/perfr-api/Cargo.toml 'version = "') ||
    die "crates/perfr-api/Cargo.toml has no package version"
  api_py=$(quoted_after crates/perfr-api/python/pyproject.toml 'version = "') ||
    die "crates/perfr-api/python/pyproject.toml has no version"
  api_init=$(quoted_after crates/perfr-api/python/perfr_api/__init__.py '__version__ = "') ||
    die "crates/perfr-api/python/perfr_api/__init__.py has no __version__"
  svc_cargo=$(quoted_after crates/perfr/Cargo.toml 'version = "') ||
    die "crates/perfr/Cargo.toml has no package version"
  svc_py=$(quoted_after crates/perfr/python/pyproject.toml 'version = "') ||
    die "crates/perfr/python/pyproject.toml has no version"
  svc_init=$(quoted_after crates/perfr/python/perfr/__init__.py '__version__ = "') ||
    die "crates/perfr/python/perfr/__init__.py has no __version__"
  svc_pin=$(quoted_after crates/perfr/python/pyproject.toml 'dependencies = ["perfr-api==') ||
    die "crates/perfr/python/pyproject.toml has no perfr-api== pin"
  lock_svc=$(lock_package_version perfr) || die "Cargo.lock has no perfr package"
  lock_api=$(lock_package_version perfr-api) || die "Cargo.lock has no perfr-api package"

  if [ "$api_cargo" != "$api_py" ] || [ "$api_cargo" != "$api_init" ] ||
    [ "$api_cargo" != "$svc_cargo" ] || [ "$api_cargo" != "$svc_py" ] ||
    [ "$api_cargo" != "$svc_init" ] || [ "$api_cargo" != "$svc_pin" ] ||
    [ "$api_cargo" != "$lock_svc" ] || [ "$api_cargo" != "$lock_api" ]; then
    printf 'error: release versions do not match. Make these the same string and re-run:\n' >&2
    printf '  %-52s %s\n' "crates/perfr-api/Cargo.toml" "$api_cargo" >&2
    printf '  %-52s %s\n' "crates/perfr-api/python/pyproject.toml" "$api_py" >&2
    printf '  %-52s %s\n' "perfr_api/__init__.py __version__" "$api_init" >&2
    printf '  %-52s %s\n' "crates/perfr/Cargo.toml (perfr --version)" "$svc_cargo" >&2
    printf '  %-52s %s\n' "crates/perfr/python/pyproject.toml" "$svc_py" >&2
    printf '  %-52s %s\n' "perfr/__init__.py __version__" "$svc_init" >&2
    printf '  %-52s %s\n' "perfr-api== pin" "$svc_pin" >&2
    printf '  %-52s %s\n' "Cargo.lock perfr" "$lock_svc" >&2
    printf '  %-52s %s\n' "Cargo.lock perfr-api" "$lock_api" >&2
    exit 1
  fi
  if ! is_orderable_version "$api_cargo"; then
    die "stamped version $api_cargo is not X.Y.Z or X.Y.Z-prerelease"
  fi
  printf '%s\n' "$api_cargo"
}

restore_version_files() {
  # Checkout from HEAD, not from the index: a failed commit may already
  # have staged the new versions.
  # shellcheck disable=SC2046
  git checkout HEAD -- $(version_files)
}

apply_version() {
  local old=$1 new=$2
  local failed=0
  replace_exact_line crates/perfr-api/Cargo.toml \
    "version = \"$old\"" "version = \"$new\"" || failed=1
  replace_exact_line crates/perfr-api/python/pyproject.toml \
    "version = \"$old\"" "version = \"$new\"" || failed=1
  replace_exact_line crates/perfr-api/python/perfr_api/__init__.py \
    "__version__ = \"$old\"" "__version__ = \"$new\"" || failed=1
  replace_exact_line crates/perfr/Cargo.toml \
    "version = \"$old\"" "version = \"$new\"" || failed=1
  replace_exact_line crates/perfr/python/pyproject.toml \
    "version = \"$old\"" "version = \"$new\"" || failed=1
  replace_exact_line crates/perfr/python/pyproject.toml \
    "dependencies = [\"perfr-api==$old\"]" \
    "dependencies = [\"perfr-api==$new\"]" || failed=1
  replace_exact_line crates/perfr/python/perfr/__init__.py \
    "__version__ = \"$old\"" "__version__ = \"$new\"" || failed=1
  update_cargo_lock "$new" || failed=1
  if [ "$failed" -ne 0 ]; then
    restore_version_files || true
    die "could not update every version file. Restored the copies this script edited."
  fi
  local now=""
  if ! now=$(read_stamped_version); then
    restore_version_files || true
    die "version files do not match $new after writing them. Restored them."
  fi
  if [ "$now" != "$new" ]; then
    restore_version_files || true
    die "version files read back as $now after writing $new. Restored them."
  fi
}

worktree_dirty() {
  git -c color.ui=false status --porcelain=v1 --untracked-files=all
}

require_on_main() {
  local branch
  branch=$(git symbolic-ref --quiet --short HEAD || true)
  if [ "$branch" != "main" ]; then
    if [ -z "$branch" ]; then
      die "HEAD is detached. Check out main. This script will not move the branch."
    fi
    die "HEAD is $branch, not main. Check out main and re-run."
  fi
}

require_clean_main() {
  local dirty local_sha remote_sha counts ahead="" behind=""
  dirty=$(worktree_dirty)
  if [ -n "$dirty" ]; then
    printf 'error: working tree is not clean. Commit, stash, or remove these paths and re-run:\n' >&2
    printf '%s\n' "$dirty" >&2
    exit 1
  fi
  require_on_main
  if ! git rev-parse --verify --quiet refs/remotes/origin/main >/dev/null; then
    die "origin/main does not exist after fetch. This script will not create it."
  fi
  local_sha=$(git rev-parse main)
  remote_sha=$(git rev-parse origin/main)
  if [ "$local_sha" != "$remote_sha" ]; then
    counts=$(git rev-list --left-right --count main...origin/main)
    IFS=$(printf '\t ') read -r ahead behind <<EOF
$counts
EOF
    printf 'error: local main is not origin/main.\n' >&2
    printf '  local main:  %s\n' "$local_sha" >&2
    printf '  origin/main: %s\n' "$remote_sha" >&2
    printf '  local is %s commit(s) ahead and %s behind.\n' "${ahead:-?}" "${behind:-?}" >&2
    printf 'Update main yourself (fetch, fast-forward, or push) and re-run. This script will not merge, rebase, or reset.\n' >&2
    exit 1
  fi
}

parse_github_slug() {
  local url=$1 rest
  url=${url%.git}
  case "$url" in
    git@github.com:*)
      printf '%s\n' "${url#git@github.com:}"
      ;;
    ssh://git@github.com/*)
      printf '%s\n' "${url#ssh://git@github.com/}"
      ;;
    https://github.com/* | http://github.com/*)
      rest=${url#*github.com/}
      printf '%s\n' "$rest"
      ;;
    https://*@github.com/* | http://*@github.com/*)
      rest=${url#*@github.com/}
      printf '%s\n' "$rest"
      ;;
    *) return 1 ;;
  esac
}

valid_slug() {
  case "$1" in
    */*) ;;
    *) return 1 ;;
  esac
  case "$1" in
    *[!A-Za-z0-9._/-]*) return 1 ;;
  esac
  case "$1" in
    */*/*) return 1 ;;
  esac
}

github_slug() {
  local slug="" url
  if slug=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null); then
    slug=${slug%$'\r'}
  else
    slug=""
  fi
  if ! valid_slug "$slug"; then
    url=$(git remote get-url origin 2>/dev/null) || url=""
    slug=$(parse_github_slug "$url" 2>/dev/null) || slug=""
  fi
  if ! valid_slug "$slug"; then
    die "could not tell which GitHub repository this is. Run gh auth login, or set origin to github.com/owner/name."
  fi
  printf '%s\n' "$slug"
}

# Appends human-readable check lines to the globals CI_OK CI_SKIP CI_FAIL CI_PENDING.
record_checks() {
  local file=$1 line kind name status conclusion
  while IFS=$(printf '\t') read -r kind name status conclusion || [ -n "$kind" ]; do
    [ -n "$kind" ] || continue
    case "$kind" in
      check)
        if [ "$status" != "completed" ]; then
          CI_PENDING="${CI_PENDING}  ${name} (${status})"$'\n'
        else
          case "$conclusion" in
            success) CI_OK=$((CI_OK + 1)) ;;
            skipped | neutral) CI_SKIP=$((CI_SKIP + 1)) ;;
            *) CI_FAIL="${CI_FAIL}  ${name} (${conclusion:-unknown})"$'\n' ;;
          esac
        fi
        ;;
      status)
        case "$status" in
          success) CI_OK=$((CI_OK + 1)) ;;
          pending) CI_PENDING="${CI_PENDING}  ${name} (pending)"$'\n' ;;
          *) CI_FAIL="${CI_FAIL}  ${name} (${status})"$'\n' ;;
        esac
        ;;
      *)
        die "unexpected CI line from gh (kind=$kind). Refusing to guess."
        ;;
    esac
  done <"$file"
}

fetch_ci_lines() {
  local repo=$1 sha=$2 out=$3 err=$4
  : >"$out"
  : >"$err"
  if ! gh api --paginate "repos/${repo}/commits/${sha}/check-runs" \
    --jq '.check_runs[] | ["check", .name, .status, (.conclusion // "")] | @tsv' \
    >>"$out" 2>>"$err"; then
    return 1
  fi
  if ! gh api "repos/${repo}/commits/${sha}/status" \
    --jq '.statuses[] | ["status", .context, .state] | @tsv' \
    >>"$out" 2>>"$err"; then
    return 1
  fi
}

require_green_ci() {
  local repo=$1 sha=$2 force=$3
  local out err short
  short=$(git rev-parse --short=12 "$sha")
  out=$(mktemp)
  err=$(mktemp)
  local label="error"
  if [ "$force" -eq 1 ]; then
    label="warning"
  fi
  if ! fetch_ci_lines "$repo" "$sha" "$out" "$err"; then
    printf '%s: could not read CI for origin/main (%s).\n' "$label" "$short" >&2
    if [ -s "$err" ]; then
      cat "$err" >&2
    fi
    rm -f "$out" "$err"
    if [ "$force" -eq 1 ]; then
      warn "--force skips only the CI check. Continuing."
      return 0
    fi
    die "pass --force to skip only the CI check."
  fi
  CI_OK=0
  CI_SKIP=0
  CI_FAIL=""
  CI_PENDING=""
  record_checks "$out"
  rm -f "$out" "$err"
  if [ -z "$CI_FAIL" ] && [ -z "$CI_PENDING" ] && [ "$CI_OK" -gt 0 ]; then
    printf 'CI on origin/main (%s) is green (%s succeeded, %s skipped or neutral).\n' \
      "$short" "$CI_OK" "$CI_SKIP"
    return 0
  fi
  printf '%s: CI on origin/main (%s, %s) is not green.\n' "$label" "$short" "$sha" >&2
  if [ -n "$CI_FAIL" ]; then
    printf 'failing:\n%s' "$CI_FAIL" >&2
  fi
  if [ -n "$CI_PENDING" ]; then
    printf 'pending:\n%s' "$CI_PENDING" >&2
  fi
  if [ -z "$CI_FAIL" ] && [ -z "$CI_PENDING" ]; then
    printf 'no successful check runs or commit statuses on this commit.\n' >&2
  fi
  if [ "$force" -eq 1 ]; then
    warn "--force skips only the CI check. Continuing."
    return 0
  fi
  die "wait for CI, or pass --force to skip only this check."
}

# Latest orderable perfr-v* version, printed as the version without the prefix.
# Warns about perfr-v* tags that are not versions. Sets nothing on empty.
latest_version() {
  local tag ver best=""
  while IFS= read -r tag; do
    [ -n "$tag" ] || continue
    ver=${tag#perfr-v}
    if ! is_orderable_version "$ver"; then
      warn "ignoring $tag (not perfr-vX.Y.Z or perfr-vX.Y.Z-prerelease)"
      continue
    fi
    if [ -z "$best" ] || version_gt "$ver" "$best"; then
      best=$ver
    fi
  done <<EOF
$(git tag -l 'perfr-v*')
EOF
  if [ -n "$best" ]; then
    printf '%s\n' "$best"
  fi
  return 0
}

next_rc_number() {
  local base=$1 tag ver n max=0
  while IFS= read -r tag; do
    [ -n "$tag" ] || continue
    ver=${tag#perfr-v}
    case "$ver" in
      "$base"-rc.*)
        n=${ver#"$base"-rc.}
        if is_canonical_number "$n"; then
          if [ "$n" -gt "$max" ]; then
            max=$n
          fi
        fi
        ;;
    esac
  done <<EOF
$(git tag -l "perfr-v${base}-rc.*")
EOF
  printf '%s\n' "$((max + 1))"
}

tag_exists_locally() {
  git show-ref --verify --quiet "refs/tags/$1"
}

tag_exists_on_origin() {
  local name=$1
  [ -n "$(git ls-remote --tags origin "refs/tags/${name}")" ]
}

refuse_if_tag_exists() {
  local tag=$1
  local local_hit=0 remote_hit=0
  if tag_exists_locally "$tag"; then
    local_hit=1
  fi
  if tag_exists_on_origin "$tag"; then
    remote_hit=1
  fi
  if [ "$local_hit" -eq 0 ] && [ "$remote_hit" -eq 0 ]; then
    return 0
  fi
  printf 'error: tag %s already exists' "$tag" >&2
  if [ "$local_hit" -eq 1 ] && [ "$remote_hit" -eq 1 ]; then
    printf ' locally and on origin.\n' >&2
  elif [ "$local_hit" -eq 1 ]; then
    printf ' locally but not on origin.\n' >&2
    printf 'This script will not delete it. If that local tag is the one you want, push it with:\n' >&2
    printf '  git push origin refs/tags/%s\n' "$tag" >&2
  else
    printf ' on origin.\n' >&2
    printf 'Fetch it with git fetch --tags origin. This script will not overwrite it.\n' >&2
  fi
  exit 1
}

choose_bump() {
  local ans
  printf 'Bump [patch/minor/major] (default patch): ' >&2
  if ! IFS= read -r ans; then
    die "no bump selected. Pass --patch, --minor, --major, or --version X.Y.Z."
  fi
  ans=$(printf '%s' "$ans" | tr '[:upper:]' '[:lower:]')
  ans=${ans#"${ans%%[![:space:]]*}"}
  ans=${ans%"${ans##*[![:space:]]}"}
  case "$ans" in
    "" | patch) printf 'patch\n' ;;
    minor) printf 'minor\n' ;;
    major) printf 'major\n' ;;
    *) die "choose patch, minor, or major (got: $ans)" ;;
  esac
}

confirm_release() {
  local tag=$1 ans
  printf "Type y to commit (if needed), tag %s, and push: " "$tag" >&2
  if ! IFS= read -r ans; then
    die "confirmation was not y. Nothing was committed or tagged."
  fi
  case "$ans" in
    y | Y) return 0 ;;
  esac
  die "confirmation was not y. Nothing was committed or tagged."
}

print_commit_log() {
  local latest_tag=$1
  local count
  if [ -z "$latest_tag" ]; then
    printf 'Commits: no previous perfr-vX.Y.Z tag. Recent history:\n'
    git log --oneline -n 20 || true
    return 0
  fi
  if ! git merge-base --is-ancestor "$latest_tag" HEAD; then
    warn "$latest_tag is not an ancestor of HEAD. The log below is commits on HEAD that the tag does not contain."
  fi
  count=$(git rev-list --count "${latest_tag}..HEAD")
  printf 'Commits since %s (%s):\n' "$latest_tag" "$count"
  if [ "$count" -eq 0 ]; then
    printf '  (none)\n'
    return 0
  fi
  git log --oneline -n 40 "${latest_tag}..HEAD"
  if [ "$count" -gt 40 ]; then
    printf '... %s more\n' "$((count - 40))"
  fi
}

print_summary() {
  local old=$1 new=$2 tag=$3 latest_tag=$4 will_commit=$5
  # The tag is always perfr-v..., so the hyphen in the prefix is not a
  # prerelease. The version string carries -rc.N when there is one.
  local kind="stable"
  case "$new" in
    *-*) kind="prerelease" ;;
  esac
  printf '\n'
  if [ -n "$latest_tag" ]; then
    printf 'latest: %s\n' "$latest_tag"
  else
    printf 'latest: (no perfr-vX.Y.Z tag)\n'
  fi
  printf 'version: %s -> %s\n' "$old" "$new"
  printf 'tag: %s\n' "$tag"
  if [ "$kind" = "stable" ]; then
    printf 'GitHub will mark this Latest.\n'
  else
    printf 'GitHub will mark this as a prerelease, not Latest.\n'
  fi
  printf '\n'
  print_commit_log "$latest_tag"
  printf '\n'
  if [ "$will_commit" -eq 1 ]; then
    printf 'Commit: Release perfr %s\n' "$new"
    printf 'Author: %s <%s>\n' "$RELEASE_AUTHOR_NAME" "$RELEASE_AUTHOR_EMAIL"
    printf 'Files:\n'
    local f
    while IFS= read -r f; do
      [ -n "$f" ] || continue
      printf '  %s\n' "$f"
    done <<EOF
$(version_files)
EOF
    printf 'Push: origin main\n'
  else
    printf 'Version files are already %s. No commit.\n' "$new"
  fi
  printf 'Then: annotated tag %s, pushed as refs/tags/%s only.\n' "$tag" "$tag"
}

is_protected_push() {
  local text
  text=$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')
  case "$text" in
    *gh006* | *gh013*) return 0 ;;
    *"protected branch"*) return 0 ;;
    *"repository rule violation"*) return 0 ;;
    *"changes must be made through a pull request"*) return 0 ;;
    *"push declined due to repository rule"*) return 0 ;;
  esac
  return 1
}

open_release_pr() {
  local repo=$1 new=$2 tag=$3 push_error=$4
  local branch="release/${tag}" bodyfile pr_url
  printf 'error: push to main was rejected.\n' >&2
  printf '%s\n' "$push_error" >&2
  if git ls-remote --heads origin "refs/heads/${branch}" | grep -q .; then
    die "origin already has $branch. This script will not force-push it. No tag was created."
  fi
  if ! git push origin "HEAD:refs/heads/${branch}"; then
    die "could not push $branch either. The release commit is only on local main. No tag was created."
  fi
  bodyfile=$(mktemp)
  cat >"$bodyfile" <<EOF
Release perfr ${new}

\`tools/release/release.sh\` bumped the release version and committed it, but main rejected the push. Merging this pull request lands that commit. The tag is not created yet.

After it is merged:

1. Make local main the same commit as origin/main:

       git fetch origin
       git checkout main
       git merge --ff-only origin/main

   If GitHub merged a different commit and the fast-forward refuses, the release commit is already on \`${branch}\`. Point local main at origin only after you are sure that branch is on the remote:

       git fetch origin
       git checkout main
       git reset --hard origin/main

2. Run:

       tools/release/release.sh --version ${new}

   The version files already match, so this does not commit again. Type y to create and push \`${tag}\`.
EOF
  if ! pr_url=$(gh pr create --repo "$repo" --base main --head "$branch" \
    --title "Release perfr ${new}" --body-file "$bodyfile"); then
    rm -f "$bodyfile"
    printf 'error: the branch %s is on origin, but opening the pull request failed.\n' "$branch" >&2
    printf 'Create it yourself, merge it, then run:\n' >&2
    printf '  tools/release/release.sh --version %s\n' "$new" >&2
    printf 'No tag was created.\n' >&2
    exit 1
  fi
  rm -f "$bodyfile"
  cat <<EOF

Push to main was rejected (branch protection or a repository rule). No tag was created.
The release commit is on local main and on origin/${branch}.

Pull request: ${pr_url}

Next:
  1. Merge the pull request.
  2. Make local main match origin/main (git fetch origin && git merge --ff-only origin/main).
     If GitHub merged a different SHA, git reset --hard origin/main is safe only once ${branch} is on origin.
  3. Run: tools/release/release.sh --version ${new}
     Type y. The version is already committed, so this only creates and pushes ${tag}.
EOF
  exit 1
}

find_run_url() {
  local repo=$1 workflow=$2 tag=$3
  local attempt=0 line branch url
  while [ "$attempt" -lt "$POLL_ATTEMPTS" ]; do
    attempt=$((attempt + 1))
    while IFS=$(printf '\t') read -r branch url || [ -n "${branch:-}" ]; do
      [ -n "${branch:-}" ] || continue
      if [ "$branch" = "$tag" ] && [ -n "$url" ]; then
        printf '%s\n' "$url"
        return 0
      fi
    done <<EOF
$(gh run list --repo "$repo" --workflow "$workflow" --limit 30 \
  --json url,headBranch --jq '.[] | [.headBranch, .url] | @tsv' 2>/dev/null || true)
EOF
    if [ "$attempt" -lt "$POLL_ATTEMPTS" ]; then
      sleep "$POLL_SLEEP"
    fi
  done
  return 1
}

print_urls() {
  local repo=$1 tag=$2
  local release_url="https://github.com/${repo}/releases/tag/${tag}"
  local rust_fallback="https://github.com/${repo}/actions/workflows/release-rust.yml"
  local wheels_fallback="https://github.com/${repo}/actions/workflows/python-wheels.yml"
  local rust_url="" wheels_url=""
  rust_url=$(find_run_url "$repo" release-rust.yml "$tag" || true)
  wheels_url=$(find_run_url "$repo" python-wheels.yml "$tag" || true)
  printf '\n'
  printf 'Tag %s was pushed.\n' "$tag"
  if [ -n "$rust_url" ]; then
    printf 'release-rust: %s\n' "$rust_url"
  else
    printf 'release-rust: %s\n' "$rust_fallback"
    printf '  (the run was not listed yet; this is the workflow page)\n'
  fi
  if [ -n "$wheels_url" ]; then
    printf 'python-wheels: %s\n' "$wheels_url"
  else
    printf 'python-wheels: %s\n' "$wheels_fallback"
    printf '  (the run was not listed yet; this is the workflow page)\n'
  fi
  printf 'Release: %s\n' "$release_url"
  printf 'The Release page is empty until the publish job finishes.\n'
}

main() {
  local bump="" explicit="" rc=0 dry_run=0 force=0
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -h | --help)
        usage
        exit 0
        ;;
      --dry-run) dry_run=1 ;;
      --force) force=1 ;;
      --patch | --minor | --major)
        if [ -n "$bump" ] || [ -n "$explicit" ]; then
          die "pass only one of --patch, --minor, --major, or --version"
        fi
        bump=${1#--}
        ;;
      --rc) rc=1 ;;
      --version)
        shift
        if [ "$#" -lt 1 ]; then
          die "--version needs X.Y.Z"
        fi
        if [ -n "$bump" ] || [ -n "$explicit" ]; then
          die "pass only one of --patch, --minor, --major, or --version"
        fi
        explicit=$1
        ;;
      --version=*)
        if [ -n "$bump" ] || [ -n "$explicit" ]; then
          die "pass only one of --patch, --minor, --major, or --version"
        fi
        explicit=${1#--version=}
        ;;
      *)
        die "unknown argument: $1 (try --help)"
        ;;
    esac
    shift
  done

  local root
  root=$(git rev-parse --show-toplevel 2>/dev/null) ||
    die "run this inside the perfr git repository"
  cd "$root" || die "cannot cd to $root"

  printf 'Fetching tags and origin/main...\n'
  if ! git fetch --tags origin; then
    die "git fetch --tags origin failed. This script will not overwrite local tags or branches."
  fi
  require_clean_main

  if ! command -v gh >/dev/null 2>&1; then
    die "gh is not installed (https://cli.github.com/)."
  fi
  if ! gh auth status >/dev/null 2>&1; then
    die "gh is not authenticated. Run gh auth login and re-run. --force does not skip this."
  fi
  local repo sha
  repo=$(github_slug)
  sha=$(git rev-parse origin/main)
  require_green_ci "$repo" "$sha" "$force"

  require_version_files
  local stamped latest latest_tag="" base new tag
  stamped=$(read_stamped_version)
  latest=$(latest_version)
  if [ -n "$latest" ]; then
    latest_tag="perfr-v${latest}"
  fi

  if [ -n "$explicit" ]; then
    if ! is_exact_version "$explicit"; then
      die "--version must be X.Y.Z or X.Y.Z-rc.N, with no leading zeros (got: $explicit)"
    fi
    case "$explicit" in
      *-rc.*)
        if [ "$rc" -eq 1 ]; then
          die "pass --version X.Y.Z --rc, or --version X.Y.Z-rc.N, not both"
        fi
        ;;
    esac
    new=$explicit
  else
    if [ -z "$bump" ]; then
      bump=$(choose_bump)
    fi
    if [ -n "$latest" ]; then
      base=$(numeric_triple "$latest")
    else
      base=$(numeric_triple "$stamped")
    fi
    split_ver "$base"
    new=$(bump_triple "$bump" "$SV_MAJ" "$SV_MIN" "$SV_PAT")
  fi

  if [ "$rc" -eq 1 ]; then
    case "$new" in
      *-*) die "internal: --rc applied to a version that already has a suffix" ;;
    esac
    local n
    n=$(next_rc_number "$new")
    new="${new}-rc.${n}"
  fi

  if ! is_exact_version "$new"; then
    die "refusing version $new"
  fi
  tag="perfr-v${new}"

  if [ -n "$latest" ] && ! version_gt "$new" "$latest"; then
    die "$new is not newer than the latest tag perfr-v${latest}. Refusing to tag it."
  fi
  if version_gt "$stamped" "$new"; then
    die "$new is older than the version already in the tree ($stamped). Refusing to move it backwards."
  fi
  refuse_if_tag_exists "$tag"

  local will_commit=0
  if [ "$new" != "$stamped" ]; then
    will_commit=1
  fi
  print_summary "$stamped" "$new" "$tag" "$latest_tag" "$will_commit"

  if [ "$dry_run" -eq 1 ]; then
    printf '\n'
    if [ "$will_commit" -eq 1 ]; then
      printf 'dry-run: would commit "Release perfr %s" as %s <%s>\n' \
        "$new" "$RELEASE_AUTHOR_NAME" "$RELEASE_AUTHOR_EMAIL"
      printf 'dry-run: would push origin main\n'
    else
      printf 'dry-run: version files already match %s; no commit\n' "$new"
    fi
    printf 'dry-run: would create annotated tag %s\n' "$tag"
    printf 'dry-run: would push origin refs/tags/%s\n' "$tag"
    printf 'dry-run: not committing, tagging, or pushing.\n'
    exit 0
  fi

  confirm_release "$tag"
  require_clean_main
  refuse_if_tag_exists "$tag"

  if [ "$will_commit" -eq 1 ]; then
    apply_version "$stamped" "$new"
    # shellcheck disable=SC2046
    git add -- $(version_files)
    if git diff --cached --quiet; then
      restore_version_files || true
      die "editing the version files produced no diff. Restored them."
    fi
    if ! as_release_author git commit -m "Release perfr ${new}"; then
      restore_version_files || true
      die "commit failed. Version files were restored. No tag was created."
    fi
    local push_out push_rc=0
    push_out=$(git push origin HEAD:main 2>&1) || push_rc=$?
    if [ "$push_rc" -ne 0 ]; then
      if is_protected_push "$push_out"; then
        open_release_pr "$repo" "$new" "$tag" "$push_out"
      fi
      printf 'error: git push origin main failed.\n%s\n' "$push_out" >&2
      printf 'The release commit is on local main only. This script will not rebase or reset. No tag was created.\n' >&2
      exit 1
    fi
    printf '%s\n' "$push_out"
  fi

  # The tag is created only after a successful main push. Do not require
  # origin/main's remote-tracking ref here: a just-finished push is the
  # source of truth, and this script will not fetch again to move it.
  local dirty
  dirty=$(worktree_dirty)
  if [ -n "$dirty" ]; then
    printf 'error: working tree is dirty after the release commit. No tag was created.\n%s\n' "$dirty" >&2
    exit 1
  fi
  require_on_main
  refuse_if_tag_exists "$tag"
  if ! as_release_author git tag -a "$tag" -m "Release perfr ${new}"; then
    die "creating the annotated tag failed. No tag was pushed."
  fi
  local tag_push tag_rc=0
  tag_push=$(git push origin "refs/tags/${tag}" 2>&1) || tag_rc=$?
  if [ "$tag_rc" -ne 0 ]; then
    printf 'error: the tag %s exists locally but the push failed.\n%s\n' "$tag" "$tag_push" >&2
    printf 'This script will not delete the local tag. When the remote accepts it, run:\n' >&2
    printf '  git push origin refs/tags/%s\n' "$tag" >&2
    exit 1
  fi
  printf '%s\n' "$tag_push"
  print_urls "$repo" "$tag"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
