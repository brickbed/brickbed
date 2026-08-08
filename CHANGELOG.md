# Changelog

All notable changes to Brickbed are documented here.

## Unreleased

## 0.2.0 — 2026-08-08

- **Breaking:** non-success HTTP responses now use the v1 error envelope with
  `error.code`, `error.message`, optional `error.details`, and `requestId`.
  Replace parsing of the former `{ "error": "…" }` string with the stable
  error code. TypeScript and Python SDK errors expose `status`, `code`,
  `message`, `details`, `requestId`/`request_id`, and raw `body`.
- Split the Rust server database core into focused domain modules for documents,
  indexes, schemas, search, writes, lifecycle, and administration.
- Added coverage ratchet gates for the Rust server and TypeScript packages,
  along with scheduled mutation sampling for correctness-critical server code.

## 0.1.0 — 2026-08-07

- Initial public alpha of the Rust server, TypeScript SDK and schema helpers, Python client, and TypeScript CMS example.
