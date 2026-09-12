# Offline backup and restore

The alpha utility supports a stopped PostgreSQL + local-artifact deployment.
It preserves the full database/all regular local files, checks accepted artifact
hashes/sizes and restores only to an empty replacement database and a NEW artifact
directory. Requirements: Python 3.11+ and PostgreSQL tools inside a named
Docker/Podman database container. Existing tables, volumes and files are never
automatically deleted or overwritten.

## Create

1. Stop submissions, drain every worker and finish or explicitly resolve active
   work. Stop workers, every server and all other database/artifact writers.
   Keep PostgreSQL running.
2. Resolve the exact database container name from the deployment listing; do not
   select it using a broad glob.
3. Choose a NEW private backup directory outside the artifact root, with an
   existing parent. Substitute verified names and paths:

```sh
python3 scripts/orbit_backup.py create --runtime podman \
  --database-container orbit-alpha-postgres \
  --artifacts /absolute/installation/artifacts \
  --bundle /absolute/backups/orbit-2026-09-12 --offline-confirmed
python3 scripts/orbit_backup.py verify --bundle /absolute/backups/orbit-2026-09-12
```

The utility rejects other database clients, missing/corrupt accepted artifacts,
symlinks/devices and existing destinations. Offline confirmation is essential:
the checks cannot prevent a new writer connecting afterward. Stay stopped.

`database.dump` is custom-format `pg_dump`; `artifacts/` contains verified files;
`manifest.json` is written last. Failure retains an incomplete directory, not an
automatic retry. Hashes detect corruption, not malicious replacement of both
content and manifest. Only restore trusted bundles.

Backups contain sensitive inputs, plans, leases, audit and command output; they
are not redacted evidence. Encrypt/access-control off-host copies using the chosen
backup system. Separately protect server config, credential sources, repository
revisions, image IDs and worker runtime config; these are not in the bundle.
Establish retention/RPO/RTO; the repository does not schedule or upload backups.

## Restore

Provision separate compatible-major PostgreSQL, but do not start Orbit against it.
The artifact destination must not exist, even as an empty directory.

```sh
python3 scripts/orbit_backup.py restore --runtime podman \
  --database-container orbit-replacement-postgres \
  --artifacts /absolute/replacement/restored-artifacts \
  --bundle /absolute/backups/orbit-2026-09-12 --offline-confirmed
```

Verification precedes writes. Restore refuses user tables, copies files exclusively
and uses transactional `pg_restore --exit-on-error`. Configure the replacement
mount to the restored artifact directory and provide private credentials. Start
a compatible pinned image, compare selected run/journal records and download/hash
accepted artifacts before resuming workers. Preserve the old installation until
operator acceptance. Failed restore retains its NEW destination for inspection;
it never touched the old installation. Do not reinitialize over restored data.

## Limits and drill

S3 installations need a coordinated object snapshot/version retention covering
every database reference. The utility refuses accepted non-local artifacts; it
does not back up buckets, external effects/sessions or repositories. Database-only
recovery is insufficient when artifacts live elsewhere.

`python3 scripts/backup-drill.py --runtime podman --image localhost/orbit:alpha`
compares run, journal and accepted bytes across an isolated restore. Recorded
results are in [qualification](qualification.md).
