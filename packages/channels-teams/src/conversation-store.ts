import { randomUUID } from "node:crypto";
import type { AbstractAgent } from "@ag-ui/client";
import type {
  ConversationStore,
  AgentSession,
  ReplyTarget,
} from "@darbotlm/channels-core";
import type { ThreadMessage, AgentContentPart } from "@darbotlm/channels-ui";

interface StoredMessage {
  id: string;
  role: "user" | "assistant";
  /**
   * Plain text, or AG-UI multimodal content parts when the user's message
   * carried files (e.g. an uploaded CSV). AG-UI types `content` as `string`,
   * but multimodal works at runtime by setting it to an `AgentContentPart[]`;
   * we keep the parts in the transcript so a follow-up turn ("now make it a bar
   * chart") still sees the data, then cast when seeding the agent.
   */
  content: string | AgentContentPart[];
}

export interface TeamsUserRevision {
  kind: "created" | "updated" | "deleted";
  revisionId: string;
  timestamp?: number;
}

interface RevisionState {
  token: symbol;
  deleted: boolean;
  timestamp?: number;
  seen: Set<string>;
}

export interface TeamsUserRevisionToken {
  isCurrent(): boolean;
  commit(content: string | AgentContentPart[]): boolean;
}

/** Collapse stored content (text or multimodal parts) to a display string. */
function contentToText(content: string | AgentContentPart[]): string {
  if (typeof content === "string") return content;
  return content
    .map((p) =>
      p.type === "text"
        ? p.text
        : `[${p.type} attachment: ${p.source.mimeType}]`,
    )
    .join("\n");
}

/**
 * In-memory Teams conversation store.
 *
 * Unlike Slack, Teams does not hand the bot a free, queryable history of a
 * conversation, so the adapter keeps the transcript itself: the listener
 * records each incoming user message, and the run renderer records the agent's
 * reply. Each turn builds a fresh AG-UI thread id (the durable history is held
 * here, not server-side) and seeds the agent with the accumulated transcript.
 *
 * The transcript retains a message's multimodal content (an uploaded file's
 * decoded contents), not just its text, so a follow-up turn can still act on
 * data the user only sent once. This grows context for long file-bearing
 * conversations; a production {@link ConversationStore} can cap or summarize it.
 *
 * This is the batteries-included default. It is deliberately swappable: a
 * production deployment that needs the transcript to survive restarts can
 * implement {@link ConversationStore} against a real datastore and pass it in.
 */
export class TeamsConversationStore implements ConversationStore {
  readonly seedsInboundTurn = true;
  private readonly history = new Map<string, StoredMessage[]>();
  private readonly revisions = new Map<string, Map<string, RevisionState>>();

  /**
   * Append a user message to the conversation transcript. Accepts plain text or
   * multimodal content parts (uploaded files); empty content is ignored.
   */
  recordUser(
    conversationKey: string,
    content: string | AgentContentPart[],
    messageId?: string,
  ): void {
    if (typeof content === "string" ? !content : content.length === 0) return;
    const existing = messageId
      ? this.history
          .get(conversationKey)
          ?.find((m) => m.id === messageId && m.role === "user")
      : undefined;
    if (existing) existing.content = content;
    else
      this.append(conversationKey, {
        id: messageId ?? randomUUID(),
        role: "user",
        content,
      });
  }

  /**
   * Record before asynchronous file resolution. A late download must not
   * resurrect a deleted message or replace a newer edit.
   */
  beginUserRevision(
    conversationKey: string,
    messageId: string,
    content: string,
    revision: TeamsUserRevision,
  ): TeamsUserRevisionToken | undefined {
    const state = this.acceptRevision(conversationKey, messageId, revision);
    if (!state) return undefined;
    const isCurrent = () =>
      this.revisions.get(conversationKey)?.get(messageId)?.token === state.token;
    this.updateUser(conversationKey, messageId, content);
    return {
      isCurrent,
      commit: (resolvedContent) => {
        if (!isCurrent()) return false;
        this.updateUser(conversationKey, messageId, resolvedContent);
        return true;
      },
    };
  }

  /**
   * Provider timestamps order revisions; opaque IDs only deduplicate them.
   * Unordered distinct edits use arrival order while live, retaining the known
   * timestamp high-water mark. Creates never overwrite an existing logical ID.
   * Deletes win unordered/equal-time races. A tombstone can only be superseded
   * by an edit strictly newer than its known timestamp; an undated delete is
   * terminal until clear(). These rules cannot infer missing provider order.
   */
  private acceptRevision(
    conversationKey: string,
    messageId: string,
    revision: TeamsUserRevision,
  ): RevisionState | undefined {
    let revisions = this.revisions.get(conversationKey);
    if (!revisions) {
      revisions = new Map();
      this.revisions.set(conversationKey, revisions);
    }
    const previous = revisions.get(messageId);
    const timestamp = Number.isFinite(revision.timestamp)
      ? revision.timestamp
      : undefined;
    if (previous) {
      if (previous.seen.has(revision.revisionId)) return undefined;
      previous.seen.add(revision.revisionId);
      if (revision.kind === "created") return undefined;
      if (
        previous.deleted &&
        (timestamp === undefined ||
          previous.timestamp === undefined ||
          timestamp <= previous.timestamp)
      ) {
        return undefined;
      }
      if (
        timestamp !== undefined &&
        previous.timestamp !== undefined &&
        (timestamp < previous.timestamp ||
          (timestamp === previous.timestamp && revision.kind !== "deleted"))
      ) {
        return undefined;
      }
    }
    const state: RevisionState = {
      token: Symbol(),
      deleted: revision.kind === "deleted",
      timestamp:
        revision.kind === "deleted"
          ? timestamp
          : timestamp ?? previous?.timestamp,
      seen: previous?.seen ?? new Set([revision.revisionId]),
    };
    revisions.set(messageId, state);
    return state;
  }

  /** Apply a complete provider revision without adding a duplicate transcript entry. */
  updateUser(
    conversationKey: string,
    messageId: string,
    content: string | AgentContentPart[],
  ): void {
    const existing = this.history
      .get(conversationKey)
      ?.find((m) => m.id === messageId && m.role === "user");
    if (existing) existing.content = content;
    else this.recordUser(conversationKey, content, messageId);
  }

  deleteUser(
    conversationKey: string,
    messageId: string,
    revision: TeamsUserRevision,
  ): boolean {
    if (!this.acceptRevision(conversationKey, messageId, revision)) return false;
    const history = this.history.get(conversationKey);
    if (history)
      this.history.set(
        conversationKey,
        history.filter((m) => m.role !== "user" || m.id !== messageId),
      );
    return true;
  }

  clear(conversationKey: string): void {
    this.history.delete(conversationKey);
    this.revisions.delete(conversationKey);
  }

  /** Append an assistant message to the conversation transcript. */
  recordAssistant(conversationKey: string, content: string): void {
    if (!content) return;
    this.append(conversationKey, {
      id: randomUUID(),
      role: "assistant",
      content,
    });
  }

  /** The accumulated transcript as bot-ui `ThreadMessage`s (backs `thread.getMessages()`). */
  getTranscript(conversationKey: string): ThreadMessage[] {
    const transcript = this.history.get(conversationKey) ?? [];
    return transcript.map((m) => ({
      text: contentToText(m.content),
      isBot: m.role === "assistant",
    }));
  }

  private append(conversationKey: string, message: StoredMessage): void {
    const existing = this.history.get(conversationKey);
    if (existing) existing.push(message);
    else this.history.set(conversationKey, [message]);
  }

  async getOrCreate(
    conversationKey: string,
    _replyTarget: ReplyTarget,
    makeAgent: (threadId: string) => AbstractAgent,
  ): Promise<AgentSession> {
    // Fresh AG-UI thread per turn. Our `history` map is the durable record, so
    // the server-side thread only needs to live for this turn (mirrors the
    // Slack adapter's rationale for not reusing a stable thread id).
    const threadId = `teams-${conversationKey}-${randomUUID()}`;
    const agent = makeAgent(threadId);
    const transcript = this.history.get(conversationKey) ?? [];
    (agent as unknown as { messages: StoredMessage[] }).messages = [
      ...transcript,
    ];
    return { agent };
  }
}
