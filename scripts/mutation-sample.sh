#!/usr/bin/env bash
# A deliberately bounded, reproducible mutation sample for correctness-critical code.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root/server"

output_dir="${MUTANTS_OUTPUT_DIR:-$repo_root/coverage/mutants}"
rm -rf "$output_dir"
mkdir -p "$(dirname "$output_dir")"
set +e
cargo mutants \
  --file src/index.rs \
  --file src/validate.rs \
  --file src/auth.rs \
  --file src/rules.rs \
  --timeout 120 \
  --output "$output_dir"
mutation_exit=$?
set -e

if [[ ! -f "$output_dir/outcomes.json" ]]; then
  echo "cargo-mutants exited $mutation_exit without outcomes.json" >&2
  exit "$mutation_exit"
fi
python3 "$repo_root/scripts/check-mutation-sample.py" \
  --outcomes "$output_dir/outcomes.json" \
  --missed "$output_dir/missed.txt" \
  --allowlist "$repo_root/testing/mutation-allowlist.md"
