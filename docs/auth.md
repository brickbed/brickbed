# Authentication and authorization

Brickbed supports project-scoped API keys and optional end-user JWT rules.

## API keys

Set `API_KEYS` to comma-separated `key:project` pairs. A project of `*` grants access to every project.

```bash
API_KEYS='writer-key:acme,admin-key:*' cargo run --release
```

Send the key as a bearer token:

```bash
curl http://localhost:3001/v1/acme/posts \
  -H 'Authorization: Bearer writer-key'
```

API keys bypass collection rules and are required to push or read schemas. Keep them in server-side secret storage; never ship one to a browser.

## End-user JWT rules

Add trusted JWT issuers and collection rules in the schema:

```ts
import { defineCollection, defineSchema, v } from '@brickbed/schema';

export default defineSchema(
  {
    posts: defineCollection({
      title: v.string(),
      authorId: v.string(),
    }).rules({
      read: 'public',
      write: { create: 'authenticated', update: { owner: 'authorId' } },
    }),
  },
  {
    auth: {
      providers: [{ issuer: 'https://issuer.example.com', audience: 'your-app' }],
    },
  },
);
```

Rules are `public`, `authenticated`, or `{ owner: 'field' }`. Owner rules compare a claim with the named document field; `match` selects `subject` (default), `tokenIdentifier`, or `email`.

Rules are opt-in. A collection without rules denies end-user access. Treat a public read or write rule as an intentional internet-facing policy, especially because the server permits cross-origin requests.
