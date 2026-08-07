import { BrickbedClient } from "./client.js";
import type { ClientConfig } from "./types.js";

export function createClient(config: ClientConfig): BrickbedClient {
  return new BrickbedClient(config);
}

export { BrickbedClient } from "./client.js";
export { Collection } from "./collection.js";
export { BrickbedError } from "./errors.js";
export type {
  Document,
  ListOptions,
  ListResponse,
  QueryOptions,
  QueryResponse,
  SearchFilter,
  SearchMode,
  SearchOptions,
  SearchResponse,
  SearchResult,
  SchemaPayload,
  AuthConfig,
  AuthProvider,
  CollectionRules,
  OwnerMatch,
  Rule,
  WriteRule,
  ClientConfig,
} from "./types.js";
