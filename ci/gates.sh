#!/usr/bin/env bash
# The gates (.github/workflows/ci.yml, and the looper's land): generate both services with tacplus at the
# floor (ci/tacplus.sh), check nothing generated is tracked, build and test each crate with its lock, and
# run the wedge's real-kernel tests in a container (ci/netns-test.sh).
#
# Usage:  ci/gates.sh
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

ci/tacplus.sh build
if tracked="$(git ls-files | grep -E '(^|/)target/')"; then
  echo "gates.sh: generated or built files are tracked:" >&2
  echo "$tracked" >&2
  exit 1
fi
for crate in policy wedge controller upstream-watch; do
  echo "== $crate"
  (cd "$crate" && cargo build --locked && cargo test --locked)
done
ci/netns-test.sh
echo "gates: ok"
