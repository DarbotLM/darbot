import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";

type Profile = { id: string; agent: string; model: string; root: string; permissionProfile: "ask-every-tool" };
type Card = { version: number; nodeId: string; label: string; endpoint: string; certificate: number[] };
type Peer = { card: Card; online: boolean; queued: number; presence: { profiles: Profile[]; busy: boolean; error: string | null } };
type Snapshot = { enabled: boolean; card: Card; profiles: Profile[]; peers: Peer[]; error?: { message: string } | null };
type Task = { id: string; origin: string; executor: string; status: string; detail: string; revision: number;
  profile: Profile; prompt: string; output: string; sessionId: string | null; cancelRequested: boolean;
  a2a: boolean; messages: { id: string; author: string; text: string }[] };
type TaskRow = Pick<Task, "id" | "origin" | "executor" | "status" | "revision" | "sessionId" | "cancelRequested"> & { profileId: string };
const terminal = new Set(["completed", "failed", "cancelled", "reconciliation-required"]);
const command = <T,>(action: object) => invoke<T>("fleet_dispatch", { action });
function message(error: unknown): string {
  if (error && typeof error === "object" && "message" in error) return String(error.message);
  return error instanceof Error ? error.message : String(error);
}

export function FleetPanel({ visible, cwd, agents }: {
  visible: boolean; cwd: string; agents: { id: string; name: string }[];
}) {
  const [state, setState] = useState<Snapshot | null>(null);
  const [tasks, setTasks] = useState<TaskRow[]>([]);
  const [detail, setDetail] = useState<Task | null>(null);
  const [error, setError] = useState("");
  const [pending, setPending] = useState(false);
  const [label, setLabel] = useState("My Darbot");
  const [endpoint, setEndpoint] = useState("127.0.0.1:0");
  const [card, setCard] = useState("");
  const [root, setRoot] = useState(cwd);
  const [profileId, setProfileId] = useState("");
  const [agent, setAgent] = useState("");
  const [model, setModel] = useState("");
  const [selected, setSelected] = useState<string[]>([]);
  const [prompt, setPrompt] = useState("");
  const [expanded, setExpanded] = useState<string | null>(null);
  const [note, setNote] = useState("");

  async function refresh() {
    const [next, rows] = await Promise.all([command<Snapshot>({ action: "status" }), command<TaskRow[]>({ action: "list_tasks" })]);
    setState(next); setTasks(rows);
  }
  useEffect(() => {
    if (!visible) return;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try {
        const [next, rows, selectedTask] = await Promise.all([command<Snapshot>({ action: "status" }),
          command<TaskRow[]>({ action: "list_tasks" }), expanded ? command<Task>({ action: "get_task", taskId: expanded }) : Promise.resolve(null)]);
        if (!stopped) { setState(next); setTasks(rows); setDetail(selectedTask); }
      } catch (e) { if (!stopped) setError(message(e)); }
      if (!stopped) timer = setTimeout(poll, 2000);
    };
    void poll();
    return () => { stopped = true; clearTimeout(timer); };
  }, [visible, expanded]);
  async function run(action: object) {
    if (pending) return;
    setPending(true); setError("");
    try { await command(action); await refresh(); }
    catch (e) { setError(message(e)); }
    finally { setPending(false); }
  }
  const targets = state ? [
    ...state.profiles.map((profile) => ({ nodeId: state.card.nodeId, profile, label: "This desktop", online: state.enabled })),
    ...state.peers.flatMap((peer) => peer.presence.profiles.map((profile) =>
      ({ nodeId: peer.card.nodeId, profile, label: peer.card.label, online: peer.online && !peer.presence.busy }))),
  ] : [];

  return <details className="fleet-panel">
    <summary data-ui-action="settings_fleet">Fleet · {state?.enabled ? "enabled" : "disabled"} · {state?.peers.filter((p) => p.online).length ?? 0} online</summary>
    <p>Pair Windows users once using pinned mutual TLS. Only tasks you explicitly share and their assistant text results travel between peers. Local approval is required for every task and tool.</p>
    <p>A2A 0.3 text-task profile: authenticated agent card, message/send, message/stream, tasks/get, tasks/cancel and tasks/resubscribe. No push notifications, files, shared contexts or automatic task continuation.</p>
    {error && <p role="alert">{error}</p>}
    {state?.error && <p role="alert">{state.error.message}</p>}
    {pending && <p role="status">Waiting for native approval or storage…</p>}
    <button data-ui-action="settings_fleet" type="button" disabled={pending || !state} onClick={() => void run({ action: state?.enabled ? "disable" : "enable" })}>
      {state?.enabled ? "Disable fleet" : "Enable fleet"}
    </button>
    <details>
      <summary data-ui-action="settings_fleet">Pairing and listener</summary>
      <p>Configure an explicit listening IP and port. Darbot does not open firewall rules. Import cards on both desktops and compare fingerprints through a trusted channel; names alone prove nothing.</p>
      <label>Node label<input data-ui-action="settings_fleet" value={label} maxLength={100} onChange={(e) => setLabel(e.target.value)} /></label>
      <label>Listener IP:port<input data-ui-action="settings_fleet" value={endpoint} onChange={(e) => setEndpoint(e.target.value)} /></label>
      <button data-ui-action="settings_fleet" type="button" disabled={pending || state?.enabled} onClick={() => void run({ action: "configure", listen: endpoint, label })}>Save listener</button>
      {state && <>
        <p>Current listener: {state.card.endpoint} · A2A JSON-RPC path: /a2a</p>
        <p>A2A clients use TLS server name darbot-fleet with a per-peer resolver for this address, the pinned certificates, and A2A-Extensions: urn:darbot:fleet:1. Discover the profile at /.well-known/agent-card.json.</p>
        <p>Use a reachable interface IP and fixed port before exchanging cards. Enabled peers reconnect automatically while Darbot is running. A changed endpoint requires re-pairing on both sides after checking outstanding work.</p>
        <label>Public pairing card<textarea data-ui-action="settings_fleet" rows={3} readOnly value={JSON.stringify(state.card)} /></label>
        <p className="fleet-fingerprint">Fingerprint: {state.card.nodeId}</p>
      </>}
      <label>Other user's public pairing card<textarea data-ui-action="settings_fleet" rows={3} value={card} onChange={(e) => setCard(e.target.value)} /></label>
      <button data-ui-action="settings_fleet" type="button" disabled={pending || !card} onClick={() => {
        try { void run({ action: "pair", card: JSON.parse(card) }); } catch (e) { setError(message(e)); }
      }}>Review pairing</button>
    </details>
    <ul className="fleet-peers">
      {state?.peers.map((peer) => <li key={peer.card.nodeId}>
        <strong>{peer.card.label}</strong> · {peer.online ? peer.presence.busy ? "busy" : "online" : "offline"} · {peer.queued} queued deliveries
        <button data-ui-action="settings_fleet" type="button" className="quiet" disabled={pending} onClick={() => void run({ action: "revoke", nodeId: peer.card.nodeId })}>Revoke</button>
        {peer.presence.error && <small>{peer.presence.error}</small>}
      </li>)}
    </ul>
    <details>
      <summary data-ui-action="settings_fleet">Approved local execution scopes ({state?.profiles.length ?? 0})</summary>
      <p>Agent is the task personality. Enter an exact CLI model ID; Darbot validates it against the real session before sending. Roots are canonical local directories, not remote paths.</p>
      {state?.profiles.map((p) => <p key={p.id}>{p.id} · {p.agent || "Copilot"} · {p.model} · {p.root}
        <button data-ui-action="settings_fleet" type="button" className="quiet" disabled={pending} onClick={() => void run({ action: "set_profiles", profiles: state.profiles.filter((item) => item.id !== p.id) })}>Remove scope</button>
      </p>)}
      <label>Scope ID<input data-ui-action="settings_fleet" value={profileId} maxLength={80} onChange={(e) => setProfileId(e.target.value)} /></label>
      <label>Repository root<input data-ui-action="settings_fleet" value={root} onChange={(e) => setRoot(e.target.value)} /></label>
      <label>Agent/personality<select data-ui-action="settings_fleet" value={agent} onChange={(e) => setAgent(e.target.value)}>
        <option value="">Copilot CLI default</option>
        {agents.filter((a) => a.id && !a.id.includes(":")).map((a) => <option key={a.id} value={a.id}>{a.name}</option>)}
      </select></label>
      <label>Exact CLI model ID<input data-ui-action="settings_fleet" value={model} maxLength={128} onChange={(e) => setModel(e.target.value)} /></label>
      <button data-ui-action="settings_fleet" type="button" disabled={pending || !profileId || !model || !root} onClick={() => void run({
        action: "set_profiles", profiles: [...(state?.profiles ?? []).filter((p) => p.id !== profileId),
          { id: profileId, root, agent, model, permissionProfile: "ask-every-tool" }],
      })}>Review execution scope</button>
    </details>
    <details>
      <summary data-ui-action="settings_fleet">Share a round-robin task</summary>
      <p>The persistent cursor selects an online, idle target. Capacity is one task per desktop; offline or busy nodes are skipped. Ambiguous work is never automatically re-executed.</p>
      <p>If a request fails or times out, inspect the task list before submitting again. A completed model turn is not independent verification of the task's real-world outcome.</p>
      {targets.map((target) => {
        const key = JSON.stringify([target.nodeId, target.profile.id]);
        return <label key={key}><input data-ui-action="settings_fleet" type="checkbox" checked={selected.includes(key)} disabled={!target.online}
          onChange={(e) => setSelected((old) => e.target.checked ? [...old, key] : old.filter((k) => k !== key))} />
          {target.label} / {target.profile.id} · {target.profile.agent || "Copilot"} · {target.profile.model}
        </label>;
      })}
      <label>Selected task prompt<textarea data-ui-action="settings_fleet" rows={3} maxLength={2000} value={prompt} onChange={(e) => setPrompt(e.target.value)} /></label>
      <button data-ui-action="settings_fleet" type="button" disabled={pending || !state?.enabled || !prompt.trim() || !selected.length} onClick={() => void run({
        action: "submit", submission: { requestId: crypto.randomUUID(), prompt, expiresAt: Date.now() + 60 * 60 * 1000,
          targets: selected.map((key) => { const [nodeId, profileId] = JSON.parse(key); return { nodeId, profileId }; }) },
      })}>Review and dispatch</button>
    </details>
    <h4>Selected fleet tasks</h4>
    {!tasks.length && <p>No shared tasks.</p>}
    {tasks.map((task) => <article className="fleet-task" key={task.id}>
      <button data-ui-action="settings_fleet" type="button" className="quiet" onClick={() => setExpanded(expanded === task.id ? null : task.id)}>
        {task.profileId} · {task.status}
      </button>
      {task.executor === state?.card.nodeId && task.status === "input-required" && !task.sessionId &&
        <button data-ui-action="settings_fleet" type="button" disabled={pending} onClick={() => void run({ action: "approve_task", taskId: task.id })}>Review execution</button>}
      {task.origin === state?.card.nodeId && !terminal.has(task.status) &&
        <button data-ui-action="settings_fleet" type="button" className="quiet" disabled={pending || task.cancelRequested}
          onClick={() => void run({ action: "cancel", taskId: task.id })}>{task.cancelRequested ? "Cancellation requested" : "Cancel task"}</button>}
      {expanded === task.id && detail?.id === task.id && <div>
        <p>{detail.prompt}</p><small>{detail.detail}</small>
        <small>{task.id} · {detail.profile.agent || "Copilot"} · {detail.profile.model} · {detail.profile.root}</small>
        <pre>{detail.output || "No assistant output yet."}</pre>
        {detail.messages?.map((item) => <p key={`${item.author}:${item.id}`}>
          <strong>{item.author === state?.card.nodeId ? "You" : "Peer"}:</strong> {item.text}
        </p>)}
        {!detail.a2a && <>
          <label>Task note (does not start an agent turn)<textarea data-ui-action="settings_fleet"
            value={note} maxLength={2000} onChange={(e) => setNote(e.target.value)} /></label>
          <button data-ui-action="settings_fleet" type="button" disabled={pending || !note.trim()}
            onClick={() => void run({ action: "message", taskId: task.id, messageId: crypto.randomUUID(), text: note })}>Review and send note</button>
        </>}
      </div>}
    </article>)}
  </details>;
}
