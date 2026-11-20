# `just gates`: the checks every change to main passes, in the build image
# tacplus.lock pins (the build pipeline, tacplus-dev/build) or locally.

# `tacplus` runs the release tacplus.lock pins (the build image's launcher, or
# the looper's); TACPLUS_BIN overrides it with a tacplus of your own.
TACPLUS := env_var_or_default("TACPLUS_BIN", "tacplus")

default: gates

# The pin: an exact release and the build image by digest (`tacplus pin` writes it).
check-pin:
  #!/usr/bin/env bash
  set -euo pipefail
  grep -Eq '^tacplus: [0-9]+\.[0-9]+\.[0-9]+$' tacplus.lock || { echo "check-pin: tacplus.lock names no exact tacplus release" >&2; exit 1; }
  grep -Eq '^build_image: [^[:space:]]+@sha256:[0-9a-f]{64}$' tacplus.lock || { echo "check-pin: tacplus.lock names no build image by digest" >&2; exit 1; }

# The tacplus that runs is the release the pin names.
check-tacplus:
  #!/usr/bin/env bash
  set -euo pipefail
  want="$(sed -n -E 's/^tacplus: ([0-9]+\.[0-9]+\.[0-9]+)$/\1/p' tacplus.lock)"
  got="$({{TACPLUS}} version | sed -n -E 's/^tacplus version ([0-9]+\.[0-9]+\.[0-9]+).*$/\1/p' | head -1)"
  [[ "$got" == "$want" ]] || { echo "check-tacplus: tacplus is '$got', tacplus.lock pins $want" >&2; exit 1; }

gates: check-pin check-tacplus
