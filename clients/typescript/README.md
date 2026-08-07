# @brickbed/client

TypeScript client for [Brickbed](https://github.com/brickbed/brickbed), the alpha document database for local and S3-compatible object storage.

Read the repository [quickstart](../../docs/quickstart.md) and [HTTP API](../../docs/http-api.md) before using it with important data.

## Installation

```bash
npm install @brickbed/client
# or
bun add @brickbed/client
```

## Usage

```typescript
import { createClient } from '@brickbed/client';

const db = createClient({
  endpoint: process.env.BRICKBED_ENDPOINT!, // http://localhost:3001
  apiKey: process.env.BRICKBED_API_KEY!,    // sent as `Authorization: Bearer <key>`
  projectId: process.env.BRICKBED_PROJECT!, // required; every key is scoped to a project
});

const posts = db.collection('posts');
```

`projectId` is required — the constructor throws without it. A key is granted one project (or `*`
for all), so using a key outside its project returns 403.

### Typed collections

```typescript
import { createClient, type Document } from '@brickbed/client';

interface Post extends Document {
  title: string;
  slug: string;
  status: 'draft' | 'published';
  tags: string[];
}

const posts = db.collection<Post>('posts');
```

Every document carries `_id` (a server-generated ULID), `_createdAt` and `_updatedAt` (epoch
milliseconds).

### Documents

```typescript
const post = await posts.insert({ title: 'Hello', slug: 'hello', status: 'draft', tags: [] });

const one = await posts.get(post._id);        // null when absent, rather than throwing
await posts.update(post._id, { ...fields });  // full replace (PUT)
await posts.patch(post._id, { status: 'published' }); // shallow merge of top-level keys
await posts.delete(post._id);

const page = await posts.list({ limit: 20, cursor });  // { data, cursor? }
```

`list` returns `cursor` only when more documents follow. `patch` merges at the top level and then
validates the *merged* document, so a patch cannot leave a document invalid.

### Schema

```typescript
import schema from './brickbed/schema';  // defineSchema output from @brickbed/schema

await db.pushSchema(schema);
```

`pushSchema` accepts the whole emitted shape, including the `auth` providers and per-collection
`rules` that govern end-user access. Pushing a schema requires an API key, and rewriting
`auth.providers` requires one granted `*` rather than a single project.

### Query by index

```typescript
const published = await posts.query('by_status', { status: 'published' }, { limit: 10 });
const withCursor = await posts.queryPage('by_status', { status: 'published' });
```

Params must bind a prefix of the index's fields, and the match is equality only.

### Search

```typescript
const hits = await posts.search({ query: 'object storage', limit: 10 });

for (const hit of hits) {
  console.log(hit._score, hit.title);
}
```

`search` returns `Array<T & { _score: number }>`, best first. Options:

| Option | Meaning |
|---|---|
| `query` | text to score with BM25 |
| `vector` | query embedding, `number[]` |
| `index` | which index to use in a single-arm search; defaults to the collection's first of that kind |
| `textIndex` / `vectorIndex` | per-arm index names, **hybrid only** |
| `mode` | `'text'`, `'vector'` or `'hybrid'`; inferred when omitted |
| `limit` | defaults to 10, clamped to 1–1000 |
| `filter` | `{ index, params }` equality predicate applied to hits |

Mode is inferred from what you pass: `query` alone is `text`, `vector` alone is `vector`, both is
`hybrid`. Passing neither throws before any request is made.

```typescript
// vector
await posts.search({ vector: embedding, index: 'by_embedding', limit: 5 });

// hybrid, naming each arm
await posts.search({
  query: 'object storage',
  vector: embedding,
  textIndex: 'search',
  vectorIndex: 'by_embedding',
});

// filtered
await posts.search({
  query: 'object storage',
  filter: { index: 'by_status', params: { status: 'published' } },
});
```

`index` names one index, so hybrid — which runs two arms — rejects it in favour of `textIndex` and
`vectorIndex`. Those two are rejected outside hybrid. The client raises both cases locally, with
the same wording the server uses.

### Understanding `_score`

`_score` is only comparable **within one response**. It is a BM25 score in `text` mode, a cosine or
dot similarity in `vector` mode, and a reciprocal-rank-fusion score in `hybrid` mode.

Hybrid fuses the two rankings by rank, not by score: a document scores `1 / (60 + rank)` for each
arm it appears in, summed. Two consequences are worth knowing before you file a bug:

- Hybrid scores are small and clustered — around 0.016 for a single top-ranked appearance, and
  roughly 0.033 at most. They are not on the same scale as BM25 scores.
- **A document that places in both arms outranks one that tops a single arm.** Ranking third in
  both arms scores `1/63 + 1/63 ≈ 0.0317`, which beats ranking first in text alone (`1/61 ≈
  0.0164`). That is the point of fusion — agreement across arms is the signal — but it does mean
  the best BM25 match is not always the first hybrid result.

Only documents retrieved by an arm count as appearing in it. Hybrid and filtered searches retrieve
four times the requested limit per arm to leave room for fusion and post-filtering.

### Errors

```typescript
import { BrickbedError } from '@brickbed/client';

try {
  await posts.search({ query: 'hi', index: 'nope' });
} catch (err) {
  if (err instanceof BrickbedError) {
    console.log(err.status); // 400
    console.log(err.body);   // raw response body
    console.log(err.code); // 'schema_invalid'
    console.log(err.requestId); // Correlates with the server log
  }
}
```

Errors carry HTTP `status`, stable `code`, human `message`, optional safe
`details`, `requestId`, and the raw `body`. Every error the server returns uses
the [v1 error envelope](../../docs/http-api.md#errors), including rejections
raised before a handler runs — a malformed JSON body
is a 400 and a body that parses but does not fit the endpoint is a 422, both in that shape — and
the message is unwrapped into `err.message`. A non-JSON body is still handled rather than throwing
a parse error, which covers a proxy or gateway answering in place of the server. `get()` is the one
method that maps 404 to `null` instead of throwing.

## License

MIT
