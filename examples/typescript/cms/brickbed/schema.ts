import { defineSchema, defineCollection, v } from "@brickbed/schema";

export default defineSchema({
  posts: defineCollection({
    title: v.string(),
    slug: v.string(),
    content: v.string(),
    excerpt: v.optional(v.string()),
    coverImage: v.optional(v.string()),
    author: v.id("authors"),
    status: v.union(v.literal("draft"), v.literal("published")),
    publishedAt: v.optional(v.number()),
    tags: v.array(v.string()),
  })
    .index("by_slug", ["slug"])
    .index("by_status", ["status", "publishedAt"])
    .searchIndex("search", { fields: ["title", "excerpt", "content", "tags"] }),

  authors: defineCollection({
    name: v.string(),
    email: v.string(),
    bio: v.optional(v.string()),
    avatar: v.optional(v.string()),
  }),

  pages: defineCollection({
    title: v.string(),
    slug: v.string(),
    content: v.string(),
  }).index("by_slug", ["slug"]),
});
