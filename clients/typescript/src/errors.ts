export type BrickbedErrorDetails = Record<string, unknown>;

/** Known error codes in Brickbed's v1 HTTP error contract. */
export type KnownBrickbedErrorCode =
  | "invalid_request"
  | "validation_failed"
  | "schema_invalid"
  | "invalid_cursor"
  | "conflict"
  | "idempotency_conflict"
  | "unauthorized"
  | "forbidden"
  | "not_found"
  | "limit_exceeded"
  | "rate_limited"
  | "unavailable"
  | "embedding_provider_error"
  | "internal_error";

/**
 * A string rather than a closed union so a newer server code is preserved for
 * callers instead of being erased or turned into a parsing failure.
 */
export type BrickbedErrorCode = KnownBrickbedErrorCode | (string & {});

export class BrickbedError extends Error {
  constructor(
    public readonly status: number,
    /** Stable machine contract. Branch on this, never on `message`. */
    public readonly code: BrickbedErrorCode,
    /** Human-actionable explanation; wording may change between releases. */
    message: string,
    public readonly details: BrickbedErrorDetails | undefined,
    public readonly requestId: string | undefined,
    /** Raw body retained for proxy/intermediary diagnostics. */
    public readonly body: string
  ) {
    // `message` is the server's human-facing explanation. Keep it unwrapped:
    // consumers display it directly, while `code` is the machine contract.
    super(message);
    this.name = "BrickbedError";
  }

  /** Decorated diagnostics for logs without changing the human message field. */
  override toString(): string {
    return `${this.name} (${this.status}, ${this.code}): ${this.message}`;
  }
}

interface ErrorEnvelope {
  error?: unknown;
  requestId?: unknown;
}

function object(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

/** Parse the v1 envelope while retaining compatibility with old/proxy bodies. */
function parseError(body: string): {
  code: BrickbedErrorCode;
  message?: string;
  details?: BrickbedErrorDetails;
  requestId?: string;
} {
  let parsed: ErrorEnvelope | undefined;
  try {
    parsed = JSON.parse(body) as ErrorEnvelope;
  } catch {
    return { code: "http_error" };
  }

  const error = object(parsed?.error);
  const code = typeof error?.code === "string" ? error.code : "http_error";
  // Accept the pre-v1 `{ error: string }` body for callers talking to an old
  // server, but do not pretend it supplied a stable machine code.
  const legacy = typeof parsed?.error === "string" ? parsed.error : undefined;
  return {
    code,
    message: typeof error?.message === "string" ? error.message : legacy,
    details: object(error?.details),
    requestId: typeof parsed?.requestId === "string" ? parsed.requestId : undefined,
  };
}

function responseRequestId(res: Response, envelope?: string): string | undefined {
  return envelope ?? res.headers.get("x-request-id") ?? undefined;
}

export async function errorFromResponse(res: Response): Promise<BrickbedError> {
  const body = await res.text().catch(() => "");
  const parsed = parseError(body);
  const message = parsed.message || body || res.statusText || "request failed";
  return new BrickbedError(
    res.status,
    parsed.code,
    message,
    parsed.details,
    responseRequestId(res, parsed.requestId),
    body
  );
}

/**
 * Parse a success body. Callers await a document, so a non-JSON or empty
 * payload (a proxy answering for the server, say) is an error rather than an
 * `undefined` that only fails further up the stack. No-content replies are
 * short-circuited before this runs.
 */
export async function parseJson<R>(res: Response): Promise<R> {
  const text = await res.text();
  try {
    if (text) {
      return JSON.parse(text) as R;
    }
  } catch {
    // Falls through to the error below with the body attached.
  }
  throw new BrickbedError(
    res.status,
    "invalid_response",
    "expected a JSON response body",
    undefined,
    res.headers.get("x-request-id") ?? undefined,
    text
  );
}
