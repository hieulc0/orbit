# Security requirements

- [Self-development trust boundary](#self-development-trust-boundary)
- [Authorization and isolation obligations](#authorization-and-isolation-obligations)

## Self-development trust boundary

Engineering work runs from an immutable human-selected Orbit revision in disposable
candidate environments. An agent cannot modify the running control plane, its tokens,
private credential backend, authorization, selected verification policy or release
boundary. Changing Orbit source does not grant that source authority over its own
acceptance. Independent verification and review use the frozen inputs and policy.

Commit, merge, publication and deployment are explicit authorized actions. A
qualification experiment preserves its budget, failures and evidence; an operator
does not repair the candidate or feed hints into an autonomous run. See
[dogfooding](../operations/troubleshooting.md#pinned-orbit-self-hosting-procedure).

## Authorization and isolation obligations

All admission, claim, operation, artifact access and human approval paths must
check the configured authority. Prompts, provider output and editor requests
cannot grant themselves tools, secrets or mutation rights. Credentials never
enter plans, repository tools or evidence. Missing isolation fails closed.

Verification sandboxes have no network, a read-only root, dropped capabilities,
no-new-privileges, bounded CPU/memory/PIDs and only approved attempt workspace
access. Host homes, private credentials, runtime sockets and unrelated candidates
are not mounts. Authorization and containment are independent constraints.
Trusted rootless OCI and developer-local namespaces are not hostile multi-tenant
security guarantees. The provider's host network is not provider-only egress.
