#!/usr/bin/env bash
# tacplus as CI runs it (.github/workflows/ci.yml): the compiler image for the floor the tacplus.yaml files
# declare (`tacplus: "MAJOR.MINOR"`), always that line's latest patch, run on this checkout, once for each
# of the two generations: wedge/ and controller/ (each its own tacplus.yaml, both from proto/). The same
# image the looper's `tacplus` runs. `depends` (eventbus) are public repositories; GH_TOKEN, when set, is
# passed for the rate limit.
#
# Usage:  ci/tacplus.sh build
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
for project in wedge controller; do
  floor="$(sed -n -E 's/^tacplus:[[:space:]]*"?([0-9]+\.[0-9]+)"?.*$/\1/p' "$root/$project/tacplus.yaml" | head -1)"
  [[ -n "$floor" ]] || { echo "tacplus.sh: $project/tacplus.yaml declares no tacplus: MAJOR.MINOR floor" >&2; exit 1; }
  docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp ${GH_TOKEN:+-e GH_TOKEN} -v "$root:$root" -w "$root/$project" \
    "ghcr.io/tacplus-dev/tacplus/tacplus:$floor" tacplus "$@"
done
