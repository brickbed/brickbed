import schema from "../brickbed/schema";
import { db, posts, authors, type Author } from "../lib/db";

/** A post minus the fields the server or the seeder fills in. */
interface SeedPost {
  title: string;
  slug: string;
  content: string;
  excerpt?: string;
  status: "draft" | "published";
  publishedAt?: number;
  tags: string[];
}

/** Fixed publish dates keep re-runs of the seeder deterministic. */
const day = (n: number): number => Date.UTC(2026, 0, n);

/**
 * Deliberately overlapping vocabulary: "object storage" should rank
 * why-object-storage first, then storage-costs, then hello-brickbed, so the
 * BM25 scores on /search are visibly different rather than all ties.
 */
const SEED_POSTS: SeedPost[] = [
  {
    title: "Hello Brickbed",
    slug: "hello-brickbed",
    content:
      "Brickbed is a document database built on object storage. This post lives in R2.",
    excerpt: "Welcome to the demo.",
    status: "published",
    publishedAt: day(1),
    tags: ["announcement"],
  },
  {
    title: "Why object storage",
    slug: "why-object-storage",
    content:
      "Object storage is the cheapest durable storage a database can build on. " +
      "Brickbed keeps every document in object storage, so storage costs scale " +
      "with bytes rather than with provisioned disks, and replication comes free.",
    excerpt: "The economics of R2-backed databases.",
    status: "published",
    publishedAt: day(2),
    tags: ["architecture", "storage"],
  },
  {
    title: "Storage costs compared",
    slug: "storage-costs",
    content:
      "A gigabyte on object storage costs roughly a tenth of a gigabyte on " +
      "provisioned block storage, and egress between regions is free on R2.",
    excerpt: "Rough numbers for a 100 GB content set.",
    status: "published",
    publishedAt: day(3),
    tags: ["storage", "pricing"],
  },
  {
    title: "Full-text search with BM25",
    slug: "full-text-search-bm25",
    content:
      "Search indexes are declared in the schema and maintained on write. " +
      "Queries are tokenized, stemmed and scored with BM25, so the best match " +
      "for a query ranks first instead of whichever document was written last.",
    excerpt: "Ranked search, no separate search cluster.",
    status: "published",
    publishedAt: day(4),
    tags: ["search", "bm25"],
  },
  {
    title: "Single-writer object storage",
    slug: "single-writer-object-storage",
    content:
      "One server process owns writes for a database path. This keeps the " +
      "current consistency model clear while object storage provides durable bytes.",
    excerpt: "The current single-writer deployment model.",
    status: "published",
    publishedAt: day(5),
    tags: ["architecture", "storage"],
  },
  {
    title: "Search in the document server",
    slug: "search-in-the-document-server",
    content:
      "Equality indexes, BM25 postings, and vectors are maintained with the " +
      "document write, so search does not need a separately synchronized service.",
    excerpt: "One API for documents and search.",
    status: "published",
    publishedAt: day(6),
    tags: ["search", "architecture"],
  },
  {
    title: "Draft: roadmap",
    slug: "draft-roadmap",
    content: "Vector search, hybrid ranking, and additional operational tooling...",
    status: "draft",
    tags: [],
  },
];

/** Seeding twice should not duplicate content, so slugs are upserted. */
async function upsertPost(
  post: SeedPost,
  authorId: string
): Promise<"inserted" | "updated"> {
  const [existing] = await posts.query("by_slug", { slug: post.slug });
  const data = { ...post, author: authorId };

  if (existing) {
    await posts.update(existing._id, data);
    return "updated";
  }
  await posts.insert(data);
  return "inserted";
}

async function upsertAuthor(): Promise<Author> {
  const email = "ada@example.com";
  const { data } = await authors.list({ limit: 100 });
  const existing = data.find((a) => a.email === email);
  if (existing) {
    return existing;
  }
  return authors.insert({
    name: "Ada Lovelace",
    email,
    bio: "First programmer.",
  });
}

async function report(query: string): Promise<void> {
  const hits = await posts.search({ query, limit: 5 });
  console.log(`\nSearch "${query}" -> ${hits.length} hit(s)`);
  hits.forEach((hit, i) => {
    console.log(`  ${i + 1}. ${hit._score.toFixed(3)}  ${hit.title}`);
  });
}

async function main() {
  console.log("Pushing schema...");
  await db.pushSchema(schema);

  console.log("Seeding author...");
  const author = await upsertAuthor();

  console.log("Seeding posts...");
  for (const post of SEED_POSTS) {
    const action = await upsertPost(post, author._id);
    console.log(`  ${action} ${post.slug}`);
  }

  const bySlug = await posts.query("by_slug", { slug: "hello-brickbed" });
  console.log(`\nQuery by_slug check: found ${bySlug.length} post(s)`);

  await report("object storage");
  await report("bm25 ranked search");

  console.log("\nDone. Try http://localhost:3000/search?q=object+storage");
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
