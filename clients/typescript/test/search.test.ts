import { afterEach, describe, expect, test } from "bun:test";

import {
  BrickbedError,
  createClient,
  type SchemaPayload,
  type SearchOptions,
} from "../src/index.js";

const realFetch = globalThis.fetch;

interface Call {
  url: string;
  init: RequestInit;
}

/** Stub global fetch, capturing every request the client makes. */
function stubFetch(responder: (call: Call) => Response): Call[] {
  const calls: Call[] = [];
  globalThis.fetch = (async (input: RequestInfo | URL, init: RequestInit = {}) => {
    const call = { url: String(input), init };
    calls.push(call);
    return responder(call);
  }) as typeof fetch;
  return calls;
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function client() {
  return createClient({
    endpoint: "http://localhost:3001/",
    apiKey: "dev-key",
    projectId: "demo",
  });
}

function postsCollection() {
  return client().collection("posts");
}

function body(call: Call): Record<string, unknown> {
  return JSON.parse(call.init.body as string);
}

afterEach(() => {
  globalThis.fetch = realFetch;
});

describe("collection.search", () => {
  test("posts to _search and returns scored documents", async () => {
    const hit = {
      _id: "01H",
      _createdAt: 1,
      _updatedAt: 2,
      title: "Hello",
      _score: 1.25,
    };
    const calls = stubFetch(() => json({ data: [hit] }));

    const results = await client().collection("posts").search({
      query: "hello",
      limit: 5,
    });

    expect(results).toEqual([hit]);
    expect(results[0]?._score).toBe(1.25);
    expect(calls).toHaveLength(1);
    expect(calls[0]?.url).toBe("http://localhost:3001/v1/demo/posts/_search");
    expect(calls[0]?.init.method).toBe("POST");
    expect(
      (calls[0]?.init.headers as Record<string, string>).Authorization
    ).toBe("Bearer dev-key");
    expect(body(calls[0]!)).toEqual({
      query: "hello",
      mode: "text",
      limit: 5,
    });
  });

  test("infers vector mode and passes index, filter and vector through", async () => {
    const calls = stubFetch(() => json({ data: [] }));

    await client()
      .collection("posts")
      .search({
        vector: [0.1, 0.2],
        index: "by_embedding",
        filter: { index: "by_status", params: { status: "published" } },
      });

    expect(body(calls[0]!)).toEqual({
      vector: [0.1, 0.2],
      index: "by_embedding",
      mode: "vector",
      filter: { index: "by_status", params: { status: "published" } },
    });
  });

  test("an explicit mode wins over inference", async () => {
    const calls = stubFetch(() => json({ data: [] }));

    // Inference would say "vector" here. The missing query is the server's to
    // reject, so the client forwards the caller's mode untouched.
    await client().collection("posts").search({ vector: [0.1], mode: "hybrid" });

    expect(body(calls[0]!).mode).toBe("hybrid");
  });

  test("query plus vector infers hybrid", async () => {
    const calls = stubFetch(() => json({ data: [] }));

    await client().collection("posts").search({ query: "hi", vector: [0.1] });

    expect(body(calls[0]!).mode).toBe("hybrid");
  });

  test("requires a query or a vector", async () => {
    const calls = stubFetch(() => json({ data: [] }));

    await expect(client().collection("posts").search({})).rejects.toThrow(
      'search requires "query" or "vector"'
    );
    await expect(
      client().collection("posts").search({ mode: "text" })
    ).rejects.toThrow('search requires "query" or "vector"');
    expect(calls).toHaveLength(0);
  });

  test("hybrid sends textIndex and vectorIndex", async () => {
    const calls = stubFetch(() => json({ data: [] }));

    await client().collection("posts").search({
      query: "hi",
      vector: [0.1],
      textIndex: "search_body",
      vectorIndex: "by_embedding",
      limit: 20,
    });

    expect(body(calls[0]!)).toEqual({
      query: "hi",
      vector: [0.1],
      textIndex: "search_body",
      vectorIndex: "by_embedding",
      mode: "hybrid",
      limit: 20,
    });
  });

  test("hybrid may name neither arm's index", async () => {
    const calls = stubFetch(() => json({ data: [] }));

    await client()
      .collection("posts")
      .search({ query: "hi", vector: [0.1], mode: "hybrid" });

    expect(body(calls[0]!)).toEqual({
      query: "hi",
      vector: [0.1],
      mode: "hybrid",
    });
  });

  test("hybrid may name just one arm's index", async () => {
    const calls = stubFetch(() => json({ data: [] }));

    await client()
      .collection("posts")
      .search({ query: "hi", vector: [0.1], textIndex: "search_body" });

    expect(body(calls[0]!)).toEqual({
      query: "hi",
      vector: [0.1],
      textIndex: "search_body",
      mode: "hybrid",
    });
  });
});

describe("search index-parameter rules", () => {
  const rejects = async (options: SearchOptions, message: string) => {
    const calls = stubFetch(() => json({ data: [] }));
    await expect(postsCollection().search(options)).rejects.toThrow(message);
    expect(calls).toHaveLength(0);
  };

  test("index is rejected in explicit hybrid", async () => {
    await rejects(
      { query: "hi", vector: [0.1], mode: "hybrid", index: "search_body" },
      '"index" is ambiguous in hybrid search; use "textIndex" and "vectorIndex"'
    );
  });

  test("index is rejected in inferred hybrid", async () => {
    await rejects(
      { query: "hi", vector: [0.1], index: "search_body" },
      '"index" is ambiguous in hybrid search; use "textIndex" and "vectorIndex"'
    );
  });

  test("textIndex is rejected in text mode", async () => {
    await rejects(
      { query: "hi", textIndex: "search_body" },
      '"textIndex" only applies to hybrid search; name the index with "index"'
    );
  });

  test("vectorIndex is rejected in text mode", async () => {
    await rejects(
      { query: "hi", vectorIndex: "by_embedding" },
      '"vectorIndex" only applies to hybrid search; name the index with "index"'
    );
  });

  test("textIndex is rejected in vector mode", async () => {
    await rejects(
      { vector: [0.1], textIndex: "search_body" },
      '"textIndex" only applies to hybrid search; name the index with "index"'
    );
  });

  test("vectorIndex is rejected in vector mode", async () => {
    await rejects(
      { vector: [0.1], vectorIndex: "by_embedding" },
      '"vectorIndex" only applies to hybrid search; name the index with "index"'
    );
  });

  // The server owns these rejections; the client must forward them untouched
  // rather than growing its own copy of the rules.
  test("a mode missing one arm's input is forwarded, not rejected", async () => {
    const calls = stubFetch(() => json({ data: [] }));

    await postsCollection().search({ query: "hi", mode: "hybrid" });

    expect(body(calls[0]!)).toEqual({ query: "hi", mode: "hybrid" });
  });

  test("a contradictory mode and input pair is forwarded, not rejected", async () => {
    const calls = stubFetch(() => json({ data: [] }));

    await postsCollection().search({ query: "hi", vector: [0.1], mode: "text" });
    await postsCollection().search({ query: "hi", vector: [0.1], mode: "vector" });

    expect(body(calls[0]!)).toEqual({
      query: "hi",
      vector: [0.1],
      mode: "text",
    });
    expect(body(calls[1]!)).toEqual({
      query: "hi",
      vector: [0.1],
      mode: "vector",
    });
  });

  test("index is still accepted by a single-arm search", async () => {
    const calls = stubFetch(() => json({ data: [] }));

    await postsCollection().search({ query: "hi", index: "search_body" });

    expect(body(calls[0]!)).toEqual({
      query: "hi",
      index: "search_body",
      mode: "text",
    });
  });
});

describe("collection.search errors", () => {
  test("surfaces a JSON error body as its message", async () => {
    stubFetch(() => json({ error: "unknown search index \"nope\"" }, 400));

    const err = (await client()
      .collection("posts")
      .search({ query: "hi" })
      .catch((e) => e)) as BrickbedError;

    expect(err).toBeInstanceOf(BrickbedError);
    expect(err.status).toBe(400);
    expect(err.message).toBe('Brickbed error (400): unknown search index "nope"');
    expect(err.body).toBe('{"error":"unknown search index \\"nope\\""}');
  });
});

describe("error handling", () => {
  test("a plain-text 422 becomes a BrickbedError, not a parse throw", async () => {
    stubFetch(
      () =>
        new Response("Failed to deserialize the JSON body: invalid type", {
          status: 422,
          headers: { "Content-Type": "text/plain; charset=utf-8" },
        })
    );

    const err = (await client()
      .collection("posts")
      .insert({ title: 1 } as never)
      .catch((e) => e)) as BrickbedError;

    expect(err).toBeInstanceOf(BrickbedError);
    expect(err.status).toBe(422);
    expect(err.message).toBe(
      "Brickbed error (422): Failed to deserialize the JSON body: invalid type"
    );
  });

  test("an empty error body falls back to the status text", async () => {
    stubFetch(() => new Response("", { status: 502, statusText: "Bad Gateway" }));

    const err = (await client()
      .collection("posts")
      .list()
      .catch((e) => e)) as BrickbedError;

    expect(err.status).toBe(502);
    expect(err.message).toBe("Brickbed error (502): Bad Gateway");
    expect(err.body).toBe("");
  });

  test("a non-JSON success body becomes a BrickbedError", async () => {
    stubFetch(() => new Response("<html>proxy</html>", { status: 200 }));

    const err = (await client()
      .collection("posts")
      .search({ query: "hi" })
      .catch((e) => e)) as BrickbedError;

    expect(err).toBeInstanceOf(BrickbedError);
    expect(err.status).toBe(200);
    expect(err.body).toBe("<html>proxy</html>");
    expect(err.message).toBe(
      "Brickbed error (200): expected a JSON response body"
    );
  });

  test("an empty success body is an error, not an undefined document", async () => {
    stubFetch(() => new Response("", { status: 200 }));

    const err = (await client()
      .collection("posts")
      .get("01H")
      .catch((e) => e)) as BrickbedError;

    expect(err).toBeInstanceOf(BrickbedError);
    expect(err.status).toBe(200);
    expect(err.message).toBe(
      "Brickbed error (200): expected a JSON response body"
    );
  });

  test("pushSchema reports a plain-text rejection", async () => {
    stubFetch(() => new Response("missing collections", { status: 422 }));

    const err = (await client()
      .pushSchema({ collections: {} })
      .catch((e) => e)) as BrickbedError;

    expect(err).toBeInstanceOf(BrickbedError);
    expect(err.message).toBe("Brickbed error (422): missing collections");
  });

  test("get returns null on 404 and 204 delete resolves", async () => {
    stubFetch((call) =>
      call.init.method === "DELETE"
        ? new Response(null, { status: 204 })
        : json({ error: "not found" }, 404)
    );

    const posts = client().collection("posts");
    expect(await posts.get("missing")).toBeNull();
    expect(await posts.delete("gone")).toBeUndefined();
  });
});

describe("pushSchema", () => {
  test("sends searchIndexes and vectorIndexes verbatim", async () => {
    const calls = stubFetch(() => json({}));
    const schema = {
      collections: {
        posts: {
          fields: { title: { type: "string" }, embedding: { type: "vector", dims: 3 } },
          indexes: [],
          searchIndexes: [{ name: "search_body", fields: ["title"] }],
          vectorIndexes: [
            {
              name: "by_embedding",
              field: "embedding",
              metric: "cosine" as const,
              dims: 3,
            },
          ],
        },
      },
    };

    await client().pushSchema(schema);

    expect(calls[0]?.url).toBe("http://localhost:3001/v1/demo/_schema");
    expect(calls[0]?.init.method).toBe("PUT");
    expect(body(calls[0]!)).toEqual(schema);
  });

  test("sends auth and rules verbatim, with no cast at the call site", async () => {
    const calls = stubFetch(() => json({}));
    // `satisfies` rather than a cast: the point of this test is that the shape
    // is accepted on its own merits.
    const schema = {
      auth: {
        providers: [
          {
            issuer: "https://idp.example.com",
            audience: "my-app",
            jwksUrl: "https://idp.example.com/.well-known/jwks.json",
            algorithms: ["RS256"],
          },
        ],
      },
      collections: {
        posts: {
          fields: { title: { type: "string" }, authorId: { type: "string" } },
          indexes: [],
          rules: {
            read: "public",
            write: { create: "authenticated", update: { owner: "authorId" } },
          },
        },
        profiles: {
          fields: { ownerId: { type: "string" } },
          indexes: [],
          rules: { read: { owner: "ownerId", match: "tokenIdentifier" } },
        },
      },
    } satisfies SchemaPayload;

    await client().pushSchema(schema);

    expect(body(calls[0]!)).toEqual(schema);
  });
});
