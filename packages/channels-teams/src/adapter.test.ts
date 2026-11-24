import { afterEach, describe, expect, it, vi } from "vitest";
import { Activity, ActivityTypes } from "@microsoft/agents-activity";
import { CloudAdapter, TurnContext } from "@microsoft/agents-hosting";
import type { InvokeResponse } from "@microsoft/agents-hosting";
import type { IngressSink } from "@darbotlm/channels-core";
import { TeamsAdapter } from "./adapter.js";
import { TeamsConversationStore } from "./conversation-store.js";
import { conversationKeyOf, parseCardAction } from "./interaction.js";
import type { TeamsAdapterOptions } from "./types.js";

class RecordingCloudAdapter extends CloudAdapter {
  readonly sent: Activity[] = [];

  constructor() {
    super({ clientId: "test-app", tenantId: "test-tenant" });
  }

  override async sendActivities(context: TurnContext, activities: Activity[]) {
    this.sent.push(...activities);
    for (const activity of activities) {
      if (activity.type === ActivityTypes.InvokeResponse) {
        // Exercise the real SDK's invoke-response turn-state handling, with no
        // connector client or network access.
        await super.sendActivities(context, [activity]);
      }
    }
    return activities.map(() => ({ id: "outgoing-id" }));
  }

  response(context: TurnContext): InvokeResponse | undefined {
    return this.processTurnResults(context);
  }
}

function activity(
  type: string,
  fields: Record<string, unknown> = {},
): Activity {
  return Activity.fromObject({
    type,
    channelId: "msteams",
    id: "message-1",
    from: { id: "user-1", role: "user" },
    recipient: { id: "bot-1", role: "bot" },
    conversation: { id: "chat-1", tenantId: "tenant-1" },
    serviceUrl: "https://connector.example.invalid",
    ...fields,
  });
}

function sink(): IngressSink {
  return {
    onTurn: vi.fn(),
    onInteraction: vi.fn(),
    onWelcome: vi.fn(),
    onCommand: vi.fn(),
    onThreadStarted: vi.fn(),
    onReaction: vi.fn(),
    onModalSubmit: vi.fn(async () => {}),
    onModalClose: vi.fn(),
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function deferredAttachment() {
  const read = deferred<ArrayBuffer>();
  const started = deferred<void>();
  const response = new Response();
  vi.spyOn(response, "arrayBuffer").mockImplementation(() => {
    started.resolve();
    return read.promise;
  });
  const fetch = vi.spyOn(globalThis, "fetch").mockResolvedValue(response);
  vi.spyOn(console, "log").mockImplementation(() => {});
  return {
    started: started.promise,
    finish: () => read.resolve(new TextEncoder().encode("stale file").buffer),
    fetch,
    attachments: [
      {
        contentType: "text/plain",
        contentUrl: "https://files.example.invalid/file.txt",
        name: "file.txt",
      },
    ],
  };
}

function fixture(opts: TeamsAdapterOptions = { clientId: "" }) {
  const adapter = new TeamsAdapter(opts);
  const cloud = new RecordingCloudAdapter();
  (adapter as unknown as { cloud: CloudAdapter }).cloud = cloud;
  const ingress = sink();
  const dispatch = (incoming: Activity) => {
    const context = new TurnContext(cloud, incoming, { aud: "test-app" });
    const completion = (
      adapter as unknown as {
        handleActivity(context: TurnContext, sink: IngressSink): Promise<void>;
      }
    ).handleActivity(context, ingress);
    return { context, completion };
  };
  return { adapter, cloud, ingress, dispatch };
}

function execute(fields: Record<string, unknown> = {}): Activity {
  return activity(ActivityTypes.Invoke, {
    name: "adaptiveCard/action",
    replyToId: "card-1",
    value: {
      action: {
        type: "Action.Execute",
        data: { ckActionId: "ck:test", value: "yes", input: "answer" },
      },
      authentication: { token: "synthetic-token-not-an-identity" },
    },
    ...fields,
  });
}

afterEach(() => vi.restoreAllMocks());

describe("Teams Activity ingress", () => {
  it.each([200, 500])(
    "writes HTTP %s through real CloudAdapter.process without a connector request",
    async (status) => {
      vi.spyOn(console, "error").mockImplementation(() => {});
      const { adapter, cloud, ingress } = fixture();
      const connections = cloud as unknown as {
        createConnectorClientWithIdentity(): Promise<unknown>;
        createUserTokenClient(): Promise<unknown>;
      };
      vi.spyOn(
        connections,
        "createConnectorClientWithIdentity",
      ).mockResolvedValue({});
      vi.spyOn(connections, "createUserTokenClient").mockResolvedValue({});
      if (status === 500)
        ingress.onInteraction = vi.fn(async () => {
          throw new Error("failed");
        });
      const response = {
        writableEnded: false,
        headersSent: false,
        status: vi.fn().mockReturnThis(),
        setHeader: vi.fn(),
        send: vi.fn(),
        end: vi.fn(),
      };
      await cloud.process(
        {
          headers: {},
          body: execute(),
          user: {
            aud: "test-app",
            serviceurl: "https://connector.example.invalid",
          },
        } as unknown as Parameters<CloudAdapter["process"]>[0],
        response as unknown as Parameters<CloudAdapter["process"]>[1],
        (context) =>
          (
            adapter as unknown as {
              handleActivity(
                context: TurnContext,
                sink: IngressSink,
              ): Promise<void>;
            }
          ).handleActivity(context, ingress),
      );
      expect(response.status).toHaveBeenCalledWith(status);
      expect(response.send).toHaveBeenCalledWith(
        expect.objectContaining({ statusCode: status }),
      );
      expect(response.setHeader).toHaveBeenCalledWith(
        "content-type",
        "application/json",
      );
      expect(ingress.onInteraction).toHaveBeenCalledOnce();
      expect(cloud.sent.map((a) => a.type)).toEqual([
        ActivityTypes.InvokeResponse,
      ]);
    },
  );

  it("awaits Action.Execute and returns the SDK InvokeResponse, never an early message ack", async () => {
    const { cloud, ingress, dispatch } = fixture({
      clientId: "credentialed-app",
    });
    let finish!: () => void;
    ingress.onInteraction = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          finish = resolve;
        }),
    );
    const { context, completion } = dispatch(
      execute({ callerId: "spoofed-caller", futureField: true }),
    );
    expect(ingress.onInteraction).toHaveBeenCalledOnce();
    expect(cloud.sent).toEqual([]);
    const event = vi.mocked(ingress.onInteraction).mock.calls[0]![0];
    expect(event.actor.id).toBe("user-1");
    expect(event.identityContext.tenant.id).toBe("tenant-1");
    expect(event.identityContext.raw).not.toHaveProperty("callerId");
    expect(event.values).toEqual({ input: "answer" });
    expect(event.requireAcknowledgement).toBe(true);
    expect(event.replyTarget).toMatchObject({ context });
    expect(event.messageRef).toMatchObject({ id: "card-1" });
    finish();
    await completion;
    expect(cloud.sent.map((a) => a.type)).toEqual([
      ActivityTypes.InvokeResponse,
    ]);
    expect(cloud.response(context)).toEqual({
      status: 200,
      body: {
        statusCode: 200,
        type: "application/vnd.microsoft.activity.message",
        value: "Action processed.",
      },
    });
    expect(ingress.onTurn).not.toHaveBeenCalled();
  });

  it.each([
    undefined,
    null,
    {},
    { action: null },
    { action: { type: "Action.Submit" } },
    { action: { type: "Action.Execute", data: { ckActionId: "" } } },
  ])("rejects malformed adaptive card values with 400: %j", async (value) => {
    const { cloud, ingress, dispatch } = fixture();
    const { context, completion } = dispatch(execute({ value }));
    await completion;
    expect(cloud.response(context)).toMatchObject({
      status: 400,
      body: { statusCode: 400, type: "application/vnd.microsoft.error" },
    });
    expect(ingress.onInteraction).not.toHaveBeenCalled();
  });

  it("returns 501 for an unsupported invoke name", async () => {
    const { cloud, ingress, dispatch } = fixture();
    const { context, completion } = dispatch(
      execute({ name: "unknown/invoke" }),
    );
    await completion;
    expect(cloud.response(context)?.status).toBe(501);
    expect(ingress.onInteraction).not.toHaveBeenCalled();
  });

  it("returns explicit 500 without leaking thrown details", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { cloud, ingress, dispatch } = fixture();
    ingress.onInteraction = vi.fn(async () => {
      throw new Error("private diagnostic");
    });
    const { context, completion } = dispatch(execute());
    await completion;
    expect(cloud.response(context)).toMatchObject({
      status: 500,
      body: {
        statusCode: 500,
        value: { code: "InternalServerError", message: "Interaction failed." },
      },
    });
    expect(JSON.stringify(cloud.response(context))).not.toContain(
      "private diagnostic",
    );
  });

  it("preserves the ActionExpired 410 invoke response", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { cloud, ingress, dispatch } = fixture();
    ingress.onInteraction = vi.fn(async () => {
      throw Object.assign(new Error("expired"), {
        code: "channel_action_expired",
      });
    });
    const { context, completion } = dispatch(execute());
    await completion;
    expect(cloud.response(context)).toMatchObject({
      status: 410,
      body: { statusCode: 410, value: { code: "ActionExpired" } },
    });
  });

  it("rejects conflicting tenant facts before invoking the sink", async () => {
    const { cloud, ingress, dispatch } = fixture();
    const { context, completion } = dispatch(
      execute({ channelData: { tenant: { id: "other-tenant" } } }),
    );
    await completion;
    expect(cloud.response(context)?.status).toBe(400);
    expect(ingress.onInteraction).not.toHaveBeenCalled();
  });

  it("preserves Action.Submit decoding and scopes all interaction keys by tenant", async () => {
    const { ingress, dispatch } = fixture();
    await dispatch(
      activity(ActivityTypes.Message, {
        value: { ckActionId: "ck:test", value: 3, input: "a" },
      }),
    ).completion;
    expect(ingress.onInteraction).toHaveBeenCalledOnce();
    const event = vi.mocked(ingress.onInteraction).mock.calls[0]![0];
    expect(event.conversationKey).toBe(conversationKeyOf(execute()));
    expect(event.values).toEqual({ input: "a" });
    expect(conversationKeyOf(execute())).not.toBe(
      conversationKeyOf(
        execute({
          conversation: { id: "chat-1", tenantId: "tenant-2" },
        }),
      ),
    );
    expect(
      parseCardAction({
        value: {
          action: { data: { ckActionId: "ck:test", input: 5 } },
          state: "ignored",
        },
      })?.values,
    ).toEqual({ input: 5 });
  });

  it("uses the same fallback tenant in direct decoding and inbound dispatch", async () => {
    const { adapter, ingress, dispatch } = fixture({
      clientId: "",
      tenantId: "configured-tenant",
    });
    const incoming = execute({ conversation: { id: "chat-1" } });
    const decoded = adapter.decodeInteraction(incoming)!;
    await dispatch(incoming).completion;
    const event = vi.mocked(ingress.onInteraction).mock.calls[0]![0];
    expect(decoded.conversationKey).toBe(event.conversationKey);
    expect(decoded.identityContext.tenant.id).toBe("configured-tenant");
    expect(decoded.values).toEqual({ input: "answer" });
    expect(adapter.decodeInteraction(null)).toBeUndefined();
  });

  it("propagates notification sink failures instead of reporting successful processing", async () => {
    const { ingress, dispatch } = fixture();
    ingress.onTurn = vi.fn(async () => {
      throw new Error("sink unavailable");
    });
    await expect(
      dispatch(activity(ActivityTypes.MessageDelete)).completion,
    ).rejects.toThrow("sink unavailable");
    await expect(
      dispatch(
        activity(ActivityTypes.MessageUpdate, {
          id: "another-id",
          text: "edited",
        }),
      ).completion,
    ).rejects.toThrow("sink unavailable");
  });

  it("dispatches edits/deletes and replaces/removes the correct transcript entry", async () => {
    const { adapter, ingress, dispatch } = fixture();
    await dispatch(activity(ActivityTypes.Message, { text: "original" }))
      .completion;
    await dispatch(
      activity(ActivityTypes.Message, { id: "message-2", text: "keep me" }),
    ).completion;
    const edit = activity(ActivityTypes.MessageUpdate, {
      text: "edited",
      timestamp: "2026-01-02T03:04:05Z",
    });
    await dispatch(edit).completion;
    const key = conversationKeyOf(edit);
    expect(await adapter.getMessages({ conversationKey: key })).toEqual([
      { text: "edited", isBot: false },
      { text: "keep me", isBot: false },
    ]);
    expect(vi.mocked(ingress.onTurn).mock.calls[2]![0]).toMatchObject({
      userText: "edited",
      operation: { kind: "updated", logicalMessageId: "message-1" },
    });
    await dispatch(
      activity(ActivityTypes.MessageDelete, { replyToId: "wrong-parent-id" }),
    ).completion;
    expect(await adapter.getMessages({ conversationKey: key })).toEqual([
      { text: "keep me", isBot: false },
    ]);
    expect(vi.mocked(ingress.onTurn).mock.lastCall?.[0]).toMatchObject({
      userText: "",
      operation: { kind: "deleted", logicalMessageId: "message-1" },
    });
  });

  it.each([ActivityTypes.Message, ActivityTypes.MessageUpdate])(
    "never dispatches a stale %s after its attachment read loses to an edit",
    async (type) => {
      const { adapter, ingress, dispatch } = fixture();
      const file = deferredAttachment();
      const original = activity(type, {
        text: "old",
        timestamp: "2026-01-02T03:04:05Z",
        attachments: file.attachments,
      });
      const pending = dispatch(original);
      await file.started;
      const newer = activity(ActivityTypes.MessageUpdate, {
        text: "new",
        timestamp: "2026-01-02T03:04:06Z",
      });
      try {
        await dispatch(newer).completion;
        // Redelivery must not change the transcript, invalidate the edit, or
        // even start another attachment download before core deduplication.
        await dispatch(original).completion;
        expect(file.fetch).toHaveBeenCalledOnce();
        expect(
          await adapter.getMessages({
            conversationKey: conversationKeyOf(original),
          }),
        ).toEqual([{ text: "new", isBot: false }]);
      } finally {
        file.finish();
        await pending.completion;
      }
      await dispatch(original).completion;
      await dispatch(newer).completion;
      expect(ingress.onTurn).toHaveBeenCalledOnce();
      expect(vi.mocked(ingress.onTurn).mock.lastCall?.[0].userText).toBe("new");
      expect(
        await adapter.getMessages({
          conversationKey: conversationKeyOf(original),
        }),
      ).toEqual([{ text: "new", isBot: false }]);
    },
  );

  it.each([ActivityTypes.Message, ActivityTypes.MessageUpdate])(
    "never dispatches a deleted %s after attachment read or undated redelivery",
    async (type) => {
      const { adapter, ingress, dispatch } = fixture();
      const file = deferredAttachment();
      const original = activity(type, {
        text: "old",
        attachments: file.attachments,
      });
      const pending = dispatch(original);
      await file.started;
      const deletion = activity(ActivityTypes.MessageDelete);
      try {
        await dispatch(deletion).completion;
        await dispatch(original).completion;
        await dispatch(
          activity(ActivityTypes.MessageUpdate, { text: "late edit" }),
        ).completion;
        await dispatch(activity(ActivityTypes.Message, { text: "late create" }))
          .completion;
        await dispatch(deletion).completion;
        expect(file.fetch).toHaveBeenCalledOnce();
      } finally {
        file.finish();
        await pending.completion;
      }
      await dispatch(original).completion;
      expect(ingress.onTurn).toHaveBeenCalledOnce();
      expect(vi.mocked(ingress.onTurn).mock.lastCall?.[0].operation?.kind).toBe(
        "deleted",
      );
      expect(
        await adapter.getMessages({
          conversationKey: conversationKeyOf(original),
        }),
      ).toEqual([]);
    },
  );

  it("orders provider timestamps numerically, rejects unseen stale delivery, and retains deletion tombstones", async () => {
    const { adapter, ingress, dispatch } = fixture();
    const edit = activity(ActivityTypes.MessageUpdate, {
      text: "latest",
      timestamp: "2026-01-02T03:04:06Z",
    });
    await dispatch(edit).completion;
    // This offset timestamp sorts later lexically, but is one second older.
    await dispatch(
      activity(ActivityTypes.MessageUpdate, {
        text: "older",
        timestamp: "2026-01-02T04:04:05+01:00",
      }),
    ).completion;
    await dispatch(
      activity(ActivityTypes.MessageDelete, {
        timestamp: "2026-01-02T03:04:05Z",
      }),
    ).completion;
    await dispatch(activity(ActivityTypes.Message, { text: "late original" }))
      .completion;
    expect(ingress.onTurn).toHaveBeenCalledOnce();
    expect(
      await adapter.getMessages({ conversationKey: conversationKeyOf(edit) }),
    ).toEqual([{ text: "latest", isBot: false }]);

    await dispatch(
      activity(ActivityTypes.MessageDelete, {
        timestamp: "2026-01-02T03:04:06Z",
      }),
    ).completion;
    await dispatch(activity(ActivityTypes.MessageUpdate, { text: "undated" }))
      .completion;
    await dispatch(
      activity(ActivityTypes.MessageUpdate, {
        text: "equal time",
        timestamp: "2026-01-02T04:04:06+01:00",
      }),
    ).completion;
    await dispatch(
      activity(ActivityTypes.Message, {
        text: "create cannot restore",
        timestamp: "2026-01-02T03:04:07Z",
      }),
    ).completion;
    expect(ingress.onTurn).toHaveBeenCalledTimes(2);
    expect(
      await adapter.getMessages({ conversationKey: conversationKeyOf(edit) }),
    ).toEqual([]);
    await dispatch(
      activity(ActivityTypes.MessageUpdate, {
        text: "restored",
        timestamp: "2026-01-02T03:04:07Z",
      }),
    ).completion;
    expect(ingress.onTurn).toHaveBeenCalledTimes(3);
    expect(
      await adapter.getMessages({ conversationKey: conversationKeyOf(edit) }),
    ).toEqual([{ text: "restored", isBot: false }]);
  });

  it("does not invalidate an accepted pending edit when the same delivery is retried", async () => {
    const { adapter, ingress, dispatch } = fixture();
    const file = deferredAttachment();
    const edit = activity(ActivityTypes.MessageUpdate, {
      text: "accepted",
      attachments: file.attachments,
    });
    const pending = dispatch(edit);
    await file.started;
    try {
      await dispatch(edit).completion;
      expect(file.fetch).toHaveBeenCalledOnce();
      expect(ingress.onTurn).not.toHaveBeenCalled();
    } finally {
      file.finish();
      await pending.completion;
    }
    expect(ingress.onTurn).toHaveBeenCalledOnce();
    expect(
      await adapter.getMessages({ conversationKey: conversationKeyOf(edit) }),
    ).toEqual([{ text: expect.stringContaining("stale file"), isBot: false }]);
  });

  it("uses arrival order for distinct undated edits and deduplicates old fingerprints", async () => {
    const { adapter, ingress, dispatch } = fixture();
    const first = activity(ActivityTypes.MessageUpdate, { text: "first" });
    const second = activity(ActivityTypes.MessageUpdate, { text: "second" });
    await dispatch(first).completion;
    await dispatch(second).completion;
    await dispatch(first).completion;
    expect(ingress.onTurn).toHaveBeenCalledTimes(2);
    expect(
      await adapter.getMessages({ conversationKey: conversationKeyOf(first) }),
    ).toEqual([{ text: "second", isBot: false }]);
    await dispatch(activity(ActivityTypes.MessageDelete)).completion;
    // No timestamp can establish that an edit is newer than an undated delete.
    await dispatch(
      activity(ActivityTypes.MessageUpdate, {
        text: "cannot prove restore",
        timestamp: "2026-01-02T03:04:07Z",
      }),
    ).completion;
    expect(ingress.onTurn).toHaveBeenCalledTimes(3);
    expect(
      await adapter.getMessages({ conversationKey: conversationKeyOf(first) }),
    ).toEqual([]);
  });

  it("acks a credentialed edit before file/handler completion on the native tracked proactive path", async () => {
    const { adapter, cloud, ingress, dispatch } = fixture({
      clientId: "credentialed-app",
    });
    const file = deferredAttachment();
    const finishHandler = deferred<void>();
    const handlerStarted = deferred<void>();
    const proactive = new TurnContext(cloud, activity(ActivityTypes.Event), {
      aud: "credentialed-app",
    });
    const continuation = vi
      .spyOn(cloud, "continueConversation")
      .mockImplementation(async (_appId, _reference, callback) => {
        await callback(proactive);
      });
    ingress.onTurn = vi.fn(async () => {
      handlerStarted.resolve();
      await finishHandler.promise;
    });
    const edit = activity(ActivityTypes.MessageUpdate, {
      text: "edit",
      timestamp: "2026-01-02T03:04:06Z",
      attachments: file.attachments,
    });
    try {
      await dispatch(edit).completion;
      await file.started;
      expect(ingress.onTurn).not.toHaveBeenCalled();
      expect(continuation).toHaveBeenCalledOnce();
      file.finish();
      await handlerStarted.promise;
      expect(vi.mocked(ingress.onTurn).mock.lastCall?.[0]).toMatchObject({
        userText: "edit",
        actor: { id: "user-1" },
        operation: { kind: "updated", logicalMessageId: "message-1" },
        replyTarget: { context: proactive },
      });
      let stopped = false;
      const stopping = adapter.stop().then(() => {
        stopped = true;
      });
      await Promise.resolve();
      expect(stopped).toBe(false);
      finishHandler.resolve();
      await stopping;
      expect(stopped).toBe(true);
    } finally {
      file.finish();
      finishHandler.resolve();
      await adapter.stop();
    }
  });

  it("keeps a credentialed edit token across proactive context creation and suppresses a deleted pending run", async () => {
    const { adapter, cloud, ingress, dispatch } = fixture({
      clientId: "credentialed-app",
    });
    const ready = deferred<void>();
    const proactive = new TurnContext(cloud, activity(ActivityTypes.Event), {
      aud: "credentialed-app",
    });
    vi.spyOn(cloud, "continueConversation").mockImplementation(
      async (_appId, _reference, callback) => {
        await ready.promise;
        await callback(proactive);
      },
    );
    const edit = activity(ActivityTypes.MessageUpdate, { text: "edit" });
    try {
      await dispatch(edit).completion;
      await dispatch(activity(ActivityTypes.MessageDelete)).completion;
    } finally {
      ready.resolve();
      await adapter.stop();
    }
    expect(ingress.onTurn).toHaveBeenCalledOnce();
    expect(vi.mocked(ingress.onTurn).mock.lastCall?.[0].operation?.kind).toBe(
      "deleted",
    );
    expect(
      await adapter.getMessages({ conversationKey: conversationKeyOf(edit) }),
    ).toEqual([]);
  });

  it.each([ActivityTypes.MessageUpdate, ActivityTypes.MessageDelete])(
    "suppresses a credentialed edit during its attachment read after %s and redelivery",
    async (type) => {
      const { adapter, cloud, ingress, dispatch } = fixture({
        clientId: "credentialed-app",
      });
      const file = deferredAttachment();
      const proactive = new TurnContext(cloud, activity(ActivityTypes.Event), {
        aud: "credentialed-app",
      });
      const continuation = vi
        .spyOn(cloud, "continueConversation")
        .mockImplementation(async (_appId, _reference, callback) => {
          await callback(proactive);
        });
      const edit = activity(ActivityTypes.MessageUpdate, {
        text: "stale edit",
        timestamp: "2026-01-02T03:04:05Z",
        attachments: file.attachments,
      });
      try {
        await dispatch(edit).completion;
        await file.started;
        await dispatch(
          activity(type, {
            text: "replacement",
            timestamp: "2026-01-02T03:04:06Z",
          }),
        ).completion;
        await dispatch(edit).completion;
      } finally {
        file.finish();
        await adapter.stop();
      }
      expect(file.fetch).toHaveBeenCalledOnce();
      expect(continuation).toHaveBeenCalledTimes(
        type === ActivityTypes.MessageUpdate ? 2 : 1,
      );
      expect(ingress.onTurn).toHaveBeenCalledOnce();
      expect(vi.mocked(ingress.onTurn).mock.lastCall?.[0].operation?.kind).toBe(
        type === ActivityTypes.MessageUpdate ? "updated" : "deleted",
      );
      expect(
        await adapter.getMessages({ conversationKey: conversationKeyOf(edit) }),
      ).toEqual(
        type === ActivityTypes.MessageUpdate
          ? [{ text: "replacement", isBot: false }]
          : [],
      );
    },
  );

  it("dispatches additions and removals using the Teams reaction target", async () => {
    const { ingress, dispatch } = fixture();
    await dispatch(
      activity(ActivityTypes.MessageReaction, {
        replyToId: "reacted-message",
        reactionsAdded: [{ type: "like" }],
        reactionsRemoved: [{ type: "heart" }],
      }),
    ).completion;
    expect(ingress.onReaction).toHaveBeenCalledTimes(2);
    expect(
      vi
        .mocked(ingress.onReaction)
        .mock.calls.map(([event]) => [
          event.rawEmoji,
          event.added,
          event.messageId,
        ]),
    ).toEqual([
      ["like", true, "reacted-message"],
      ["heart", false, "reacted-message"],
    ]);
    await dispatch(
      activity(ActivityTypes.MessageReaction, {
        reactionsAdded: [{ type: "like" }],
      }),
    ).completion;
    expect(vi.mocked(ingress.onReaction).mock.lastCall?.[0].messageId).toBe(
      "message-1",
    );
  });

  it("welcomes bot activation, not unrelated members; removal clears only its conversation", async () => {
    const { adapter, ingress, dispatch } = fixture();
    await dispatch(activity(ActivityTypes.Message, { text: "history" }))
      .completion;
    await dispatch(
      activity(ActivityTypes.ConversationUpdate, {
        membersAdded: [{ id: "someone-else" }],
      }),
    ).completion;
    expect(ingress.onWelcome).not.toHaveBeenCalled();
    await dispatch(
      activity(ActivityTypes.ConversationUpdate, {
        membersAdded: [{ id: "bot-1" }],
      }),
    ).completion;
    await dispatch(
      activity(ActivityTypes.InstallationUpdate, { action: "add" }),
    ).completion;
    expect(ingress.onWelcome).toHaveBeenCalledTimes(2);
    await dispatch(
      activity(ActivityTypes.InstallationUpdate, { action: "remove" }),
    ).completion;
    expect(
      await adapter.getMessages({
        conversationKey: conversationKeyOf(execute()),
      }),
    ).toEqual([]);
  });

  it("rejects a valid unsupported Activity command rather than dispatching a slash command", async () => {
    const { cloud, ingress, dispatch } = fixture();
    await dispatch(
      activity(ActivityTypes.Command, {
        name: "application/test",
        value: { commandId: "cmd-1", data: {} },
      }),
    ).completion;
    expect(cloud.sent).toHaveLength(1);
    expect(cloud.sent[0]).toMatchObject({
      type: ActivityTypes.CommandResult,
      name: "application/test",
      replyToId: "message-1",
      value: { commandId: "cmd-1", error: { code: "NotSupported" } },
    });
    expect(ingress.onCommand).not.toHaveBeenCalled();
  });

  it.each(["session.init", "session.update", "session.end"])(
    "explicitly rejects unsupported reserved %s commands",
    async (name) => {
      const { cloud, dispatch } = fixture();
      await dispatch(
        activity(ActivityTypes.Command, {
          name,
          value: { sessionId: "session-1" },
        }),
      ).completion;
      expect(cloud.sent[0]).toMatchObject({
        type: ActivityTypes.CommandResult,
        name,
        replyToId: "message-1",
        value: { status: "error", error: { code: "NotSupported" } },
      });
    },
  );

  it.each([
    activity(ActivityTypes.Typing),
    activity(ActivityTypes.Event, { name: "unknown-event" }),
    activity("future-activity", { futureField: { arbitrary: true } }),
    activity(ActivityTypes.Command, { name: "invalid-name", value: {} }),
    activity(ActivityTypes.Command, { name: "application/test" }),
  ])(
    "intentionally ignores non-applicable events without inventing success: $type",
    async (incoming) => {
      const { cloud, ingress, dispatch } = fixture();
      await dispatch(incoming).completion;
      expect(cloud.sent).toEqual([]);
      for (const callback of Object.values(ingress))
        expect(callback).not.toHaveBeenCalled();
    },
  );

  it("prevents late downloads from undoing edits, deletions or uninstall", () => {
    const store = new TeamsConversationStore();
    const original = store.beginUserRevision("conversation", "id", "old", {
      kind: "created",
      revisionId: "z-random",
    })!;
    const edited = store.beginUserRevision("conversation", "id", "new", {
      kind: "updated",
      revisionId: "a-random",
    })!;
    expect(original.isCurrent()).toBe(false);
    expect(original.commit("stale file")).toBe(false);
    expect(store.getTranscript("conversation")[0]?.text).toBe("new");
    store.deleteUser("conversation", "id", {
      kind: "deleted",
      revisionId: "delete",
    });
    expect(edited.commit("deleted file")).toBe(false);
    expect(store.getTranscript("conversation")).toEqual([]);
    const late = store.beginUserRevision("conversation", "other-id", "again", {
      kind: "created",
      revisionId: "again",
    })!;
    store.clear("conversation");
    expect(late.commit("uninstalled file")).toBe(false);
    expect(store.getTranscript("conversation")).toEqual([]);
  });
});
