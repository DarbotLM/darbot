import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";

type ApprovalRequest = {
  requestId: string;
  title: string;
  message: string;
  expiresAt: number;
};

type Outcome = "always_allow" | "allow_once" | "cancel";

export function ApprovalOverlay() {
  const [request, setRequest] = useState<ApprovalRequest | null>(null);
  const [responding, setResponding] = useState(false);

  useEffect(() => {
    let active = true;
    const unlisten = listen<ApprovalRequest>(
      "app-control:owner-approval",
      (event) => {
        if (active) setRequest(event.payload);
      },
    );
    return () => {
      active = false;
      void unlisten.then((fn) => fn());
    };
  }, []);

  // Auto-dismiss when the approval request expires.
  useEffect(() => {
    if (!request) return;
    const remaining = request.expiresAt - Date.now();
    if (remaining <= 0) {
      setRequest(null);
      return;
    }
    const timer = setTimeout(() => setRequest(null), remaining);
    return () => clearTimeout(timer);
  }, [request]);

  if (!request) return null;

  async function respond(outcome: Outcome) {
    if (!request || responding) return;
    setResponding(true);
    try {
      await invoke("app_control_approval_respond", {
        requestId: request.requestId,
        outcome,
      });
    } catch {
      // The backend may have already timed out; dismiss the overlay regardless.
    } finally {
      setRequest(null);
      setResponding(false);
    }
  }

  return (
    <div
      className="approval-overlay"
      role="dialog"
      aria-modal="true"
      aria-labelledby="approval-title"
    >
      <div className="approval-card">
        <div className="approval-card-header">
          <span aria-hidden="true" style={{ fontSize: "1.1rem" }}>
            ⚠️
          </span>
          <h3 id="approval-title">{request.title}</h3>
        </div>
        <div className="approval-card-body">{request.message}</div>
        <div className="approval-card-actions">
          <button
            type="button"
            className="cancel-btn quiet"
            disabled={responding}
            onClick={() => void respond("cancel")}
          >
            Cancel
          </button>
          <button
            type="button"
            disabled={responding}
            onClick={() => void respond("allow_once")}
          >
            Allow once
          </button>
          <button
            type="button"
            className="always-allow"
            disabled={responding}
            onClick={() => void respond("always_allow")}
          >
            Always Allow
          </button>
        </div>
      </div>
    </div>
  );
}
