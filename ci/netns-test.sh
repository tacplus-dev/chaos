#!/usr/bin/env bash
# The wedge's real-kernel tests (wedge/tests/netns.rs): netem through `tc` in a network namespace, its
# revert at the fault's end, and the revert of a crashed wedge's journal. They need CAP_NET_ADMIN and
# CAP_SYS_ADMIN, so the test binary is built here and run in a container with exactly those two, in a
# network namespace of its own (the container's), never the host's. Ignored by a plain `cargo test`.
#
# Usage:  ci/netns-test.sh
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$(cd "$root/wedge" && cargo test --locked --test netns --no-run --message-format=json 2>/dev/null |
  jq -r 'select(.reason == "compiler-artifact" and .target.name == "netns" and .executable != null) | .executable' | tail -1)"
[[ -x "$bin" ]] || { echo "netns-test.sh: no test binary built" >&2; exit 1; }

# The image: Debian trixie, as the service images (its glibc at least the build's), with iproute2 for tc.
# Built first, so the test itself runs with no capability but the two it needs.
image=chaos-netns-test:trixie
docker build -q -t "$image" - >/dev/null <<'DOCKERFILE'
FROM debian:trixie-slim
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends iproute2 && rm -rf /var/lib/apt/lists/*
DOCKERFILE

docker run --rm --memory 1g --cap-drop ALL --cap-add NET_ADMIN --cap-add SYS_ADMIN \
  -v "$root:$root:ro" -w "$root/wedge" "$image" "$bin" --ignored --test-threads 1
