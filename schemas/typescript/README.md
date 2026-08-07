# @brickbed/schema

Schema definition helpers for [Brickbed](https://github.com/brickbed/brickbed), the alpha document database for local and S3-compatible object storage.

Brickbed currently runs as a single writer. Read [the operating guide](../../docs/operating.md) for its deployment constraints.

## Installation

```bash
npm install @brickbed/schema
# or
bun add @brickbed/schema
```

## Usage

A schema is plain data. `defineSchema` returns a JSON object that you push with
`client.pushSchema(schema)`; the server then validates every write against it and maintains the
indexes you declare.

```typescript
import { defineSchema, defineCollection, v } from '@brickbed/schema';

export default defineSchema({
  posts: defineCollection({
    title: v.string(),
    slug: v.string(),
    content: v.string(),
    excerpt: v.optional(v.string()),
    author: v.id('authors'),
    status: v.union(v.literal('draft'), v.literal('published')),
    publishedAt: v.optional(v.number()),
    tags: v.array(v.string()),
    embedding: v.optional(v.vector(1536)),
  })
    .index('by_slug', ['slug'])
    .index('by_status', ['status', 'publishedAt'])
    .searchIndex('search', { fields: ['title', 'excerpt', 'content', 'tags'] })
    .vectorIndex('by_embedding', { field: 'embedding' }),

  authors: defineCollection({
    name: v.string(),
    email: v.string(),
    bio: v.optional(v.string()),
  }),
});
```

## Validators

| Validator | Accepts |
|---|---|
| `v.string()` | a JSON string |
| `v.number()` | a JSON number |
| `v.boolean()` | `true` or `false` |
| `v.array(inner)` | an array whose every item satisfies `inner` |
| `v.object(shape)` | an object; declared keys are checked, extra keys allowed |
| `v.optional(inner)` | absent, `null`, or a value satisfying `inner` |
| `v.id(collection)` | a string; the reference itself is not enforced server-side |
| `v.union(a, b, ...)` | a value satisfying at least one variant |
| `v.literal(value)` | exactly `value` (string, number or boolean) |
| `v.vector(dims, opts?)` | an array of exactly `dims` finite numbers |

Fields you do not declare are still accepted on writes: the schema constrains what it names, it
does not close the document.

### v.vector

```typescript
v.vector(1536)
v.vector(1536, { from: ['title', 'content'], model: 'text-embedding-3-small' })
```

`dims` must be a positive integer, and the server caps it at 4096, since vector search holds a
collection's vectors in memory. Components are stored as 32-bit floats, so a value outside f32
range is rejected on write.

`from` and `model` turn the field into an embed-on-write vector: when a write does not carry the
vector itself, the server concatenates those source fields, sends them to the configured embedding
provider, and stores the result. Writes that supply their own vector are left alone, and a server
with no embedding provider configured simply leaves the field unset.

The server rejects the schema push if `from` is present without a non-empty `model`, if a named
source field does not exist, if it holds no text (strings, arrays of strings, string literals and
unions of those qualify), or if the vector field names itself. A provider failure at write time is
a 502 and nothing is persisted, so a document is never stored without the embedding it declared.

## Indexes

### `.index(name, fields)`

An ordered index for equality lookups through `collection.query()`. Query params must bind a
**prefix** of `fields` — given `['status', 'publishedAt']` you may bind `status`, or both, but not
`publishedAt` alone.

### `.searchIndex(name, { fields })`

A BM25 full-text index. Text is collected from the named fields — strings and nested arrays of
strings contribute, other types are ignored — then lowercased, split on Unicode word boundaries,
filtered against a small English stop list, and stemmed. At least one field is required.

### `.vectorIndex(name, { field, metric })`

A brute-force nearest-neighbour index over a `v.vector()` field. `metric` is `'cosine'` (default)
or `'dot'`, and both rank higher-is-better. The dimension is read from the field's validator rather
than repeated here, and one level of `v.optional()` is unwrapped, so an embedding that has not been
written yet still resolves.

Index maintenance happens in the same write as the document, so a document is searchable as soon as
its insert or update returns.

## End-user access

By default a collection is reachable only with an API key. To let end users in, declare the
identity providers whose JWTs the project trusts, and per-collection rules:

```typescript
export default defineSchema(
  {
    posts: defineCollection({
      title: v.string(),
      authorId: v.string(),
    }).rules({
      read: 'public',
      write: { create: 'authenticated', update: { owner: 'authorId' } },
    }),

    profiles: defineCollection({ ownerId: v.string() }).rules({
      read: { owner: 'ownerId', match: 'tokenIdentifier' },
      write: { owner: 'ownerId' },
    }),
  },
  {
    auth: {
      providers: [
        {
          issuer: 'https://your-tenant.clerk.accounts.dev',
          audience: 'your-app',
        },
      ],
    },
  }
);
```

A rule is `'public'` (no credential at all), `'authenticated'` (any valid token), or
`{ owner: 'field' }` (a token whose identity matches that document field). `match` selects what is
compared: `'subject'` (default, the `sub` claim), `'tokenIdentifier'` (`{issuer}|{subject}`, the
safe choice with two providers), or `'email'`.

`read` takes one rule. `write` takes either one rule for every write, or `{ create, update, delete }`
with any subset. An operation no rule mentions is denied, and a collection with no `.rules()` denies
end users entirely. API keys bypass rules completely.

A provider needs only `issuer`; `audience`, `jwksUrl` and `algorithms` are optional and documented
in the repository [authentication guide](../../docs/auth.md).

Two mistakes are caught here rather than at push time: an owner rule naming a `v.vector()` field,
which the server fills during the write so the document judged would not be the document stored;
and rules that need an identity while no providers are declared. The server additionally enforces
that issuers are https URLs and that algorithms come from its asymmetric allowlist.

## Emitted wire format

`defineSchema` output is exactly what `PUT /v1/{project}/_schema` expects:

```json
{
  "collections": {
    "posts": {
      "fields": {
        "title": { "type": "string" },
        "status": {
          "type": "union",
          "variants": [
            { "type": "literal", "value": "draft" },
            { "type": "literal", "value": "published" }
          ]
        },
        "embedding": {
          "type": "optional",
          "inner": { "type": "vector", "dims": 1536 }
        }
      },
      "indexes": [{ "name": "by_slug", "fields": ["slug"] }],
      "searchIndexes": [
        { "name": "search", "fields": ["title", "excerpt", "content", "tags"] }
      ],
      "vectorIndexes": [
        {
          "name": "by_embedding",
          "field": "embedding",
          "metric": "cosine",
          "dims": 1536
        }
      ],
      "rules": { "read": "public", "write": { "owner": "authorId" } }
    }
  },
  "auth": {
    "providers": [{ "issuer": "https://your-tenant.clerk.accounts.dev" }]
  }
}
```

All three index arrays are always present, and empty when unused. `rules` and `auth` are the
opposite: each is omitted entirely unless declared, which is what the server distinguishes between
"no rules" and "rules that deny".

## Errors

These throw while the schema is being built, before anything reaches the server:

- `v.vector(0)` or `v.vector(1.5)` — dims must be a positive integer
- `.searchIndex('s', { fields: [] })` — a search index needs at least one field
- `.vectorIndex('v', { field: 'title' })` — the field is not a `v.vector()`
- `.vectorIndex('v', { field: 'absent' })` — no such field on the collection
- duplicate index names of the same kind within one collection
- `.rules({ read: { owner: 'embedding' } })` where `embedding` is a `v.vector()` field
- rules needing an end-user identity when no `auth.providers` are declared
- an `auth` block with no providers, a duplicate issuer, an empty `audience`, or an empty
  `algorithms` array

The server rejects a pushed schema with a 400 when a name is unusable. Project and collection names
must match `[a-z0-9][a-z0-9_-]{0,63}`; index names must be 1–64 bytes containing no `:` and no
control characters. An index and a search index may share a name, two indexes of the same kind may
not.

## Links

- [Website](https://brickbed.dev)
- [GitHub](https://github.com/brickbed/brickbed)

## License

MIT
