import Link from "next/link";
import type { SearchResult } from "@brickbed/client";
import { posts, type Post } from "@/lib/db";

export const dynamic = "force-dynamic";

interface PageProps {
  searchParams: Promise<{ q?: string | string[] }>;
}

export default async function SearchPage({ searchParams }: PageProps) {
  const { q } = await searchParams;
  // A repeated ?q= arrives as an array; the last one wins, as in a resubmit.
  const query = (Array.isArray(q) ? q[q.length - 1] : q)?.trim() ?? "";

  let results: SearchResult<Post>[] = [];
  let error: string | null = null;

  if (query) {
    try {
      results = await posts.search({ query, limit: 10 });
    } catch (e) {
      error = e instanceof Error ? e.message : "Search failed";
    }
  }

  return (
    <div>
      <h1 className="text-3xl font-bold text-gray-900 mb-8">Search</h1>

      <form action="/search" method="get" className="flex gap-2 mb-8">
        <input
          type="search"
          name="q"
          defaultValue={query}
          placeholder="Search posts..."
          aria-label="Search posts"
          className="flex-1 px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-blue-500"
        />
        <button
          type="submit"
          className="bg-blue-600 text-white py-2 px-6 rounded-lg hover:bg-blue-700 focus:ring-2 focus:ring-blue-500 focus:ring-offset-2"
        >
          Search
        </button>
      </form>

      {error && (
        <div className="bg-red-50 border border-red-200 text-red-700 px-4 py-3 rounded mb-6">
          {error}
        </div>
      )}

      {!query && !error && (
        <p className="text-gray-500 text-center py-12">
          Enter a query to search posts by title, excerpt, content and tags.
        </p>
      )}

      {query && !error && results.length === 0 && (
        <p className="text-gray-500 text-center py-12">
          No posts match &ldquo;{query}&rdquo;.
        </p>
      )}

      {results.length > 0 && (
        <>
          <p className="text-sm text-gray-500 mb-4">
            {results.length} result{results.length === 1 ? "" : "s"} for &ldquo;
            {query}&rdquo;, ranked by BM25 relevance.
          </p>

          <div className="space-y-6">
            {results.map((post, i) => (
              <article
                key={post._id}
                className="bg-white border border-gray-200 rounded-lg p-6"
              >
                <div className="flex items-start justify-between gap-4 mb-2">
                  <Link href={`/posts/${post.slug}`}>
                    <h2 className="text-xl font-semibold text-gray-900 hover:text-blue-600">
                      <span className="text-gray-400 mr-2">{i + 1}.</span>
                      {post.title}
                    </h2>
                  </Link>
                  <span
                    className="shrink-0 px-2 py-1 rounded bg-blue-50 text-blue-700 text-sm font-mono"
                    title="BM25 score"
                  >
                    {post._score.toFixed(3)}
                  </span>
                </div>
                {post.excerpt && (
                  <p className="text-gray-600 mb-3">{post.excerpt}</p>
                )}
                <div className="flex items-center gap-4 text-sm text-gray-500">
                  <span
                    className={`px-2 py-1 rounded ${
                      post.status === "published"
                        ? "bg-green-100 text-green-700"
                        : "bg-yellow-100 text-yellow-700"
                    }`}
                  >
                    {post.status}
                  </span>
                  {post.tags.length > 0 && <span>{post.tags.join(", ")}</span>}
                </div>
              </article>
            ))}
          </div>
        </>
      )}
    </div>
  );
}
