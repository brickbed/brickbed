/**
 * End-user access rules and identity providers. These mirror the server's wire
 * format exactly; the server is the enforcement point, this is the vocabulary.
 */

/** What an owner rule compares the document field against. */
export type OwnerMatch = "subject" | "tokenIdentifier" | "email";

/** Matches a document whose `owner` field holds the caller's identity. */
export interface OwnerRule {
  owner: string;
  /** Defaults to `"subject"`, the `sub` claim. */
  match?: OwnerMatch;
}

export type Rule = "public" | "authenticated" | OwnerRule;

/** Per-operation writes. An operation left out is denied. */
export interface PerOpWriteRule {
  create?: Rule;
  update?: Rule;
  delete?: Rule;
}

export type WriteRule = Rule | PerOpWriteRule;

export interface CollectionRules {
  read?: Rule;
  write?: WriteRule;
}

export interface AuthProvider {
  /** Exact-matched against the token's `iss`. */
  issuer: string;
  /** When set, must appear in `aud`. */
  audience?: string;
  /** Defaults to `{issuer}/.well-known/openid-configuration` discovery. */
  jwksUrl?: string;
  /** Defaults to `["RS256"]`; asymmetric algorithms only. */
  algorithms?: string[];
}

export interface AuthConfig {
  providers: AuthProvider[];
}

/**
 * An object carrying `owner` is a rule; any other object is the per-operation
 * write form. This is the same discriminator the server deserializes with.
 */
export function isOwnerRule(rule: WriteRule): rule is OwnerRule {
  return typeof rule === "object" && "owner" in rule;
}

function isPerOp(write: WriteRule): write is PerOpWriteRule {
  return typeof write === "object" && !isOwnerRule(write);
}

/** Every rule a collection declares, across read and all write operations. */
export function ruleSides(rules: CollectionRules): Rule[] {
  const sides: (Rule | undefined)[] = [rules.read];
  if (rules.write !== undefined) {
    sides.push(
      ...(isPerOp(rules.write)
        ? [rules.write.create, rules.write.update, rules.write.delete]
        : [rules.write])
    );
  }
  return sides.filter((rule): rule is Rule => rule !== undefined);
}

/** Fields named by owner rules, which must be client-controlled. */
export function ownerFields(rules: CollectionRules): string[] {
  return ruleSides(rules)
    .filter(isOwnerRule)
    .map((rule) => rule.owner);
}

/** Does any side need a verified end user? `"public"` alone does not. */
export function namesAnIdentityRule(rules: CollectionRules): boolean {
  return ruleSides(rules).some((rule) => rule !== "public");
}

/**
 * Shapes the type system permits but the server's deserializer rejects, using
 * its wording. Both are silently wrong rather than obviously wrong: an empty
 * owner field matches nothing, and an empty write block reads as "allow writes"
 * while denying every one of them.
 */
export function checkRuleShapes(rules: CollectionRules): void {
  if (rules.write !== undefined && isPerOp(rules.write)) {
    const { create, update } = rules.write;
    if (create === undefined && update === undefined && rules.write.delete === undefined) {
      throw new Error("rules: write rule is empty");
    }
  }

  for (const rule of ruleSides(rules)) {
    if (isOwnerRule(rule) && rule.owner === "") {
      throw new Error("rules: owner rule needs a field name");
    }
  }
}
