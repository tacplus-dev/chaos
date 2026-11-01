Suggestion: Chaos Mesh Add PodChaos action pod-freeze to stop a pod's processes (v9.9.9)
Type: Resiliency
Summary: Add PodChaos action pod-freeze to stop a pod's processes
Why: Chaos Mesh added it in v9.9.9; tacplus's chaos survey (tacplus-dev/chaos docs/chaos-mesh-survey.md) has no decision for it yet.

## Evidence
- Release: https://github.com/chaos-mesh/chaos-mesh/releases/tag/v9.9.9
- Change: https://github.com/chaos-mesh/chaos-mesh/pull/9999

## Cost
Unknown until surveyed: add it to docs/chaos-mesh-survey.md with an adopt now / adopt later / not doing decision.

## Touches
- tacplus-dev/chaos#1 (the chaos system's design) and docs/chaos-mesh-survey.md, section 6.

## Reuse considered
tacplus's chaos system is informed by Chaos Mesh, not built on it (tacplus-dev/chaos#1): the question is whether to build our own equivalent.

## Status
open
