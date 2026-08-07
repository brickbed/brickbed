export class BrickbedError extends Error {
  constructor(
    public readonly status: number,
    public readonly body: string,
    detail?: string
  ) {
    super(`Brickbed error (${status}): ${detail ?? body}`);
    this.name = "BrickbedError";
  }
}

/**
 * The server wraps every error in JSON (`{"error": "..."}`), including
 * body-rejection 422s, but the body is still parsed opportunistically so an
 * intermediary answering with plain text (a proxy, say) degrades gracefully.
 */
export async function errorFromResponse(res: Response): Promise<BrickbedError> {
  const body = await res.text().catch(() => "");
  const detail = errorMessage(body) || body || res.statusText || "request failed";
  return new BrickbedError(res.status, body, detail);
}

function errorMessage(body: string): string | undefined {
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return undefined;
  }
  if (typeof parsed !== "object" || parsed === null) {
    return undefined;
  }
  const { error, message } = parsed as { error?: unknown; message?: unknown };
  if (typeof error === "string") {
    return error;
  }
  return typeof message === "string" ? message : undefined;
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
  throw new BrickbedError(res.status, text, "expected a JSON response body");
}
