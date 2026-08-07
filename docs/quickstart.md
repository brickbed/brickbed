# Quickstart

This guide runs Brickbed locally, creates a schema, writes a document, and searches it. You need Rust and Bun.

## Start the server

```bash
cd server
cargo run
```

The development defaults listen on `http://localhost:3001`, store data in `server/data`, and authorize `dev-key` for the `demo` project. Do not use that default key outside local development.

```bash
curl http://localhost:3001/health
curl http://localhost:3001/ready
```

Expected output:

```json
{"status":"ok"}
```

`/health` reports process liveness. `/ready` performs a database read and
reports whether the server is ready to receive traffic.

## Install the TypeScript packages

```bash
bun add @brickbed/client @brickbed/schema
```

Create a client:

```ts
import { createClient, type Document } from '@brickbed/client';

export interface Post extends Document {
  title: string;
  slug: string;
  content: string;
  status: 'draft' | 'published';
  tags: string[];
}

const db = createClient({
  endpoint: 'http://localhost:3001',
  apiKey: 'dev-key',
  projectId: 'demo',
});

const posts = db.collection<Post>('posts');
```

## Push a schema

```ts
import { defineCollection, defineSchema, v } from '@brickbed/schema';

await db.pushSchema(
  defineSchema({
    posts: defineCollection({
      title: v.string(),
      slug: v.string(),
      content: v.string(),
      status: v.union(v.literal('draft'), v.literal('published')),
      tags: v.array(v.string()),
    })
      .index('by_slug', ['slug'])
      .searchIndex('search', { fields: ['title', 'content', 'tags'] }),
  }),
);
```

Schema pushes rebuild that project's indexes. Treat them as a deployment or administration operation rather than an application request-path operation.

## Write, query, and search

```ts
const post = await posts.insert({
  title: 'Why object storage',
  slug: 'why-object-storage',
  content: 'Object storage can be a durable backing store for documents.',
  status: 'published',
  tags: ['storage'],
});

const [samePost] = await posts.query('by_slug', { slug: post.slug });
const hits = await posts.search({ query: 'object storage', limit: 5 });

console.log(samePost._id, hits[0]?._score);
```

The server assigns `_id`, `_createdAt`, and `_updatedAt`. Client document data must not include `_id`, `_createdAt`, `_updatedAt`, or `_score`; those names are reserved for server metadata and search responses.

Next, read the [HTTP API](http-api.md), [schema and search guide](schema-and-search.md), and [operating guide](operating.md).
