import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type Status = {
  enabled: boolean;
  currentVersion: string;
  phase: string;
  available: { version: string } | null;
  signed: boolean | null;
  notify: boolean;
  error: { message: string } | null;
};
const descriptions: Record<string, string> = {
  disabled: "Automatic checks are off.",
  idle: "Automatic checks are enabled.",
  checking: "Verifying the build with GitHub…",
  available: "A newer verified build is available.",
  no_newer_build: "No newer build was found.",
  verification_pending:
    "A peer reported an update; GitHub verification is pending.",
  error: "The update check could not finish.",
};

export function DesktopUpdates({
  mode,
  visible = true,
}: {
  mode: "settings" | "toast";
  visible?: boolean;
}) {
  const [status, setStatus] = useState<Status | null>(null);
  const [failure, setFailure] = useState("");
  const [pending, setPending] = useState(false);
  useEffect(() => {
    let active = true;
    let stop: (() => void) | undefined;
    let changed = false;
    void listen<Status>("desktop:update-state", (event) => {
      changed = true;
      if (active) setStatus(event.payload);
    })
      .then(async (unlisten) => {
        if (!active) {
          unlisten();
          return;
        }
        stop = unlisten;
        const initial = await invoke<Status>("desktop_update_status");
        if (active && !changed) setStatus(initial);
      })
      .catch(() => {
        if (active)
          setFailure("Update status is unavailable. Reopen Settings to retry.");
      });
    return () => {
      active = false;
      stop?.();
    };
  }, []);
  async function run(command: string, args?: Record<string, unknown>) {
    if (pending) return;
    setPending(true);
    setFailure("");
    try {
      const result = await invoke<Status | null>(command, args);
      if (result) setStatus(result);
    } catch (error) {
      setFailure(
        error && typeof error === "object" && "message" in error
          ? String(error.message)
          : "The update operation failed. No update was installed.",
      );
    } finally {
      setPending(false);
    }
  }
  if (mode === "toast") {
    if (!visible || !status?.notify || !status.available) return null;
    return (
      <aside
        className="desktop-update-toast"
        role="status"
        aria-label="Darbot update"
      >
        <div>
          <strong>Darbot {status.available.version} is available</strong>
          <p>
            Current: {status.currentVersion}.{" "}
            {status.signed
              ? "Signed build."
              : "Unsigned build; review before installing."}
          </p>
        </div>
        <button
          type="button"
          data-ui-action="settings_desktop_updates"
          disabled={pending}
          onClick={() => void run("desktop_update_view")}
        >
          View update
        </button>
        <button
          type="button"
          className="quiet"
          data-ui-action="settings_desktop_updates"
          aria-label="Dismiss update notice"
          disabled={pending}
          onClick={() => void run("desktop_update_dismiss")}
        >
          Dismiss
        </button>
        {failure && <p role="alert">{failure}</p>}
      </aside>
    );
  }
  return (
    <section aria-label="Desktop updates">
      <h3>Desktop updates</h3>
      <p>
        Verified builds from DarbotLM/darbot. Paired peers share notices only;
        installation is never automatic.
      </p>
      <label>
        <input
          type="checkbox"
          data-ui-action="settings_desktop_updates"
          checked={status?.enabled ?? false}
          disabled={pending || !status}
          onChange={(event) =>
            void run("desktop_update_enable", { enabled: event.target.checked })
          }
        />
        Check at startup and every 15 minutes
      </label>
      <button
        type="button"
        className="quiet"
        data-ui-action="settings_desktop_updates"
        disabled={pending || status?.phase === "checking"}
        onClick={() => void run("desktop_update_check")}
      >
        Check for updates
      </button>
      {status && (
        <p aria-live="polite">
          {descriptions[status.phase] ?? "Update status changed."}
        </p>
      )}
      {(failure || status?.error) && (
        <p role="alert">{failure || status?.error?.message}</p>
      )}
    </section>
  );
}
