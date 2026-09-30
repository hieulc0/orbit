# Operating Orbit

- [Installation](installation.md): local server, runtime images, workers, credentials,
  repository workflows, editor clients and external reasoning connections.
- [Deployment](deployment.md): pinned server images, PostgreSQL, Compose, Quadlet and host workers.
- [Upgrades](upgrades.md): maintenance windows, migrations and credential rotation.
- [Backup and recovery](backup-recovery.md): offline verified backups and replacement restores.
- [Troubleshooting](troubleshooting.md): probes, logs, execution inspection, preflight and qualification procedures.

Use explicit installation paths, pinned images and authorized credentials. These
procedures do not authorize provider calls, publication or production changes by
themselves. Interpret results using [requirements](../requirements/README.md) and
[architecture](../architecture/README.md). Retain temporary execution results under
`.local/qualification/`; an unexecuted command or fixture does not establish acceptance.
