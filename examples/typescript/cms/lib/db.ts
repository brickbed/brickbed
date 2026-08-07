import { createClient, type Document } from "@brickbed/client";

export interface Post extends Document {
  title: string;
  slug: string;
  content: string;
  excerpt?: string;
  coverImage?: string;
  author: string;
  status: "draft" | "published";
  publishedAt?: number;
  tags: string[];
}

export interface Author extends Document {
  name: string;
  email: string;
  bio?: string;
  avatar?: string;
}

export const db = createClient({
  endpoint: process.env.BRICKBED_ENDPOINT || "http://localhost:3001",
  apiKey: process.env.BRICKBED_API_KEY || "dev-key",
  projectId: process.env.BRICKBED_PROJECT || "demo",
});

export const posts = db.collection<Post>("posts");
export const authors = db.collection<Author>("authors");
