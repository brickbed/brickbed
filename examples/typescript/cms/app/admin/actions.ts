"use server";

import { revalidatePath } from "next/cache";
import { redirect } from "next/navigation";
import { posts } from "@/lib/db";

interface CreatePostData {
  title: string;
  content: string;
  excerpt: string;
  tags: string;
  status: "draft" | "published";
}

export async function createPost(formData: FormData): Promise<void> {
  const data: CreatePostData = {
    title: formData.get("title") as string,
    content: formData.get("content") as string,
    excerpt: formData.get("excerpt") as string,
    tags: formData.get("tags") as string,
    status: (formData.get("status") as "draft" | "published") || "draft",
  };

  if (!data.title || !data.content) {
    throw new Error("Title and content are required");
  }

  const slug = data.title
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "");

  const tagArray = data.tags
    ? data.tags.split(",").map((t) => t.trim()).filter(Boolean)
    : [];

  await posts.insert({
    title: data.title,
    slug,
    content: data.content,
    excerpt: data.excerpt || undefined,
    author: "default",
    status: data.status,
    publishedAt: data.status === "published" ? Date.now() : undefined,
    tags: tagArray,
  });

  revalidatePath("/");
  redirect("/");
}
