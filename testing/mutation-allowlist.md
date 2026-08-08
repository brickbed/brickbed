# Mutation testing allowlist

The mutation sample covers the key encoding, validation, and authorization
modules because a surviving mutant in these paths can silently return the
wrong data or grant access incorrectly.

The target is to kill at least 90% of **non-equivalent** sampled mutants. A
survivor is never silently accepted: record its stable mutant name, proof that
it is equivalent (the reason must begin `Equivalent:`), the issue that owns
the review, and an expiry date below. An actual test gap cannot be allowlisted:
add a test and rerun the sample.

The one accepted survivor below is a proven equivalent mutant; all other
survivors must be killed by a public test before the release gate can pass.

| Mutant | Reason | Tracking issue | Expires |
| --- | --- | --- | --- |
| src/index.rs:47:22: replace ^ with \| in encode_value | Equivalent: this expression runs only when `f.is_sign_negative()` is false, so IEEE-754 bit 63 is zero and setting it with XOR or OR yields identical bits. | #8 | 2026-11-08 |
