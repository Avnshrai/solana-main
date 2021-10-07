#!/usr/bin/env bash
#
# Check if files in the commit range match one or more prefixes
#

# Always run the job if we are on a tagged release
if [[ -n "$TRAVIS_TAG" ]]; then
  exit 0
fi

(
  set -x
#   GITHUB_EVENT_BEFORE: ${{ github.event.before }}
#   GITHUB_EVENT_AFTER: ${{ github.event.after }}
  git diff --name-only "$COMMIT_RANGE"
)

for file in $(git diff --name-only "$COMMIT_RANGE"); do
  for prefix in "$@"; do
    if [[ $file =~ ^"$prefix" ]]; then
      exit 0
    fi
    done
done

echo "No modifications to $*"
exit 1
