
import { normalizeError, type Severity } from "../errors/normalizeError";

export interface ErrorEntry {
  id: number;
  message: string;
  severity: Severity;
  count: number;
}

const MAX_ERRORS = 10;

let nextId = 0;

let entries = $state<ErrorEntry[]>([]);

export const errorStore = {

  get errors(): ErrorEntry[] {
    return entries;
  },


  reportError(input: unknown): void {
    const { message, severity } = normalizeError(input);
    const idx = entries.findIndex((e) => e.message === message);
    if (idx !== -1) {
      entries = entries.map((e, i) =>
        i === idx ? { ...e, count: e.count + 1 } : e,
      );
      return;
    }
    const entry: ErrorEntry = { id: nextId++, message, severity, count: 1 };
    entries = [...entries, entry];
    if (entries.length > MAX_ERRORS) {
      entries = entries.slice(1);
    }
  },


  dismiss(id: number): void {
    entries = entries.filter((e) => e.id !== id);
  },


  dismissAll(): void {
    entries = [];
  },
};
