import { describe, expect, test } from "bun:test";

import { defineCollection, defineSchema, v } from "../src/index.js";

const PROVIDER = { issuer: "https://idp.example.com" };
const auth = { providers: [PROVIDER] };

describe("collection rules emission", () => {
  test("rules ride alongside the indexes", () => {
    const schema = defineSchema(
      {
        posts: defineCollection({ title: v.string(), authorId: v.string() })
          .index("by_author", ["authorId"])
          .rules({ read: "public", write: { owner: "authorId" } }),
      },
      { auth }
    );

    expect(schema.collections.posts).toEqual({
      fields: { title: { type: "string" }, authorId: { type: "string" } },
      indexes: [{ name: "by_author", fields: ["authorId"] }],
      searchIndexes: [],
      vectorIndexes: [],
      rules: { read: "public", write: { owner: "authorId" } },
    });
  });

  test("a collection without rules omits the key entirely", () => {
    const schema = defineSchema({
      posts: defineCollection({ title: v.string() }),
    });

    expect("rules" in schema.collections.posts!).toBe(false);
    expect("auth" in schema).toBe(false);
  });

  test("every rule form survives to the wire", () => {
    const schema = defineSchema(
      {
        a: defineCollection({ ownerId: v.string() }).rules({
          read: { owner: "ownerId", match: "tokenIdentifier" },
        }),
        b: defineCollection({ ownerId: v.string() }).rules({
          read: "authenticated",
          write: { create: "authenticated", update: { owner: "ownerId" } },
        }),
        c: defineCollection({ title: v.string() }).rules({ read: "public" }),
      },
      { auth }
    );

    expect(schema.collections.a?.rules).toEqual({
      read: { owner: "ownerId", match: "tokenIdentifier" },
    });
    expect(schema.collections.b?.rules).toEqual({
      read: "authenticated",
      write: { create: "authenticated", update: { owner: "ownerId" } },
    });
    expect(schema.collections.c?.rules).toEqual({ read: "public" });
  });
});

describe("auth emission", () => {
  test("providers ride at the top level, verbatim", () => {
    const schema = defineSchema(
      { posts: defineCollection({ title: v.string() }) },
      {
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
      }
    );

    expect(schema.auth).toEqual({
      providers: [
        {
          issuer: "https://idp.example.com",
          audience: "my-app",
          jwksUrl: "https://idp.example.com/.well-known/jwks.json",
          algorithms: ["RS256"],
        },
      ],
    });
  });

  test("structural provider mistakes are rejected", () => {
    const posts = () => ({ posts: defineCollection({ title: v.string() }) });

    expect(() => defineSchema(posts(), { auth: { providers: [] } })).toThrow(
      "auth: providers must not be empty"
    );
    expect(() =>
      defineSchema(posts(), { auth: { providers: [PROVIDER, PROVIDER] } })
    ).toThrow('auth: duplicate issuer "https://idp.example.com"');
    expect(() =>
      defineSchema(posts(), {
        auth: { providers: [{ ...PROVIDER, audience: "" }] },
      })
    ).toThrow("must not be empty");
    expect(() =>
      defineSchema(posts(), {
        auth: { providers: [{ ...PROVIDER, algorithms: [] }] },
      })
    ).toThrow("declares no algorithms");
  });
});

describe("rule checks mirrored from the server", () => {
  test("an owner rule cannot name a vector field", () => {
    expect(() =>
      defineCollection({ embedding: v.vector(3) }).rules({
        read: { owner: "embedding" },
      })
    ).toThrow(/owner rule cannot match on "embedding"/);

    // The server unwraps one level of optional before deciding, so this is the
    // same field as far as the check is concerned.
    expect(() =>
      defineCollection({ embedding: v.optional(v.vector(3)) }).rules({
        write: { create: { owner: "embedding" } },
      })
    ).toThrow(/owner rule cannot match on "embedding"/);
  });

  test("identity-requiring rules need providers", () => {
    const build = (rules: Parameters<ReturnType<typeof defineCollection>["rules"]>[0]) =>
      defineSchema({
        posts: defineCollection({ authorId: v.string() }).rules(rules),
      });

    expect(() => build({ read: "authenticated" })).toThrow(
      /declares no auth providers/
    );
    expect(() => build({ write: { owner: "authorId" } })).toThrow(
      /declares no auth providers/
    );
    expect(() => build({ write: { delete: "authenticated" } })).toThrow(
      /declares no auth providers/
    );
  });

  test("shapes the server's deserializer rejects are caught here", () => {
    // An empty owner field matches nothing; an empty write block reads as
    // "allow writes" while denying every one of them.
    expect(() =>
      defineCollection({ authorId: v.string() }).rules({ read: { owner: "" } })
    ).toThrow("rules: owner rule needs a field name");

    expect(() =>
      defineCollection({ authorId: v.string() }).rules({
        write: { create: { owner: "" } },
      })
    ).toThrow("rules: owner rule needs a field name");

    expect(() =>
      defineCollection({ authorId: v.string() }).rules({ write: {} })
    ).toThrow("rules: write rule is empty");
  });

  test("a purely public collection needs no providers", () => {
    expect(() =>
      defineSchema({
        posts: defineCollection({ title: v.string() }).rules({
          read: "public",
          write: "public",
        }),
      })
    ).not.toThrow();
  });

  test("an owner rule on a non-vector field is fine", () => {
    expect(() =>
      defineSchema(
        {
          posts: defineCollection({
            authorId: v.string(),
            embedding: v.vector(3),
          }).rules({ read: { owner: "authorId" } }),
        },
        { auth }
      )
    ).not.toThrow();
  });
});
