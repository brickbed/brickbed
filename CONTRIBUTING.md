# Contributing to Brickbed

Thanks for contributing. Brickbed is an alpha database server, so changes to storage, API behavior, authorization, or search need tests and a compatibility note.

## Before you start

Open an issue or discussion for a substantial feature, API change, or storage-format change. Small focused fixes can go straight to a pull request.

Do not include secrets, production data, benchmark corpora, or private infrastructure configuration in a pull request.

## Development setup

Install a current Rust toolchain, Bun, Python 3.10+, and uv. Docker is required to test the image.

```bash
cd server
cargo test --all-targets
cargo clippy --all-targets -- -D warnings

cd ../clients/typescript
bun install
bun test
bun run typecheck
bun run build

cd ../../schemas/typescript
bun install
bun test
bun run typecheck
bun run build

cd ../../clients/python
uv sync --locked
uv run --with ruff ruff check .
uv run python -m compileall brickbed

cd ../../server
docker build -t brickbed:dev .
```

Run `scripts/smoke.sh` against a running server before submitting a change that affects the HTTP API.

## Pull requests

- Keep each pull request scoped to one concern.
- Add or update tests for changed behavior.
- Update `docs/` and the changelog for user-visible changes.
- Preserve backward compatibility unless the pull request explicitly documents a breaking change.
- Never silently discard malformed or unreadable stored documents; return a clear error and add a regression test.

## Code style

Run the checks above before opening a pull request. Use `cargo fmt` for Rust. Keep public API names and error responses deliberate: SDKs and users rely on them.

## Community standards

By participating, you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
