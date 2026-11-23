# State: chaos

Overwritten to stay current (loopers and agents). Last updated: 2026-11-23 by the looper (#3).

- **Version / line:** none released; tacplus floor 2.6, pinned to 2.6.14 and its build image by digest (tacplus.lock)
- **Works:** the pin and `just gates` (the pin is exact, and the tacplus that runs is the pinned one), run by the build pipeline without Docker
- **Released / deployed:** nowhere yet
- **Known gaps:** the chaos agent itself (#1, on looper/chaos-1, not on main)
- **Blocked on:** #1 is blocked on tacplus-dev/registry#6
