# Configuration

Configure the server with environment variables. Invalid critical configuration causes startup to fail rather than running with a different storage or authentication policy.

## Server

| Variable | Default | Meaning |
| --- | --- | --- |
| `HOST` | `0.0.0.0` | Bind address. |
| `PORT` | `3001` | HTTP port. |
| `STORAGE_PATH` | `./data` | Local storage directory when `S3_BUCKET` is unset. |
| `DB_PATH` | `brickbed` | Database path inside the storage backend. |
| `API_KEYS` | `dev-key:demo` | Comma-separated `key:project` grants. `*` grants every project. |

## S3-compatible object storage

Set `S3_BUCKET` to enable S3-compatible storage. Then both credentials are required.

```bash
S3_BUCKET=brickbed \
S3_ENDPOINT=https://<account-id>.r2.cloudflarestorage.com \
S3_REGION=auto \
S3_ACCESS_KEY_ID=<access-key> \
S3_SECRET_ACCESS_KEY=<secret-key> \
API_KEYS=<production-key>:acme \
cargo run --release
```

| Variable | Meaning |
| --- | --- |
| `S3_BUCKET` | S3-compatible bucket name. |
| `S3_ENDPOINT` | Optional endpoint; required for R2 and typical MinIO configurations. |
| `S3_REGION` | Defaults to `auto`; use an AWS region for Amazon S3. |
| `S3_ACCESS_KEY_ID` | Access key when S3 is enabled. |
| `S3_SECRET_ACCESS_KEY` | Secret key when S3 is enabled. |

## Instance keys

Set `INSTANCE_SECRET` to a 64-character hexadecimal secret to enable instance-derived keys. `INSTANCE_NAME` defaults to `brickbed` and must match when minting and verifying those keys.

## Embed-on-write

| Variable | Meaning |
| --- | --- |
| `EMBEDDINGS_PROVIDER` | `openai`, `cohere`, or `mock`. Unset disables generated embeddings. |
| `OPENAI_API_KEY` | Required for `openai`. |
| `COHERE_API_KEY` | Required for `cohere`. |
| `EMBEDDINGS_BASE_URL` | Optional provider endpoint override. |
| `EMBEDDINGS_MOCK_DIMS` | Width for the deterministic `mock` provider; default `8`. |

Use the mock provider only for development and tests.
