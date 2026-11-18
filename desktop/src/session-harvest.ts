import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useSyncExternalStore } from "react";
import {
  feedback,
  parseFlashcard,
  type Feedback,
  type Flashcard,
} from "./agent-flashcard";
import { asProblem } from "./Problem";

type Ledger = {
  version: 1;
  decisions: Map<string, Feedback>;
  references: Flashcard[];
};
type Snapshot = Ledger & {
  available: boolean;
  busy: boolean;
  loading: boolean;
  problem: string | null;
};
export type HarvestMutation =
  | { kind: "migrate"; ledger: string }
  | { kind: "toggle-decision"; row: string; field: keyof Feedback }
  | { kind: "promote-draft"; conversation: string; canonicalSession: string }
  | { kind: "keep-reference" | "forget-reference"; card: string };
type HarvestPort = {
  read: (agent: string) => Promise<string | null>;
  mutate: (agent: string, mutation: HarvestMutation) => Promise<string>;
  legacy: (agent: string) => string | null;
};
const changed = "darbot:harvest-changed";
const key = (agent: string) => `darbot:harvest:v1:${JSON.stringify(agent)}`;
export const draftKey = (id: string) => JSON.stringify(["draft", id]);
export const sessionKey = (id: string) => JSON.stringify(["session", id]);
const empty = (): Ledger => ({
  version: 1,
  decisions: new Map(),
  references: [],
});
const MAX_SIZE = 2_000_000;

function readLedger(raw: string | null): Ledger {
  if (raw === null) return empty();
  if (typeof raw !== "string")
    throw new Error("Native memory returned an invalid ledger.");
  if (raw.length > MAX_SIZE)
    throw new Error("Local memory exceeds its size limit.");
  const v = JSON.parse(raw);
  if (
    v?.version !== 1 ||
    Object.keys(v).sort().join() !== "decisions,references,version" ||
    !Array.isArray(v.decisions) ||
    !Array.isArray(v.references) ||
    v.decisions.length > 20000 ||
    v.references.length > 32
  )
    throw new Error("Unsupported local memory. Existing data was kept.");
  const decisions = new Map<string, Feedback>();
  for (const pair of v.decisions) {
    if (
      !Array.isArray(pair) ||
      pair.length !== 2 ||
      typeof pair[0] !== "string" ||
      !pair[0] ||
      decisions.has(pair[0])
    )
      throw new Error("Invalid local decisions.");
    decisions.set(pair[0], feedback(pair[1]));
  }
  return {
    version: 1,
    decisions,
    references: v.references.map((r: unknown) =>
      parseFlashcard(JSON.stringify(r)),
    ),
  };
}

export function createHarvestStore(port: HarvestPort) {
  type Entry = {
    snapshot: Snapshot;
    listeners: Set<() => void>;
    tail: Promise<void>;
    pending: number;
  };
  const entries = new Map<string, Entry>();
  function entry(agent: string): Entry {
    let value = entries.get(agent);
    if (!value) {
      value = {
        snapshot: {
          ...empty(),
          available: false,
          busy: false,
          loading: true,
          problem: null,
        },
        listeners: new Set(),
        tail: Promise.resolve(),
        pending: 0,
      };
      entries.set(agent, value);
    }
    return value;
  }
  function publish(value: Entry, fields: Partial<Snapshot>) {
    value.snapshot = { ...value.snapshot, ...fields };
    for (const notify of value.listeners) notify();
  }
  function release(agent: string, value: Entry) {
    if (!value.listeners.size && !value.pending && entries.get(agent) === value)
      entries.delete(agent);
  }
  async function hydrate(agent: string, value: Entry) {
    let raw = await port.read(agent);
    if (raw === null) {
      const legacy = port.legacy(agent);
      if (legacy !== null) {
        readLedger(legacy);
        // Native migration creates only when absent. The original remains an untouched backup.
        raw = await port.mutate(agent, { kind: "migrate", ledger: legacy });
      }
    }
    const ledger = readLedger(raw);
    publish(value, {
      ...ledger,
      available: true,
      loading: false,
      problem: null,
    });
  }
  function run(
    agent: string,
    operation: (value: Entry) => Promise<void>,
    reading: boolean,
  ) {
    const value = entry(agent);
    value.pending++;
    publish(value, { busy: true });
    const result = value.tail
      .then(() => operation(value))
      .catch((error: unknown) => {
        const message = asProblem(error).said;
        publish(value, {
          available: reading ? false : value.snapshot.available,
          loading: false,
          problem: message,
        });
        throw new Error(message);
      })
      .finally(() => {
        value.pending--;
        publish(value, { busy: value.pending > 0 });
        release(agent, value);
      });
    // A failed operation is reported above but must not poison later attempts.
    value.tail = result.then(
      () => {},
      () => {},
    );
    return result;
  }
  const refresh = (agent: string) =>
    run(agent, (value) => hydrate(agent, value), true);
  return {
    snapshot: (agent: string) => entry(agent).snapshot,
    subscribe(agent: string, notify: () => void) {
      const value = entry(agent);
      value.listeners.add(notify);
      // Refresh failures are exposed by the snapshot, including during initial hydration.
      void refresh(agent).catch(() => {});
      return () => {
        value.listeners.delete(notify);
        release(agent, value);
      };
    },
    refresh,
    reportError(agent: string, error: unknown) {
      publish(entry(agent), { problem: asProblem(error).said });
    },
    mutate(
      agent: string,
      mutation: Exclude<HarvestMutation, { kind: "migrate" }>,
    ) {
      return run(
        agent,
        async (value) => {
          if (!value.snapshot.available) await hydrate(agent, value);
          const raw = await port.mutate(agent, mutation);
          publish(value, {
            ...readLedger(raw),
            available: true,
            loading: false,
            problem: null,
          });
        },
        false,
      );
    },
  };
}

const store = createHarvestStore({
  read: (agentKey) =>
    invoke<string | null>("harvest_ledger_read", { agentKey }),
  mutate: (agentKey, mutation) =>
    invoke<string>("harvest_ledger_mutate", { agentKey, mutation }),
  legacy: (agent) => window.localStorage.getItem(key(agent)),
});

export function toggleDecision(
  agent: string,
  row: string,
  field: keyof Feedback,
) {
  return store.mutate(agent, { kind: "toggle-decision", row, field });
}
export function promoteDraft(
  agent: string,
  conversation: string,
  canonicalSession: string,
) {
  return store.mutate(agent, {
    kind: "promote-draft",
    conversation,
    canonicalSession,
  });
}
export async function keepReference(agent: string, text: string) {
  const card = JSON.stringify(parseFlashcard(text));
  await store.mutate(agent, { kind: "keep-reference", card });
}
export async function forgetReference(agent: string, card: Flashcard) {
  const raw = JSON.stringify(parseFlashcard(JSON.stringify(card)));
  await store.mutate(agent, { kind: "forget-reference", card: raw });
}
export function useHarvest(agent: string) {
  const subscribe = useCallback(
    (notify: () => void) => store.subscribe(agent, notify),
    [agent],
  );
  const snapshot = useCallback(() => store.snapshot(agent), [agent]);
  const value = useSyncExternalStore(subscribe, snapshot);
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    const refresh = () => {
      void store.refresh(agent).catch(() => {});
    };
    const storage = (event: StorageEvent) => {
      if (event.key === null || event.key === key(agent)) refresh();
    };
    void listen<string>(changed, (event) => {
      if (active && event.payload === agent) refresh();
    })
      .then((dispose) => {
        if (active) unlisten = dispose;
        else dispose();
      })
      .catch((error: unknown) => {
        if (active) store.reportError(agent, error);
      });
    window.addEventListener("focus", refresh);
    window.addEventListener("storage", storage);
    return () => {
      active = false;
      unlisten?.();
      window.removeEventListener("focus", refresh);
      window.removeEventListener("storage", storage);
    };
  }, [agent]);
  return value;
}
