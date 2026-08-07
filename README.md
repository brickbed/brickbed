# Brickbed

Brickbed is an open-source JSON document database for object storage. It provides document CRUD, schema validation, equality indexes, BM25 full-text search, vector search, hybrid search, and project-scoped authorization from one Rust server.

Brickbed is currently an **alpha**. It is suitable for local development and evaluation. Read the [operating guide](docs/operating.md) before using it with important data.

## What it is

- A single-writer document server backed by a local directory or S3-compatible object storage.
- One API for documents, equality indexes, BM25 search, vector search, and hybrid search.
- TypeScript client and schema helpers, plus an early Python client.
- Server-side schema validation and project-scoped API keys; optional JWT rules for end-user access.

## What it is not yet

- A multi-region database service or a CMS/editor product.
- A multi-writer or transactional database.
- A replacement for a dedicated approximate-nearest-neighbour system at very large vector counts.

## Quick start

Run the server with its development defaults:

```bash
cd server
cargo run
```

It listens on `http://localhost:3001`, stores data in `./data`, and accepts `dev-key` for project `demo`. In another terminal:

```bash
curl http://localhost:3001/health

curl -X POST http://localhost:3001/v1/demo/posts \
  -H 'Authorization: Bearer dev-key' \
  -H 'Content-Type: application/json' \
  -d '{"title":"Hello Brickbed","status":"draft"}'
```

For a complete working example, see the [quickstart](docs/quickstart.md). For an existing deployment, run `scripts/smoke.sh <endpoint> <api-key> <project>`.

## Packages

- [`server/`](server/) — Rust HTTP server.
- [`clients/typescript/`](clients/typescript/) — `@brickbed/client`.
- [`schemas/typescript/`](schemas/typescript/) — `@brickbed/schema`.
- [`clients/python/`](clients/python/) — early `brickbed` Python client.
- [`examples/typescript/cms/`](examples/typescript/cms/) — a Next.js example application.

## Documentation

- [Quickstart](docs/quickstart.md)
- [HTTP API](docs/http-api.md)
- [Schema and search](docs/schema-and-search.md)
- [Authentication](docs/auth.md)
- [Configuration](docs/configuration.md)
- [Operating Brickbed](docs/operating.md)

## Development

```bash
cd server && cargo test --all-targets
cd ../clients/typescript && bun install && bun test && bun run typecheck && bun run build
cd ../../schemas/typescript && bun install && bun test && bun run typecheck && bun run build
cd ../../clients/python && uv sync --locked && uv run --with ruff ruff check .
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the complete development workflow and [SECURITY.md](SECURITY.md) to report a vulnerability.

## License

Brickbed is licensed under the [MIT License](LICENSE).
