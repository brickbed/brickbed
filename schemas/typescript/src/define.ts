import type { AuthConfig, CollectionRules } from "./rules.js";
import { checkRuleShapes, namesAnIdentityRule, ownerFields } from "./rules.js";
import type { ValidatorType, VectorValidator } from "./validators.js";

export interface IndexDefinition {
  name: string;
  fields: string[];
}

export interface SearchIndexDefinition {
  name: string;
  fields: string[];
}

export type VectorMetric = "cosine" | "dot";

export interface VectorIndexDefinition {
  name: string;
  field: string;
  metric: VectorMetric;
  dims: number;
}

export interface SearchIndexOptions {
  /** Document fields whose text is indexed for BM25 scoring. */
  fields: string[];
}

export interface VectorIndexOptions {
  /** Name of a `v.vector()` field on this collection; supplies the dimension. */
  field: string;
  metric?: VectorMetric;
}

export interface CollectionDefinition {
  fields: Record<string, ValidatorType>;
  indexes: IndexDefinition[];
  searchIndexes: SearchIndexDefinition[];
  vectorIndexes: VectorIndexDefinition[];
  /** Omitted entirely when the collection declares none, as the server expects. */
  rules?: CollectionRules;
}

export interface CollectionBuilder {
  _fields: Record<string, ValidatorType>;
  _indexes: IndexDefinition[];
  _searchIndexes: SearchIndexDefinition[];
  _vectorIndexes: VectorIndexDefinition[];
  _rules?: CollectionRules;
  index(name: string, fields: string[]): CollectionBuilder;
  searchIndex(name: string, options: SearchIndexOptions): CollectionBuilder;
  vectorIndex(name: string, options: VectorIndexOptions): CollectionBuilder;
  rules(rules: CollectionRules): CollectionBuilder;
}

/**
 * Resolve the vector a vector index points at. One level of `v.optional()` is
 * unwrapped, matching how the server resolves the declared width.
 */
function vectorField(
  fields: Record<string, ValidatorType>,
  indexName: string,
  field: string
): VectorValidator {
  let validator: ValidatorType | undefined = fields[field];
  if (validator !== undefined && validator.type === "optional") {
    validator = validator.inner;
  }

  if (validator === undefined) {
    throw new Error(
      `vectorIndex "${indexName}": no field "${field}" on this collection`
    );
  }
  if (validator.type !== "vector") {
    throw new Error(
      `vectorIndex "${indexName}": field "${field}" is ${validator.type}, expected v.vector()`
    );
  }
  return validator;
}

export function defineCollection(
  fields: Record<string, ValidatorType>
): CollectionBuilder {
  const builder: CollectionBuilder = {
    _fields: fields,
    _indexes: [],
    _searchIndexes: [],
    _vectorIndexes: [],
    index(name: string, fieldList: string[]): CollectionBuilder {
      this._indexes.push({ name, fields: fieldList });
      return this;
    },
    searchIndex(name: string, options: SearchIndexOptions): CollectionBuilder {
      if (options.fields.length === 0) {
        throw new Error(`searchIndex "${name}": needs at least one field`);
      }
      this._searchIndexes.push({ name, fields: options.fields });
      return this;
    },
    vectorIndex(name: string, options: VectorIndexOptions): CollectionBuilder {
      const { dims } = vectorField(this._fields, name, options.field);
      this._vectorIndexes.push({
        name,
        field: options.field,
        metric: options.metric ?? "cosine",
        dims,
      });
      return this;
    },
    rules(rules: CollectionRules): CollectionBuilder {
      checkRuleShapes(rules);
      for (const field of ownerFields(rules)) {
        if (isVectorField(this._fields, field)) {
          throw new Error(
            `rules: owner rule cannot match on "${field}", which the server fills during the write`
          );
        }
      }
      this._rules = rules;
      return this;
    },
  };
  return builder;
}

/** Same one-level `v.optional()` unwrap the server applies. */
function isVectorField(
  fields: Record<string, ValidatorType>,
  field: string
): boolean {
  let validator: ValidatorType | undefined = fields[field];
  if (validator !== undefined && validator.type === "optional") {
    validator = validator.inner;
  }
  return validator?.type === "vector";
}

export type SchemaDefinition = Record<string, CollectionBuilder>;

export interface SchemaOptions {
  /** Identity providers whose JWTs this project accepts. */
  auth?: AuthConfig;
}

export interface Schema {
  collections: Record<string, CollectionDefinition>;
  /** Omitted entirely when no providers are declared, as the server expects. */
  auth?: AuthConfig;
}

/** Index names share a keyspace per kind on the server, which rejects dupes. */
function checkDuplicates(
  collection: string,
  kind: string,
  names: string[]
): void {
  const seen = new Set<string>();
  for (const name of names) {
    if (seen.has(name)) {
      throw new Error(`duplicate ${kind} "${name}" on collection "${collection}"`);
    }
    seen.add(name);
  }
}

/**
 * Structural checks the server also makes, caught here so a push does not have
 * to round-trip. The algorithm allowlist and the https-URL rule are left to the
 * server: they are policy that can move, and a stale copy here would reject
 * configuration the server accepts.
 */
function checkAuth(auth: AuthConfig): void {
  if (auth.providers.length === 0) {
    throw new Error("auth: providers must not be empty");
  }

  const seen = new Set<string>();
  for (const provider of auth.providers) {
    if (seen.has(provider.issuer)) {
      throw new Error(`auth: duplicate issuer "${provider.issuer}"`);
    }
    seen.add(provider.issuer);

    if (provider.audience !== undefined && provider.audience === "") {
      throw new Error(`auth: audience for "${provider.issuer}" must not be empty`);
    }
    if (provider.algorithms !== undefined && provider.algorithms.length === 0) {
      throw new Error(`auth: provider "${provider.issuer}" declares no algorithms`);
    }
  }
}

export function defineSchema(
  collections: SchemaDefinition,
  options: SchemaOptions = {}
): Schema {
  const result: Record<string, CollectionDefinition> = {};

  if (options.auth !== undefined) {
    checkAuth(options.auth);
  }

  for (const [name, builder] of Object.entries(collections)) {
    checkDuplicates(name, "index", builder._indexes.map((i) => i.name));
    checkDuplicates(
      name,
      "search index",
      builder._searchIndexes.map((i) => i.name)
    );
    checkDuplicates(
      name,
      "vector index",
      builder._vectorIndexes.map((i) => i.name)
    );

    // Rules that need a verified end user are meaningless without a provider to
    // verify against, and the server rejects the push outright.
    if (
      builder._rules !== undefined &&
      options.auth === undefined &&
      namesAnIdentityRule(builder._rules)
    ) {
      throw new Error(
        `collection "${name}" has rules needing an end-user identity, but the schema declares no auth providers`
      );
    }

    result[name] = {
      fields: builder._fields,
      indexes: builder._indexes,
      searchIndexes: builder._searchIndexes,
      vectorIndexes: builder._vectorIndexes,
      ...(builder._rules !== undefined ? { rules: builder._rules } : {}),
    };
  }

  return {
    collections: result,
    ...(options.auth !== undefined ? { auth: options.auth } : {}),
  };
}
