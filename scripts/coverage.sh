#!/usr/bin/env bash
# Generate the three public coverage reports and enforce the Brickbed ratchet.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

rm -rf coverage
mkdir -p coverage/typescript-client coverage/typescript-schema

cargo llvm-cov clean --manifest-path server/Cargo.toml
cargo llvm-cov --manifest-path server/Cargo.toml --all-targets --no-report
cargo llvm-cov report --manifest-path server/Cargo.toml --lcov --output-path coverage/rust.lcov
cargo llvm-cov report --manifest-path server/Cargo.toml --html --output-dir coverage/rust-html

(
  cd clients/typescript
  bun test --coverage --coverage-reporter=lcov --coverage-dir="$repo_root/coverage/typescript-client"
)
(
  cd schemas/typescript
  bun test --coverage --coverage-reporter=lcov --coverage-dir="$repo_root/coverage/typescript-schema"
)

python3 scripts/render-coverage-html.py \
  --output coverage/index.html \
  --report rust=coverage/rust.lcov \
  --report typescript-client=coverage/typescript-client/lcov.info \
  --report typescript-schema=coverage/typescript-schema/lcov.info

python3 scripts/check-coverage.py \
  --report rust=coverage/rust.lcov=. \
  --report typescript-client=coverage/typescript-client/lcov.info=clients/typescript \
  --report typescript-schema=coverage/typescript-schema/lcov.info=schemas/typescript \
  "$@"
