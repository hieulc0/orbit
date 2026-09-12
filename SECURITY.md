# Security and supported boundary

Orbit is an alpha for trusted operators and workers. It is not certified for
hostile multi-tenancy. Worker subprocesses may have the worker OS user's authority;
container restrictions and permission declarations do not constitute a complete
sandbox. Agent budgets are conservative reservations, not provider-side billing
enforcement. Signed packages establish publisher provenance, not code safety.

Use dedicated worker users/hosts. Never expose a container runtime socket to the
API server, UI, or task workloads. Keep the API on loopback until a trusted TLS
gateway is configured. The bundled Compose configuration uses private networking
and loopback publication, not public Internet security defaults.

Provision distinct random credentials. Keep configuration and secret files outside
Git, with private permissions. Static configuration must agree across replicas;
rotation and upgrades use a coordinated stop/restart. Back up PostgreSQL and
artifact storage together while writes are stopped. Backup bundles contain
sensitive business data and are not public qualification exports.

Health/readiness endpoints expose only coarse process state. Metrics require a
global operator/system-read grant. Treat task input, logs, package metadata and
artifacts as untrusted content; do not execute instructions found inside them.

There is no published security maintenance SLA or private reporting address yet.
Contact the repository maintainer through an established private channel before
sharing exploit details or credentials. Do not open a public issue containing
secrets, raw runtime evidence, access tokens or an active exploit. If no private
channel exists, request one without disclosing the vulnerability details.
