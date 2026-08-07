# Changelog

All notable changes to Brickbed are documented here.

## Unreleased

- Established the public Brickbed repository and release workflow.
- **Breaking:** non-success HTTP responses now use the v1 error envelope with
  `error.code`, `error.message`, optional `error.details`, and `requestId`.
  Replace parsing of the former `{ "error": "…" }` string with the stable
  error code. TypeScript and Python SDK errors expose `status`, `code`,
  `message`, `details`, `requestId`/`request_id`, and raw `body`.

## 0.1.0 — unreleased alpha

- Initial public alpha of the Rust server, TypeScript SDK and schema helpers, Python client, and TypeScript CMS example.
