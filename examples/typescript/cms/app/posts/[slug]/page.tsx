import Link from "next/link";
import { notFound } from "next/navigation";
import { posts } from "@/lib/db";

export const dynamic = "force-dynamic";

interface PageProps {
  params: Promise<{ slug: string }>;
}

export default async function PostPage({ params }: PageProps) {
  const { slug } = await params;
  const [post] = await posts.query("by_slug", { slug });

  if (!post) {
    notFound();
  }

  return (
    <article>
      <div className="mb-8">
        <Link
          href="/"
          className="text-blue-600 hover:text-blue-800 text-sm"
        >
          &larr; Back to posts
        </Link>
      </div>

      <header className="mb-8">
        <h1 className="text-4xl font-bold text-gray-900 mb-4">{post.title}</h1>
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
            <span>Tags: {post.tags.join(", ")}</span>
          )}
          <span>
            Created: {new Date(post._createdAt).toLocaleDateString()}
          </span>
        </div>
      </header>

      <div className="prose max-w-none">
        <p className="whitespace-pre-wrap text-gray-700">{post.content}</p>
      </div>
    </article>
  );
}
