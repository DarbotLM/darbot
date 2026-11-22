import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  forwardRef, useEffect, useImperativeHandle, useLayoutEffect, useRef, useState, type ReactNode,
} from "react";

export type ContentOwner = { conversationId: string; agentId: string };
export type DarbotAttachment = {
  id: string;
  name: string;
  mimeType: string;
  bytes: number;
  provenance: string;
  preview: string;
};
export type ContentSnapshot = { attachments: DarbotAttachment[]; leaseId: string | null };
export type ComposerContentHandle = {
  freeze: () => Promise<DarbotAttachment[]>;
  sent: () => void;
};
type Source = { id: string; name: string };
type Props = {
  conversationId: string | null;
  agentId: string;
  agentName: string;
  disabled: boolean;
  visible?: boolean;
  ensureConversation: () => void;
  onState: (count: number, protectedContent: boolean, pending: boolean) => void;
  editor: ReactNode;
  controls: ReactNode;
  primaryAction: ReactNode;
};
const empty = (): ContentSnapshot => ({ attachments: [], leaseId: null });
function problem(error: unknown): string {
  if (error && typeof error === "object" && "said" in error && typeof error.said === "string") return error.said;
  return error instanceof Error ? error.message : "The attachment operation failed. Your draft was kept.";
}

/** Binary content is memory-only. Neither this component nor the workspace ledger persists it. */
export const CopilotComposer = forwardRef<ComposerContentHandle, Props>(function CopilotComposer(
  { conversationId, agentId, agentName, disabled, visible = true, ensureConversation, onState,
    editor, controls, primaryAction }, ref,
) {
  const [snapshot, setSnapshot] = useState<ContentSnapshot>(empty);
  const [menu, setMenu] = useState(false);
  const [context, setContext] = useState(false);
  const [selectedText, setSelectedText] = useState("");
  const [sources, setSources] = useState<Source[] | null>(null);
  const [sourceId, setSourceId] = useState("");
  const [share, setShare] = useState(false);
  const [pending, setPending] = useState(false);
  const [ready, setReady] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const generation = useRef(0);
  const inFlight = useRef(false);
  const snapshotRef = useRef(snapshot);
  snapshotRef.current = snapshot;
  const owner: ContentOwner | null = conversationId ? { conversationId, agentId } : null;
  const contentScope = JSON.stringify(visible ? owner : null);
  const ownerRef = useRef(owner);
  ownerRef.current = owner;

  function apply(next: ContentSnapshot) {
    snapshotRef.current = next;
    setSnapshot(next);
  }

  useEffect(() => {
    let disposed = false;
    const subscriptions: (() => void)[] = [];
    const register = (name: string, handler: () => void) => {
      void listen(name, handler)
        .then((stop) => { if (disposed) stop(); else subscriptions.push(stop); })
        .catch((failure) => { if (!disposed) setError(problem(failure)); });
    };
    register("chat-content:revoked", () => {
      generation.current++;
      apply(empty());
      setReady(false);
      setPending(false);
      inFlight.current = false;
      setSources(null);
      setContext(false);
      setSelectedText("");
      setMenu(false);
      setError("Attachment access ended when the window closed. Select files or a capture source again.");
    });
    const clearScreen = () => {
      const hadScreen = !!snapshotRef.current.leaseId || snapshotRef.current.attachments.some((item) => item.provenance.includes("window"));
      generation.current++;
      apply({ attachments: snapshotRef.current.attachments.filter((item) => !item.provenance.includes("window")), leaseId: null });
      setSources(null);
      setPending(false);
      inFlight.current = false;
      if (hadScreen) setError("Screen access was revoked when the turn stopped. Capture again if needed.");
    };
    register("chat-content:screen-stopped", clearScreen);
    register("copilot:runtime-error", () => {
      clearScreen();
      const currentOwner = ownerRef.current;
      const epoch = generation.current;
      if (currentOwner) void invoke("chat_capture_stop", { owner: currentOwner, keepSnapshot: false })
        .catch((failure) => { if (!disposed && generation.current === epoch) setError(problem(failure)); });
    });
    return () => { disposed = true; subscriptions.forEach((stop) => stop()); };
  }, []);

  useEffect(() => {
    const epoch = ++generation.current;
    apply(empty());
    setReady(false);
    setSources(null);
    setContext(false);
    setSelectedText("");
    setError(null);
    setPending(false);
    inFlight.current = false;
    void invoke<ContentSnapshot | null>("chat_content_activate", { owner: visible ? owner : null })
      .then((next) => {
        if (generation.current !== epoch) return;
        if (next) apply(next);
        setReady(!!owner && visible);
      }).catch((failure) => {
        if (generation.current === epoch) setError(problem(failure));
      });
    return () => {
      generation.current++;
      // Revoke native leases even if the component disappears while a picker is open.
      void invoke("chat_content_activate", { owner: null }).catch((failure) => {
        console.error("Darbot could not revoke attachment access:", problem(failure));
      });
    };
  }, [contentScope]);

  useLayoutEffect(() => {
    onState(snapshot.attachments.length,
      snapshot.attachments.length > 0 || !!snapshot.leaseId || pending || sources !== null || context,
      pending || sources !== null || context);
  }, [snapshot, pending, sources, context, onState]);

  async function run<T>(operation: () => Promise<T>, accept: (result: T) => void) {
    if (inFlight.current) return;
    const epoch = generation.current;
    inFlight.current = true;
    setPending(true);
    setError(null);
    try {
      const result = await operation();
      if (epoch === generation.current) accept(result);
    } catch (failure) {
      if (epoch === generation.current) setError(problem(failure));
    } finally {
      if (epoch === generation.current) {
        inFlight.current = false;
        setPending(false);
      }
    }
  }

  useEffect(() => {
    if (!snapshot.leaseId || disabled || pending || !owner) return;
    let disposed = false;
    let running = false;
    const epoch = generation.current;
    const timer = window.setInterval(() => {
      if (running) return;
      running = true;
      void invoke<ContentSnapshot>("chat_capture_frame", { owner, leaseId: snapshot.leaseId })
        .then((next) => {
          if (!disposed && generation.current === epoch && snapshotRef.current.leaseId === snapshot.leaseId) apply(next);
        }).catch((failure) => {
          if (!disposed && generation.current === epoch && snapshotRef.current.leaseId === snapshot.leaseId) {
            apply({ attachments: snapshotRef.current.attachments.filter((item) =>
              !item.provenance.includes("window")), leaseId: null });
            setError(problem(failure));
          }
        }).finally(() => { running = false; });
    }, 3100);
    return () => { disposed = true; window.clearInterval(timer); };
  }, [snapshot.leaseId, disabled, pending, conversationId, agentId]);

  useImperativeHandle(ref, () => ({
    async freeze() {
      if (inFlight.current) throw new Error("Finish the attachment selection before sending.");
      const activeOwner = ownerRef.current;
      const epoch = generation.current;
      let next = snapshotRef.current;
      if (next.leaseId && activeOwner) {
        const reviewedIds = next.attachments.map((item) => item.id);
        next = await invoke<ContentSnapshot>("chat_capture_stop", {
          owner: activeOwner, keepSnapshot: true,
          expectedAttachmentId: next.attachments.find((item) => item.provenance === "Shared window snapshot")?.id ?? null,
        });
        if (epoch !== generation.current) throw new Error("The attachment recipient changed.");
        apply(next);
        if (reviewedIds.some((id) => !next.attachments.some((item) => item.id === id))) {
          throw new Error("Screen access expired. Review the draft and capture again before sending.");
        }
      }
      return next.attachments;
    },
    sent() { apply(empty()); },
  }), []);

  async function pickFiles() {
    setMenu(false);
    await run(() => invoke<ContentSnapshot>("chat_content_pick", { owner }), apply);
  }
  async function chooseScreen(ongoing: boolean) {
    setMenu(false);
    setShare(ongoing);
    await run(() => invoke<Source[]>("chat_capture_sources", { owner }), (next) => {
      setSources(next);
      setSourceId("");
    });
  }
  async function stop() {
    await run(() => invoke<ContentSnapshot>("chat_capture_stop", { owner, keepSnapshot: false }), (next) => {
      apply(next);
      setSources(null);
    });
  }

  return <section className="copilot-composer-input" aria-label="Message composer">
    {editor}
    <div className="copilot-content" aria-label="Message attachments">
      {snapshot.leaseId && <button type="button" className="quiet capture-stop"
        data-ui-action="workspace_capture_stop" disabled={pending} onClick={() => void stop()}>
        Stop sharing
      </button>}
    {menu && <div className="copilot-content-menu" aria-label="Add to message">
      <button type="button" className="quiet" data-ui-action="workspace_upload"
        disabled={!ready || disabled || pending} onClick={() => void pickFiles()}>Upload</button>
      <button type="button" className="quiet" data-ui-action="workspace_select_context"
        disabled={!ready || disabled || pending} onClick={() => { setMenu(false); setContext(true); }}>Select context</button>
      <button type="button" className="quiet" data-ui-action="workspace_capture"
        disabled={!ready || disabled || pending} onClick={() => void chooseScreen(false)}>Capture Screen</button>
      <button type="button" className="quiet" data-ui-action="workspace_share_screen"
        disabled={!ready || disabled || pending} onClick={() => void chooseScreen(true)}>Share Screen</button>
    </div>}
    {context && <div className="copilot-content-selection" role="group" aria-label="Select context">
      <p>Choose exact files, or paste only the text you want {agentName} to see.</p>
      <button type="button" className="quiet" data-ui-action="workspace_upload"
        disabled={disabled || pending} onClick={() => void pickFiles()}>Choose files</button>
      <label htmlFor="darbot-selected-context">Selected text</label>
      <textarea id="darbot-selected-context" data-ui-action="workspace_select_context"
        value={selectedText} maxLength={32000} disabled={disabled || pending}
        onChange={(event) => setSelectedText(event.target.value)} />
      <div className="row">
        <button type="button" data-ui-action="workspace_select_context" disabled={disabled || pending || !selectedText.trim()}
          onClick={() => void run(() => invoke<ContentSnapshot>("chat_content_text", { owner, text: selectedText }),
            (next) => { apply(next); setContext(false); setSelectedText(""); })}>Attach selected text</button>
        <button type="button" className="quiet" data-ui-action="workspace_attachment_cancel"
          disabled={pending} onClick={() => { setContext(false); setSelectedText(""); }}>Cancel</button>
      </div>
    </div>}
    {sources && <div className="copilot-content-selection" role="group" aria-label="Choose capture source">
      <p>{share ? "Share Screen" : "Capture Screen"} → {agentName}</p>
      <p className="hint">Conversation: {conversationId}. Windows only; no monitor capture. Nothing is sent automatically.</p>
      <label htmlFor="darbot-capture-source">Window to capture</label>
      <select id="darbot-capture-source" data-ui-action="workspace_capture_source"
        value={sourceId} disabled={pending} onChange={(event) => setSourceId(event.target.value)}>
        <option value="">Select a visible window…</option>
        {sources.map((source) => <option key={source.id} value={source.id}>{source.name}</option>)}
      </select>
      {!sources.length && <p>No visible capture sources are available.</p>}
      <div className="row">
        <button type="button" data-ui-action="workspace_capture_source" disabled={!sourceId || pending}
          onClick={() => void run(() => invoke<ContentSnapshot>("chat_capture_start", { owner, sourceId, share }),
            (next) => { apply(next); setSources(null); })}>Preview selected window</button>
        <button type="button" className="quiet" data-ui-action="workspace_attachment_cancel"
          disabled={pending} onClick={() => void stop()}>Cancel</button>
      </div>
    </div>}
    {snapshot.leaseId && <p role="status" className="copilot-sharing-status">
      Sharing locally with {agentName} · preview every 3 seconds. Send freezes one reviewed snapshot, not video.
    </p>}
    {snapshot.attachments.length > 0 && <ul className="copilot-attachments" aria-label="Staged attachments">
      {snapshot.attachments.map((item) => <li key={item.id}>
        {item.mimeType === "image/png"
          ? <img src={item.preview} alt={`Preview of ${item.name}`} />
          : <pre>{item.preview}</pre>}
        <div className="copilot-attachment-label"><strong>{item.name}</strong>
          <span>{item.provenance} · {item.mimeType} · {Math.ceil(item.bytes / 1024)} KiB</span></div>
        <button type="button" className="quiet" data-ui-action="workspace_attachment_remove"
          aria-label={`Remove ${item.name}`} disabled={disabled || pending}
          onClick={() => void run(() => invoke<ContentSnapshot>("chat_content_remove", { owner, id: item.id }), apply)}>×</button>
      </li>)}
    </ul>}
    {(menu || snapshot.attachments.length > 0 || context || sources) &&
      <p className="hint">PNG and UTF-8 text · 2 MiB/file · 8 attachments · memory only until Darbot closes.
        Screen images are revoked when you leave this conversation or hide its composer.</p>}
    {pending && <p role="status">Waiting for local selection or approval…</p>}
    {error && <p role="alert" className="copilot-content-error">{error}</p>}
    </div>
    <div className="copilot-composer-toolbar" role="group" aria-label="Message controls">
      <button type="button" className="quiet copilot-composer-add"
        data-ui-action="workspace_attachment_menu" aria-label="Add attachments or context"
        aria-expanded={menu} disabled={disabled || pending}
        onClick={() => {
          ensureConversation();
          setMenu(!menu);
          if (owner && !ready) void run(() => invoke<ContentSnapshot>("chat_content_activate", { owner }),
            (next) => { apply(next); setReady(true); });
        }}><span aria-hidden="true">+</span> Attach</button>
      {controls}
      <div className="copilot-composer-primary">{primaryAction}</div>
    </div>
  </section>;
});
