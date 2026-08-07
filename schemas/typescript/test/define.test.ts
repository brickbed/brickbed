import { describe, expect, test } from "bun:test";

import { defineCollection, defineSchema, v } from "../src/index.js";

describe("v.vector", () => {
  test("emits type and dims", () => {
    expect(v.vector(3)).toEqual({ type: "vector", dims: 3 });
  });

  test("emits from/model only when provided", () => {
    expect(v.vector(1536, { from: ["title", "content"], model: "text-embedding-3-small" })).toEqual({
      type: "vector",
      dims: 1536,
      from: ["title", "content"],
      model: "text-embedding-3-small",
    });
    expect(Object.keys(v.vector(4, { model: "m" }))).toEqual([
      "type",
      "dims",
      "model",
    ]);
  });

  test("rejects non-positive and fractional dims", () => {
    expect(() => v.vector(0)).toThrow(/positive integer/);
    expect(() => v.vector(-1)).toThrow(/positive integer/);
    expect(() => v.vector(1.5)).toThrow(/positive integer/);
  });
});

describe("defineSchema wire format", () => {
  test("emits indexes, searchIndexes and vectorIndexes", () => {
    const schema = defineSchema({
      posts: defineCollection({
        title: v.string(),
        content: v.string(),
        embedding: v.vector(1536),
      })
        .index("by_title", ["title"])
        .searchIndex("search_body", { fields: ["title", "content"] })
        .vectorIndex("by_embedding", { field: "embedding" }),
    });

    expect(schema).toEqual({
      collections: {
        posts: {
          fields: {
            title: { type: "string" },
            content: { type: "string" },
            embedding: { type: "vector", dims: 1536 },
          },
          indexes: [{ name: "by_title", fields: ["title"] }],
          searchIndexes: [
            { name: "search_body", fields: ["title", "content"] },
          ],
          vectorIndexes: [
            {
              name: "by_embedding",
              field: "embedding",
              metric: "cosine",
              dims: 1536,
            },
          ],
        },
      },
    });
  });

  test("collections without search stay backwards compatible", () => {
    const schema = defineSchema({
      authors: defineCollection({ name: v.string() }),
    });

    expect(schema.collections.authors).toEqual({
      fields: { name: { type: "string" } },
      indexes: [],
      searchIndexes: [],
      vectorIndexes: [],
    });
  });
});

describe("vectorIndex", () => {
  test("honours an explicit metric", () => {
    const schema = defineSchema({
      posts: defineCollection({ embedding: v.vector(8) }).vectorIndex(
        "by_embedding",
        { field: "embedding", metric: "dot" }
      ),
    });

    expect(schema.collections.posts?.vectorIndexes[0]).toEqual({
      name: "by_embedding",
      field: "embedding",
      metric: "dot",
      dims: 8,
    });
  });

  test("derives dims through v.optional", () => {
    const schema = defineSchema({
      posts: defineCollection({
        embedding: v.optional(v.vector(64)),
      }).vectorIndex("by_embedding", { field: "embedding" }),
    });

    expect(schema.collections.posts?.vectorIndexes[0]?.dims).toBe(64);
  });

  test("rejects an unknown field", () => {
    expect(() =>
      defineCollection({ title: v.string() }).vectorIndex("by_embedding", {
        field: "embedding",
      })
    ).toThrow(/no field "embedding"/);
  });

  test("rejects a non-vector field", () => {
    expect(() =>
      defineCollection({ title: v.string() }).vectorIndex("by_title", {
        field: "title",
      })
    ).toThrow(/is string, expected v.vector\(\)/);
  });
});

describe("index name validation", () => {
  test("searchIndex needs at least one field", () => {
    expect(() =>
      defineCollection({ title: v.string() }).searchIndex("empty", {
        fields: [],
      })
    ).toThrow(/at least one field/);
  });

  test("duplicate names within a kind are rejected", () => {
    expect(() =>
      defineSchema({
        posts: defineCollection({ title: v.string(), body: v.string() })
          .searchIndex("search", { fields: ["title"] })
          .searchIndex("search", { fields: ["body"] }),
      })
    ).toThrow(/duplicate search index "search"/);
  });

  test("an index and a search index may share a name", () => {
    expect(() =>
      defineSchema({
        posts: defineCollection({ title: v.string() })
          .index("by_title", ["title"])
          .searchIndex("by_title", { fields: ["title"] }),
      })
    ).not.toThrow();
  });
});
