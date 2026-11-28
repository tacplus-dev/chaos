# Proposals: chaos

Written by loopers. Each proposal: what, why, links. Jody promotes accepted ones into ROADMAP.md (via an agent);
the looper then marks it `promoted` or `declined` here with the date.

## Open
- **<title>** (<date>, <issue/run link>): <what and why>
- **#1's services move onto the root pin** (2026-11-20, #2): looper/chaos-1 gives controller/ and wedge/ their own
  tacplus.yaml at floor 2.5, and gates of its own (ci/gates.sh, a GitHub workflow). Main now pins 2.6.16 at the root
  (tacplus.lock) and the build pipeline runs only `just gates` in that image: when #1 lands, its floors should move to
  2.6 and its gates become recipes of the root justfile's `gates`, or they never run as build jobs.

## Promoted / declined
