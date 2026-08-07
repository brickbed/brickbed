export interface Document {
  _id: string;
  _createdAt: number;
  _updatedAt: number;
  [key: string]: unknown;
}

export interface ListOptions {
  limit?: number;
  cursor?: string;
}

export interface ListResponse<T extends Document> {
  data: T[];
  cursor?: string;
}

export interface QueryOptions {
  limit?: number;
  cursor?: string;
}

export interface QueryResponse<T extends Document> {
  data: T[];
  cursor?: string;
}

/** `text` scores BM25 over `query`, `vector` scores similarity to `vector`. */
export type SearchMode = "text" | "vector" | "hybrid";

/** Restrict search to documents matching a regular index lookup. */
export interface SearchFilter {
  index: string;
  params: Record<string, unknown>;
}

export interface SearchOptions {
  /** Text query; required for `text` and `hybrid` modes. */
  query?: string;
  /** Query embedding; required for `vector` and `hybrid` modes. */
  vector?: number[];
  /**
   * Index for a single-arm search, defaulting to the collection's first index
   * of that kind. Ambiguous in `hybrid`, which runs two arms: name those with
   * `textIndex` and `vectorIndex` instead.
   */
  index?: string;
  /** Search index for the text arm of a `hybrid` search. */
  textIndex?: string;
  /** Vector index for the vector arm of a `hybrid` search. */
  vectorIndex?: string;
  /**
   * Defaults to `text` when only `query` is given, `vector` when only
   * `vector`, and `hybrid` when both are.
   */
  mode?: SearchMode;
  limit?: number;
  filter?: SearchFilter;
}

/**
 * A hit with its ranking score. `_score` is only comparable within one
 * response: BM25 in `text`, similarity in `vector`, and a reciprocal-rank
 * fusion score in `hybrid`, which is rank-derived and much smaller.
 */
export type SearchResult<T extends Document> = T & { _score: number };

export interface SearchResponse<T extends Document> {
  data: SearchResult<T>[];
}

/** What an owner rule compares the document field against. */
export type OwnerMatch = "subject" | "tokenIdentifier" | "email";

/** `"public"`, `"authenticated"`, or a document field holding the caller's id. */
export type Rule =
  | "public"
  | "authenticated"
  | { owner: string; match?: OwnerMatch };

/** One rule for every write, or per-operation rules. */
export type WriteRule =
  | Rule
  | { create?: Rule; update?: Rule; delete?: Rule };

export interface CollectionRules {
  read?: Rule;
  write?: WriteRule;
}

export interface AuthProvider {
  issuer: string;
  audience?: string;
  jwksUrl?: string;
  algorithms?: string[];
}

export interface AuthConfig {
  providers: AuthProvider[];
}

/** Structural match for @brickbed/schema's defineSchema output. */
export interface SchemaPayload {
  collections: Record<
    string,
    {
      fields: Record<string, unknown>;
      indexes: { name: string; fields: string[] }[];
      searchIndexes?: { name: string; fields: string[] }[];
      vectorIndexes?: {
        name: string;
        field: string;
        metric: "cosine" | "dot";
        dims: number;
      }[];
      rules?: CollectionRules;
    }
  >;
  auth?: AuthConfig;
}

export interface ClientConfig {
  endpoint: string;
  apiKey: string;
  projectId: string;
}
