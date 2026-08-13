#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
project_root="$(cd -- "$script_dir/.." && pwd)"
cd "$project_root"

# Compare every tracked working-tree file with an empty tree. This still checks
# committed content in a clean CI checkout, unlike a plain `git diff --check`.
empty_tree="$(git hash-object -t tree /dev/null)"
whitespace_rules="blank-at-eol,blank-at-eof,space-before-tab"
git -c core.whitespace="$whitespace_rules" diff --check "$empty_tree" --
git -c core.whitespace="$whitespace_rules" diff --cached --check "$empty_tree" --

# Git does not include untracked files in the comparison above. Check each one
# independently; exit 1 only means that the file differs from /dev/null, while
# an exit code greater than 1 reports a whitespace error.
while IFS= read -r -d '' file; do
    check_output=""
    if check_output="$(git -c core.whitespace="$whitespace_rules" diff --no-index --check -- /dev/null "$file" 2>&1)"; then
        check_status=0
    else
        check_status=$?
    fi

    if ((check_status > 1)); then
        printf '%s\n' "$check_output" >&2
        exit 1
    fi
done < <(git ls-files --others --exclude-standard -z)

# `git diff --check` does not reject a missing final newline. Check all text
# files that are either tracked or ready to be added, while skipping binaries.
while IFS= read -r -d '' file; do
    [[ ! -L "$file" && -f "$file" && -s "$file" ]] || continue
    LC_ALL=C grep -Iq '' -- "$file" || continue

    if [[ "$(tail -c 1 -- "$file" | wc -l | tr -d ' ')" != "1" ]]; then
        printf '%s: missing newline at end of file\n' "$file" >&2
        exit 1
    fi
done < <(git ls-files --cached --others --exclude-standard -z)
