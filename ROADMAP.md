# Roadmap: chaos

Owned by Jody; changed only by an agent at Jody's request. Loopers propose in PROPOSALS.md.

## Now
Nothing released yet. The chaos agent that every tacplus fleet deploys by default (Jody, 2026-11-01): fleet-scoped,
tagged fault injection, with `fleet.yaml chaos.enabled: false` to opt out. It runs in production too, where a killed container
isn't a disruption.

## Next
- The chaos agent, first release: pod kills by default, opt-in levels (members, network, nodes), fleet-scoped RBAC, an exclusion list, tagged faults (#1)

## Later
- Feeding the agent's fault tags into automatic security remediation and canary judgement (tacplus-dev/roadmap ROADMAP.md, Later)

## Not doing
- Acting outside its own fleet's namespaces, or on letsencrypt or secrets: never, by design.
