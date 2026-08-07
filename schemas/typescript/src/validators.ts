/** Embedding vector field; `dims` is the fixed length of every stored vector. */
export interface VectorValidator {
  type: "vector";
  dims: number;
  /** Fields the server embeds to produce this vector (embed-on-write). */
  from?: string[];
  /** Embedding model used for embed-on-write. */
  model?: string;
}

export type ValidatorType =
  | { type: "string" }
  | { type: "number" }
  | { type: "boolean" }
  | { type: "array"; items: ValidatorType }
  | { type: "object"; properties: Record<string, ValidatorType> }
  | { type: "optional"; inner: ValidatorType }
  | { type: "id"; collection: string }
  | { type: "union"; variants: ValidatorType[] }
  | { type: "literal"; value: string | number | boolean }
  | VectorValidator;

export interface VectorOptions {
  from?: string[];
  model?: string;
}

export const v = {
  string(): ValidatorType {
    return { type: "string" };
  },

  number(): ValidatorType {
    return { type: "number" };
  },

  boolean(): ValidatorType {
    return { type: "boolean" };
  },

  array(items: ValidatorType): ValidatorType {
    return { type: "array", items };
  },

  object(properties: Record<string, ValidatorType>): ValidatorType {
    return { type: "object", properties };
  },

  optional(inner: ValidatorType): ValidatorType {
    return { type: "optional", inner };
  },

  id(collection: string): ValidatorType {
    return { type: "id", collection };
  },

  union(...variants: ValidatorType[]): ValidatorType {
    return { type: "union", variants };
  },

  literal(value: string | number | boolean): ValidatorType {
    return { type: "literal", value };
  },

  vector(dims: number, opts: VectorOptions = {}): ValidatorType {
    if (!Number.isInteger(dims) || dims <= 0) {
      throw new Error(`v.vector: dims must be a positive integer, got ${dims}`);
    }
    const validator: VectorValidator = { type: "vector", dims };
    if (opts.from !== undefined) {
      validator.from = opts.from;
    }
    if (opts.model !== undefined) {
      validator.model = opts.model;
    }
    return validator;
  },
};
