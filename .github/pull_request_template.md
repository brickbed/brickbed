## Summary

<!-- What changed and why? Link the issue. -->

## Verification

- [ ] Unit/table tests cover every new branch, boundary, state transition, and error mapping.
- [ ] Property/model test added, or not applicable because: <!-- explain -->
- [ ] Concurrency test added, or not applicable because: <!-- explain -->
- [ ] HTTP integration test added, or not applicable because: <!-- explain -->
- [ ] MinIO/process-level test added for persistence/network/lifecycle changes, or not applicable because: <!-- explain -->
- [ ] Restart/fault-injection test added for persistence/destructive changes, or not applicable because: <!-- explain -->
- [ ] New tests assert data and index state, not only response status.
- [ ] `scripts/coverage.sh` passes; changed production lines meet the coverage gate.
- [ ] Randomized tests print a seed and exact reproduction command.

## Compatibility, docs, and observability

- [ ] Compatibility impact documented (or no public behavior/format change).
- [ ] Docs/changelog updated where user-visible behavior changed.
- [ ] Logs, metrics, readiness, or alerting impact considered (or not applicable).
- [ ] No secrets, production data, benchmark corpus, or private infrastructure configuration included.
