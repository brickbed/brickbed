# Schema and search

A project schema declares field validators, equality indexes, BM25 search indexes, vector indexes, and optional end-user access rules. Push it with `PUT /v1/{project}/_schema` or `db.pushSchema()`.

## Define a collection

```ts
import { defineCollection, defineSchema, v } from '@brickbed/schema';

export default defineSchema({
  posts: defineCollection({
    title: v.string(),
    slug: v.string(),
    content: v.string(),
    status: v.union(v.literal('draft'), v.literal('published')),
    tags: v.array(v.string()),
    embedding: v.optional(v.vector(1536)),
  })
    .index('by_slug', ['slug'])
    .index('by_status', ['status'])
    .searchIndex('search', { fields: ['title', 'content', 'tags'] })
    .vectorIndex('by_embedding', { field: 'embedding', metric: 'cosine' }),
});
```

The available validators are `string`, `number`, `boolean`, `literal`, `id`, `array`, `object`, `optional`, `union`, and `vector`. Undeclared application fields are currently accepted; schema fields constrain what they declare rather than closing a document.

`v.id('collection')` validates a string identifier but does not enforce a foreign-key reference.

## Equality indexes

`.index(name, fields)` creates an ordered equality index. Given `['status', 'publishedAt']`, a query may bind `status`, or both fields, but not `publishedAt` alone. Range predicates and ordering by arbitrary fields are not currently available.

## Text search

`.searchIndex(name, { fields })` creates a BM25 index. String fields and nested arrays of strings contribute text. Results are ranked best-first. Text scores are not comparable between separate requests.

## Vector and hybrid search

`.vectorIndex()` performs brute-force nearest-neighbour search over a `v.vector()` field. Use `cosine` (the default) or `dot` similarity. Vectors have a maximum width of 4096 dimensions.

```ts
const vectorHits = await posts.search({
  vector: embedding,
  index: 'by_embedding',
  limit: 10,
});

const hybridHits = await posts.search({
  query: 'object storage',
  vector: embedding,
  textIndex: 'search',
  vectorIndex: 'by_embedding',
  limit: 10,
});
```

Hybrid search combines text and vector rankings with reciprocal rank fusion. It is exact but brute-force vectors are deliberately a scale boundary, not an ANN implementation.

## Embed on write

You can specify `from` and `model` in a vector validator to have the server generate an embedding when a write does not provide one. Configure an embedding provider before using this feature; see [configuration](configuration.md).

```ts
embedding: v.optional(
  v.vector(1536, { from: ['title', 'content'], model: 'text-embedding-3-small' }),
),
```

Embedding-provider errors fail the write rather than saving a partially indexed document.
