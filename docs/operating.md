# Operating Brickbed

Brickbed currently runs as a single writer for one database path. This guide covers the operating constraints of the alpha server.

## Run one writer

Run exactly one writer process for each `DB_PATH` in a storage backend. A second writer can fence the first; do not use overlapping rolling deployments or autoscaling against the same database path.

To run a second independent database, give it a different database path and treat it as a separate database.

Read replicas and multi-region routing are not shipped in this repository. Do not advertise an object-storage deployment as multi-region just because the underlying bucket is reachable from multiple regions.

## Docker

Build from the server directory:

```bash
docker build -t brickbed:local ./server
```

Run locally with a persistent local directory:

```bash
docker run --rm -p 3001:3001 \
  -v "$PWD/data:/data" \
  -e STORAGE_PATH=/data \
  -e API_KEYS=local-key:demo \
  brickbed:local
```

For S3-compatible storage, pass the variables in [configuration](configuration.md) as secrets. Do not put credentials in images or shell history.

## Health and smoke tests

`GET /health` confirms that the process is serving HTTP. `GET /ready` performs a real database read, so a fenced or unavailable engine is removed from service. Neither endpoint proves backup recoverability or acceptable query latency.

Run the smoke test after a deployment:

```bash
scripts/smoke.sh https://your-server.example.com <api-key> <project>
```

It creates and removes a document in the `smoke` collection. Use a dedicated project in production validation.

## Backups and restore

Object storage durability is not a tested restore procedure. Before storing important data, configure bucket versioning or replication as appropriate, create a backup policy, and regularly prove that a clean environment can restore a usable database.

Brickbed does not yet provide a point-in-time-recovery or logical export/import interface. Plan retention and incident response around that limitation.

## Capacity and safety limits

The alpha server has no multi-tenant quota system. Treat request body size, collection cardinality, vector corpus size, and search frequency as capacity planning inputs. Vector search is brute-force, and large collections increase memory and latency needs.

Monitor process logs and resource usage. Test your exact S3 or R2 provider with failure and restart scenarios before production use.
