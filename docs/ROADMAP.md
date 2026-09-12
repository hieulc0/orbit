# Current roadmap

## Milestone: reproducible, deployable alpha

The bounded Phase 1–9 feature work is committed. Historical acceptance and
qualification remain in [the archive](archive/README.md); they are not a claim
that the complete long-term vision or production hardening is finished.

The alpha increment covers:

1. Current docs and a fresh-agent entry point, with preserved historical evidence.
2. Repeatable Rust/Python/UI checks and CI qualification with disposable services.
3. A non-root server image, Docker Compose, Podman Quadlet, and host worker units.
4. Probes, metrics, safe structured logs, SIGTERM handling and durable worker drain.
5. Backup/restore, coordinated upgrades, credential rotation and operational tests.
6. A trusted command-agent adapter and Orbit-on-Orbit work against a pinned baseline.

See [alpha qualification](operations/qualification.md) for implemented vs verified
items. A workflow is not qualified by the mere presence of its template or test.

## Acceptance gates

- A fresh checkout has one documented build/check path and no secret prerequisites
  for regular tests.
- A fresh installation can serve the UI/API, finish a worker-free workflow,
  restart with retained state, and preserve verified artifacts.
- Container shutdown and worker drain preserve the existing lease/retry semantics.
- A stopped deployment can be backed up and restored into an empty isolated
  destination, with run/journal/artifact agreement independently checked.
- A fresh agent can locate authoritative docs and run the relevant checks without
  relying on conversation history.
- Orbit-on-Orbit produces a reviewed patch and successful independent checks
  without changing or pushing the developer checkout.

## Follow-up work requiring separate evidence or choices

Live model/provider integration and billing, physical GPUs, Docker compute-worker
qualification on a suitable host, production SSO/cloud vaults, hostile-agent isolation,
HA/throughput/retention, cross-browser/accessibility certification, public package
distribution, and infrastructure for a selected cloud. Do not add Kubernetes or
Terraform until an actual hosting requirement justifies it.

Public publishing, deployments outside disposable local fixtures and paid model
calls require explicit authority and selected destinations/accounts.
