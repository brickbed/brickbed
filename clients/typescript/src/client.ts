import { Collection } from "./collection.js";
import { errorFromResponse } from "./errors.js";
import type { ClientConfig, Document, SchemaPayload } from "./types.js";

export class BrickbedClient {
  private readonly endpoint: string;
  private readonly apiKey: string;
  private readonly projectId: string;

  constructor(config: ClientConfig) {
    if (!config.projectId) {
      throw new Error("BrickbedClient requires projectId");
    }
    this.endpoint = config.endpoint.replace(/\/$/, "");
    this.apiKey = config.apiKey;
    this.projectId = config.projectId;
  }

  collection<T extends Document = Document>(name: string): Collection<T> {
    return new Collection<T>(this.endpoint, this.projectId, name, this.apiKey);
  }

  /** Push the project schema (defineSchema output); enables validation + indexes. */
  async pushSchema(schema: SchemaPayload): Promise<void> {
    const res = await fetch(
      `${this.endpoint}/v1/${this.projectId}/_schema`,
      {
        method: "PUT",
        headers: {
          "Content-Type": "application/json",
          Authorization: `Bearer ${this.apiKey}`,
        },
        body: JSON.stringify(schema),
      }
    );
    if (!res.ok) {
      throw await errorFromResponse(res);
    }
  }
}
