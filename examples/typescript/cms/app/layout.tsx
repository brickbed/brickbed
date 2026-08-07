import type { Metadata } from "next";
import Link from "next/link";
import "./globals.css";

export const metadata: Metadata = {
  title: "Brickbed CMS Example",
  description: "A simple CMS built with Brickbed",
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en">
      <body className="min-h-screen bg-gray-50">
        <nav className="bg-white border-b border-gray-200">
          <div className="max-w-4xl mx-auto px-4 py-4">
            <div className="flex items-center justify-between">
              <Link href="/" className="text-xl font-bold text-gray-900">
                Brickbed CMS
              </Link>
              <div className="flex gap-4">
                <Link
                  href="/"
                  className="text-gray-600 hover:text-gray-900"
                >
                  Posts
                </Link>
                <Link
                  href="/search"
                  className="text-gray-600 hover:text-gray-900"
                >
                  Search
                </Link>
                <Link
                  href="/admin"
                  className="text-gray-600 hover:text-gray-900"
                >
                  Admin
                </Link>
              </div>
            </div>
          </div>
        </nav>
        <main className="max-w-4xl mx-auto px-4 py-8">{children}</main>
      </body>
    </html>
  );
}
