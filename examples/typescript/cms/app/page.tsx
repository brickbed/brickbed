import Link from "next/link";
import { posts, type Post } from "@/lib/db";

export const dynamic = "force-dynamic";

export default async function HomePage() {
  let postList: Post[] = [];
  let error: string | null = null;

  try {
    const result = await posts.list({ limit: 20 });
    postList = result.data;
  } catch (e) {
    error = e instanceof Error ? e.message : "Failed to fetch posts";
  }

  return (
    <div>
      <h1 className="text-3xl font-bold text-gray-900 mb-8">Posts</h1>

      {error && (
        <div className="bg-red-50 border border-red-200 text-red-700 px-4 py-3 rounded mb-6">
          {error}
        </div>
      )}

      {postList.length === 0 && !error ? (
        <div className="text-gray-500 text-center py-12">
          <p className="mb-4">No posts yet.</p>
          <Link
            href="/admin"
            className="text-blue-600 hover:text-blue-800 underline"
          >
            Create your first post
          </Link>
        </div>
      ) : (
        <div className="space-y-6">
          {postList.map((post) => (
            <article
              key={post._id}
              className="bg-white border border-gray-200 rounded-lg p-6"
            >
              <Link href={`/posts/${post.slug}`}>
                <h2 className="text-xl font-semibold text-gray-900 hover:text-blue-600 mb-2">
                  {post.title}
                </h2>
              </Link>
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
                {post.tags.length > 0 && (
                  <span>{post.tags.join(", ")}</span>
                )}
              </div>
            </article>
          ))}
        </div>
      )}
    </div>
  );
}
