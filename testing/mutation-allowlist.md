# Mutation testing allowlist

The mutation sample covers the key encoding, validation, and authorization
modules because a surviving mutant in these paths can silently return the
wrong data or grant access incorrectly.

The target is to kill at least 90% of **non-equivalent** sampled mutants. A
survivor is never silently accepted: record its stable mutant name, proof that
it is equivalent (the reason must begin `Equivalent:`), the issue that owns
the review, and an expiry date below. An actual test gap cannot be allowlisted:
add a test and rerun the sample.

There are no accepted survivors at this baseline.

| Mutant | Reason | Tracking issue | Expires |
| --- | --- | --- | --- |
| _None_ | _No allowlisted survivors_ | — | — |
