# Coordinated upgrades and credential rotation

Alpha uses a maintenance window, not a rolling mixed-version guarantee. Startup
applies additive migrations under PostgreSQL coordination. This increment adds
`0006_operations.sql` (`orbit_workers.draining`); old plan digests/outcomes remain
immutable. Migrations are not automatic down-migration or rollback.

## Upgrade sequence

1. Record source revision, immutable image ID, PostgreSQL major version and private
   config location. Build/check the candidate before downtime; read schema changes.
2. Stop submissions; drain workers and resolve outstanding leases/external effects.
   Stop workers and all servers; take a verified [offline backup](backup-restore.md).
3. Restore a copy into an isolated replacement and start the candidate there.
   Check readiness, old runs/journals/artifacts, a new timer and relevant worker
   capabilities on disposable workspaces.
4. With operator approval, change the stopped installation to that image and start
   it. Compose uses the same explicit env/config paths and `up -d --wait server`;
   Quadlet needs a source image update and user daemon reload/start. Observe logs
   and reconciliation before traffic resumes.
5. Start upgraded workers, explicitly resume durable drain flags and submissions.
   Retain the old image and backup until acceptance.

If verification fails, stop the candidate and restore the old backup into a
separate replacement using its compatible old image/config. Do not run an old
binary against an unqualified newer schema, remove columns or overwrite the only
backup. Same-image restart/restore does not prove every future upgrade safe.

## Credentials

Server credential references resolve at startup, with no hot reload. Clients also
resolve `ORBIT_TOKEN_FILE` at startup; inline `--token`/`ORBIT_TOKEN` remains
available. Database URLs may use `--database-url-file`/`ORBIT_DATABASE_URL_FILE`
instead of `DATABASE_URL`. Do not supply both URL sources or both nonempty token
sources. Files must be absolute, regular, non-symlink, private (no group/other bits),
at most 8 KiB and hold a single credential of at least 24 bytes.

To rotate a worker token, drain/stop the worker, update both its private credential
and server-owned reference, restart the server, start the worker and resume it.
For an operator token, coordinate dependent clients, replace the private file
atomically while stopped, restart and confirm old-token 401/new-token success.
The restart also terminates existing streams. Keep tokens out of history/logs.

`POSTGRES_PASSWORD_FILE` initializes a database: changing it does not rotate an
existing role password. Use a separate authenticated database administration
session to rotate the role, update the private database URL, then restart/check
clients during maintenance. Do not log the SQL/password. Provider credentials
belong to the trusted runtime and require provider-specific rotation coordination.
