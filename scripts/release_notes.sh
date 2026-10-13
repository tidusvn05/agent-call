#!/usr/bin/env bash
set -euo pipefail

tag=${1:?usage: release_notes.sh TAG OUTPUT}
output=${2:?usage: release_notes.sh TAG OUTPUT}
repository=${REPOSITORY:-tidusvn05/agent-call}
commit=$(git rev-parse "refs/tags/$tag^{commit}")

previous=''
if git rev-parse -q --verify "${commit}^" >/dev/null; then
  previous=$(git describe --tags --abbrev=0 --match 'v[0-9]*' "${commit}^" 2>/dev/null || true)
fi

if [[ -n "$previous" ]]; then
  range="$previous..$commit"
else
  range="$commit"
fi

{
  printf '# %s\n\n' "$tag"
  printf '## Changes\n\n'
  git log --no-merges --format="- %s ([%h](https://github.com/$repository/commit/%H))" "$range"
  printf '\n'
  if [[ -n "$previous" ]]; then
    printf 'Full diff: https://github.com/%s/compare/%s...%s\n' "$repository" "$previous" "$tag"
  fi
} > "$output"
