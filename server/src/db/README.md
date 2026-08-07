# Database module ownership

The database façade remains `brickbed_server::db::Db`; this directory separates the implementation by the state and invariants each operation owns.

| Module | Primary ownership |
| --- | --- |
| `lifecycle` | Storage construction and opening the SlateDB writer |
| `documents` | Document validation and create/replace/patch/delete flows |
| `schemas` | Schema persistence and full equality/search/vector rebuilds |
| `equality_indexes` | Collection scans and declared equality-index queries |
| `search` | BM25, vector, hybrid ranking, filtering, and corpus inspection |
| `indexes` | Equality/FTS/vector batch operations, embedding, and vector-cache population |
| `write` | The single cross-domain atomic sequencing and durability boundary |
| `administration` | Close and readiness operations |

## Mutation and lock invariants

All document mutations are orchestrated by `documents` and finish through `write::commit_write`. The write batch contains the document plus every relevant equality, full-text, and vector-index operation. No domain module may durably write part of that state on its own.

Locks are acquired in this order:

1. shared schema lock (exclusive only for schema replacement/rebuild);
2. one document-lock shard for updates and deletes;
3. FTS statistics lock while the final batch is assembled and sequenced.

The schema and document guards are passed into `commit_write`, which sequences the complete batch with `await_durable: false`, releases all locks, and only then waits for a SlateDB flush. This preserves group commit: no Brickbed lock spans durable object-store I/O. The FTS statistics lock is released after sequencing, when successors can observe the published state.

Vector cache invalidation happens after sequencing even if the flush fails, because sequenced values are already readable. A cache fill records the generation before its scan and publishes the result only if no mutation invalidated that generation in the meantime.

## Parallel lane boundaries

Storage/lifecycle work owns `lifecycle`, `write`, and `administration`; document/query work owns `documents` and `equality_indexes`; search work owns `search` and the search-specific portions of `indexes`; operations/security work should remain above the `Db` façade; SDK/integration work consumes the public façade and HTTP API. Changes crossing these boundaries need a coordination review focused on the atomic batch and lock order above.
