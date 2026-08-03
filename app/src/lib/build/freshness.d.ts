export interface FreshnessInput {
  embeddedStamp: string | null | undefined;
  headSha: string | null | undefined;
  exeExists: boolean;
}

export interface FreshnessResult {
  fresh: boolean;
  code: "FRESH" | "MISSING" | "NO_GIT" | "UNKNOWN" | "STALE";
  dirty: boolean;
  reason?: string;
}

export function classifyFreshness(input: FreshnessInput): FreshnessResult;
