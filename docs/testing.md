# Testing and coverage policy

Brickbed is a database server. A passing status code or a high coverage number does not prove data correctness. This policy makes the minimum evidence for a change explicit and keeps measured coverage from regressing.

## Run the coverage gate

Install the pinned tools used by CI, then run one command from the repository root:

```bash
cargo install cargo-llvm-cov --version 0.8.7 --locked
bun --version # must report 1.3.14
bun install --cwd clients/typescript --frozen-lockfile
bun install --cwd schemas/typescript --frozen-lockfile
scripts/coverage.sh
```

The command writes LCOV reports and HTML to the ignored `coverage/` directory. `coverage/index.html` is the combined summary and `coverage/rust-html/html/index.html` has Rust line detail. CI uploads these artifacts and puts the same summary in the pull-request checks.

The versioned measured baseline lives in [`testing/coverage-baseline.json`](../testing/coverage-baseline.json). Coverage may rise but must not fall below it. The gate also requires at least 90% of changed production lines to be covered. New modules under `server/src/db`, `server/src/storage`, `server/src/backup`, and `server/src/recovery` must reach 95% line coverage. Those paths are correctness-critical because a defect can lose, duplicate, or misrepresent persisted data. The policy and any exclusion are versioned in [`testing/coverage-policy.json`](../testing/coverage-policy.json).

This is a minimum path list, not a loophole: before adding or extracting a Rust module that owns durable state, key encoding, recovery, destructive operations, schema authority, or authorization authority, add its directory prefix to the policy in the same pull request. Renames are evaluated at the destination path. Do not relabel a correctness module as “utility” to avoid the 95% requirement.

## `db/` extraction sequencing

The pending `server/src/db/` extraction must carry its existing behavior tests and land before this coverage-gate pull request, or this pull request must be rebased after it. That establishes an honest measured baseline for the moved code at its destination, rather than exempting a move from measurement. If the gate lands first, the extraction is deliberately subject to the 90% changed-line and 95% new-critical-module requirements; add the tests necessary to meet them rather than weakening the policy.

Coverage includes only source modules reported by Rust/Bun. Generated output, build directories, and examples are excluded because they are not production database code. An unreachable branch must be documented in the test or review; it must not be excluded silently.

## Required test evidence

| Change area | Required evidence |
| --- | --- |
| Storage, recovery, destructive operation | Unit/table tests, model/property test, real HTTP integration, MinIO/process/restart/fault test, recovery-state assertions |
| Index/key encoding/query | Unit/table tests plus model/property tests proving index and document state agree |
| Schema/validation | Unit/table tests for every accepted/rejected boundary, HTTP integration for wire errors |
| Authorization | Unit decision table, HTTP integration for every route/role, regression test for every bypass or confusion bug |
| Search/vector | Unit ranking/filter tests, HTTP integration, persisted-index/restart check when state changes |
| Any database bug | A failing regression test first; fix second. Private tests can supplement but never replace public regression coverage. |

Use table-driven tests for finite inputs. Use property/model tests for arbitrary operation sequences; a randomized test must print its seed and a one-command reproduction. New tests must assert documents, schemas, index/search state, and durability where relevant—not only response status.

## Mutation sample

The scheduled mutation sample checks encoding, validation, and authorization:

```bash
cargo install cargo-mutants --version 27.1.0 --locked
scripts/mutation-sample.sh
```

The target is at least 90% of non-equivalent sampled mutants killed. The scheduled release-gate job parses `outcomes.json`, refuses timeouts, requires a passing baseline run, and fails unless every survivor has a non-expired, reviewed **equivalence** record in [`testing/mutation-allowlist.md`](../testing/mutation-allowlist.md). An actual test gap cannot be deferred through the allowlist. The sample is scheduled rather than a normal PR blocker because it runs each test suite many times; a failed scheduled result blocks release until it is fixed and rerun.

## Pull-request checklist

The repository pull-request template requires authors to record which layers apply: unit, property/model, concurrency, HTTP, MinIO, restart/fault, documentation, and observability. “Not applicable” must name why. Storage or API changes also require a compatibility note.
