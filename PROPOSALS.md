# Proposals: chaos

Written by loopers. Each proposal: what, why, links. Jody promotes accepted ones into ROADMAP.md (via an agent);
the looper then marks it `promoted` or `declined` here with the date.

## Open
- **Node-routed S2S calls in tacplus** (2026-11-01, #1): the controller must call the wedge on one given node, and the generated S2S
  clients pick any healthy instance. Today it finds the wedge's pod on that node and dials it with `sm.create_channel` and a token
  for chaos-wedge (controller/src/main.rs), which is unverified against the wedge's serving certificate. A generated way to call
  "the instance on node N" (like tenant routing's `channel_for_tenant`, keyed by node) would make it an ordinary generated call,
  verified by the SPIFFE verifier. Needs a tacplus-dev/tacplus feature.
- **Release images for both services** (2026-11-01, #1): Dockerfiles and a release workflow like llmproxy's (debian trixie-slim,
  the launcher; the wedge's image with iproute2 for `tc`), so operator#18 has images to deploy.
- **The survey's "adopt later" features** (2026-11-01, #1, docs/chaos-mesh-survey.md section 5): pod-failure, the other netem
  faults, bandwidth, partition (the "members" level), DNS, a chassis-level gRPC fault, IO latency and errors, `SkewClock`, node
  cordon and drain, typed scenarios, `fixed` modes with a quorum cap, a `chaos` CLI, metrics by kind and outcome. Each needs
  Jody's approval and a ROADMAP.md entry.
- **Rounds kept across a controller restart** (2026-11-01, #1): rounds and a pending recovery check live in the controller's
  memory; a restarted controller forgets a target that has not recovered (the record keeps it). Reading back the last
  `FaultEvent`s on start would close that.

## Promoted / declined
