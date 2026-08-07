import { BrickbedError, errorFromResponse, parseJson } from "./errors.js";
import type {
  Document,
  ListOptions,
  ListResponse,
  QueryOptions,
  QueryResponse,
  SearchMode,
  SearchOptions,
  SearchResponse,
  SearchResult,
} from "./types.js";

export class Collection<T extends Document = Document> {
  constructor(
    private readonly baseUrl: string,
    private readonly projectId: string,
    private readonly name: string,
    private readonly apiKey: string
  ) {}

  private get url(): string {
    return `${this.baseUrl}/v1/${this.projectId}/${this.name}`;
  }

  private get headers(): Record<string, string> {
    return {
      "Content-Type": "application/json",
      Authorization: `Bearer ${this.apiKey}`,
    };
  }

  private async request<R>(
    path: string,
    options: RequestInit = {}
  ): Promise<R> {
    const res = await fetch(`${this.url}${path}`, {
      ...options,
      headers: { ...this.headers, ...options.headers },
    });

    if (!res.ok) {
      throw await errorFromResponse(res);
    }

    if (res.status === 204) {
      return undefined as R;
    }

    return parseJson<R>(res);
  }

  async get(id: string): Promise<T | null> {
    try {
      return await this.request<T>(`/${id}`);
    } catch (err) {
      if (err instanceof BrickbedError && err.status === 404) {
        return null;
      }
      throw err;
    }
  }

  async insert(data: Omit<T, "_id" | "_createdAt" | "_updatedAt">): Promise<T> {
    return this.request<T>("", {
      method: "POST",
      body: JSON.stringify(data),
    });
  }

  async update(
    id: string,
    data: Omit<T, "_id" | "_createdAt" | "_updatedAt">
  ): Promise<T> {
    return this.request<T>(`/${id}`, {
      method: "PUT",
      body: JSON.stringify(data),
    });
  }

  async patch(
    id: string,
    data: Partial<Omit<T, "_id" | "_createdAt" | "_updatedAt">>
  ): Promise<T> {
    return this.request<T>(`/${id}`, {
      method: "PATCH",
      body: JSON.stringify(data),
    });
  }

  async delete(id: string): Promise<void> {
    await this.request<void>(`/${id}`, {
      method: "DELETE",
    });
  }

  async list(options: ListOptions = {}): Promise<ListResponse<T>> {
    const params = new URLSearchParams();
    if (options.limit !== undefined) {
      params.set("limit", options.limit.toString());
    }
    if (options.cursor !== undefined) {
      params.set("cursor", options.cursor);
    }

    const query = params.toString();
    const path = query ? `?${query}` : "";

    return this.request<ListResponse<T>>(path);
  }

  async query(
    indexName: string,
    params: Record<string, unknown>,
    options: QueryOptions = {}
  ): Promise<T[]> {
    const res = await this.queryPage(indexName, params, options);
    return res.data;
  }

  async queryPage(
    indexName: string,
    params: Record<string, unknown>,
    options: QueryOptions = {}
  ): Promise<QueryResponse<T>> {
    return this.request<QueryResponse<T>>("/_query", {
      method: "POST",
      body: JSON.stringify({
        index: indexName,
        params,
        limit: options.limit,
        cursor: options.cursor,
      }),
    });
  }

  /** Ranked search; results carry `_score` and are ordered best-first. */
  async search(options: SearchOptions): Promise<SearchResult<T>[]> {
    const mode = resolveMode(options);
    checkIndexes(mode, options);

    const res = await this.request<SearchResponse<T>>("/_search", {
      method: "POST",
      body: JSON.stringify({
        query: options.query,
        vector: options.vector,
        index: options.index,
        textIndex: options.textIndex,
        vectorIndex: options.vectorIndex,
        mode,
        limit: options.limit,
        filter: options.filter,
      }),
    });

    return res.data;
  }
}

/** Mirrors the server: explicit `mode` wins, otherwise the inputs decide. */
function resolveMode(options: SearchOptions): SearchMode {
  const hasQuery = options.query !== undefined;
  const hasVector = options.vector !== undefined;

  // No arm to search is invalid under every mode, so it is checked first.
  if (!hasQuery && !hasVector) {
    throw new Error('search requires "query" or "vector"');
  }
  if (options.mode !== undefined) {
    return options.mode;
  }
  if (hasQuery && hasVector) {
    return "hybrid";
  }
  return hasQuery ? "text" : "vector";
}

/**
 * A single-arm search names its index with `index`; hybrid runs two arms, so it
 * takes `textIndex`/`vectorIndex` and rejects the ambiguous `index`. Caught
 * here with the server's own wording, since the server 400s on all of these.
 */
function checkIndexes(mode: SearchMode, options: SearchOptions): void {
  if (mode === "hybrid") {
    if (options.index !== undefined) {
      throw new Error(
        '"index" is ambiguous in hybrid search; use "textIndex" and "vectorIndex"'
      );
    }
    return;
  }

  // Checked in the server's order, so passing both names the same field it would.
  const fields =
    mode === "text"
      ? (["vectorIndex", "textIndex"] as const)
      : (["textIndex", "vectorIndex"] as const);

  for (const field of fields) {
    if (options[field] !== undefined) {
      throw new Error(
        `"${field}" only applies to hybrid search; name the index with "index"`
      );
    }
  }
}
