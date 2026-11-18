import type {
  CopilotHistory,
  CopilotHistorySession,
  CopilotWorkspaceChat,
} from "./copilot-types";
import {
  conversationFromHistory,
  sessionScope,
} from "./copilot-workspace-store";
import { isStudioAgentId } from "./copilot-types";

export const CHAT_PREVIEW_LIMIT = 5;

export function conversationHistoryRows(
  chats: readonly CopilotWorkspaceChat[],
  history: readonly CopilotHistorySession[],
): CopilotWorkspaceChat[] {
  const bySession = new Map(
    chats
      .filter((chat) => chat.sessionId !== null)
      .map((chat) => [sessionScope(chat), chat]),
  );
  const local = [
    ...new Map(
      history
        .filter((chat) => !isStudioAgentId(chat.agentId))
        .map((chat) => [sessionScope(chat), chat]),
    ).values(),
  ];
  const recorded = new Set(local.map(sessionScope));
  return [
    ...chats.filter(
      (chat) => chat.sessionId === null || !recorded.has(sessionScope(chat)),
    ),
    ...local.map((chat) => {
      const previous = bySession.get(sessionScope(chat));
      return {
        ...conversationFromHistory(chat),
        conversationId:
          previous?.conversationId ?? `record:${sessionScope(chat)}`,
        draft: previous?.draft ?? "",
      };
    }),
  ];
}

export function mergeHistoryBatch(
  current: CopilotHistory | null,
  batch: readonly CopilotHistorySession[],
): CopilotHistory {
  const sessions = new Map(
    (current?.sessions ?? []).map((session) => [
      JSON.stringify([session.cwd, session.sessionId]),
      session,
    ]),
  );
  for (const session of batch)
    sessions.set(JSON.stringify([session.cwd, session.sessionId]), session);
  const agents = new Map<string, CopilotHistory["agents"][number]>();
  for (const session of sessions.values()) {
    const agent = agents.get(session.agentId);
    if (agent) {
      agent.conversationCount += 1;
    } else {
      agents.set(session.agentId, {
        id: session.agentId,
        name: session.agentName,
        conversationCount: 1,
      });
    }
  }
  return {
    sessions: [...sessions.values()],
    agents: [...agents.values()].sort((left, right) => {
      if (!left.id) return right.id ? -1 : 0;
      if (!right.id) return 1;
      return left.name.localeCompare(right.name);
    }),
    warnings: current?.warnings ?? [],
  };
}

export function mergeLinkedConversations(
  current: readonly CopilotWorkspaceChat[],
  history: readonly CopilotHistorySession[],
  importedAgentIds: readonly string[],
  activeConversationId: string | null,
): CopilotWorkspaceChat[] {
  const allowed = new Set([
    "",
    ...importedAgentIds.filter((id) => !isStudioAgentId(id)),
  ]);
  const chats = new Map(current.map((chat) => [chat.conversationId, chat]));
  const bySession = new Map(
    current
      .filter((chat) => chat.sessionId !== null)
      .map((chat) => [sessionScope(chat), chat]),
  );
  const active = current.find(
    (chat) => chat.conversationId === activeConversationId,
  );
  for (const item of history) {
    if (item.agentId === "__unavailable__" || !allowed.has(item.agentId)) {
      continue;
    }
    if (active?.sessionId === item.sessionId && active.cwd === item.cwd)
      continue;
    const key = sessionScope(item);
    const previous = bySession.get(key);
    if (previous?.conversationId === activeConversationId) continue;
    const conversationId = previous?.conversationId ?? crypto.randomUUID();
    const linked = {
      ...conversationFromHistory(item),
      conversationId,
      draft: previous?.draft ?? "",
      title: item.title?.trim() || previous?.title || "Untitled conversation",
      updatedAt: item.updatedAt ?? previous?.updatedAt ?? null,
    };
    chats.set(conversationId, linked);
    bySession.set(key, linked);
  }
  return [...chats.values()].sort((left, right) => {
    if (left.conversationId === right.conversationId) return 0;
    if (left.conversationId === activeConversationId) return -1;
    if (right.conversationId === activeConversationId) return 1;
    return (right.updatedAt ?? "").localeCompare(left.updatedAt ?? "");
  });
}

export function groupWorkspaceChats(
  chats: readonly CopilotWorkspaceChat[],
): Map<string, CopilotWorkspaceChat[]> {
  const grouped = new Map<string, CopilotWorkspaceChat[]>();
  for (const chat of chats) {
    const group = grouped.get(chat.agentId);
    if (group) group.push(chat);
    else grouped.set(chat.agentId, [chat]);
  }
  return grouped;
}

export function previewAgentChats(
  chats: readonly CopilotWorkspaceChat[],
  activeConversationId: string | null,
): CopilotWorkspaceChat[] {
  const preview = chats.slice(0, CHAT_PREVIEW_LIMIT);
  const active = chats.find(
    (chat) => chat.conversationId === activeConversationId,
  );
  if (active && !preview.includes(active)) {
    return [active, ...preview.slice(0, CHAT_PREVIEW_LIMIT - 1)];
  }
  return preview;
}
