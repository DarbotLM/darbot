import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
} from "react";
import { asProblem, InlineFailure, type Problem } from "./Problem";

type Presentation = { widget: boolean; pinned: boolean; wide: boolean };

const DesktopWindowContext = createContext({
  widget: false,
  front: false,
  setAvailable: (_available: boolean) => {},
  showWorkspace: () => {},
});

export function useDesktopWindow() {
  return useContext(DesktopWindowContext);
}

export function DesktopWindow({ children }: { children: ReactNode }) {
  const [presentation, setPresentation] = useState<Presentation>({
    widget: false,
    pinned: false,
    wide: false,
  });
  const [front, setFront] = useState(true);
  const [available, setAvailable] = useState(false);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<Problem | null>(null);
  const changing = useRef(false);
  const flipRef = useRef<HTMLButtonElement>(null);
  const showWorkspace = useCallback(() => {
    setFront(false);
    flipRef.current?.focus();
  }, []);

  useEffect(() => {
    let active = true;
    invoke<Presentation>("desktop_window_state")
      .then((state) => {
        if (active) setPresentation(state);
      })
      .catch((error: unknown) => {
        if (active) setProblem(asProblem(error));
      });
    return () => {
      active = false;
    };
  }, []);

  async function present(
    widget: boolean,
    pinned: boolean,
    wide = presentation.wide,
  ) {
    if (changing.current) return;
    changing.current = true;
    setBusy(true);
    setProblem(null);
    try {
      setPresentation(
        await invoke<Presentation>("desktop_window_present", {
          widget,
          pinned,
          wide,
        }),
      );
      if (widget !== presentation.widget) setFront(true);
    } catch (error) {
      setProblem(asProblem(error));
    } finally {
      changing.current = false;
      setBusy(false);
    }
  }

  async function windowAction(action: () => Promise<void>) {
    setProblem(null);
    try {
      await action();
    } catch (error) {
      setProblem(asProblem(error));
    }
  }

  return (
    <DesktopWindowContext
      value={{
        widget: presentation.widget,
        front: presentation.widget && available && front,
        setAvailable,
        showWorkspace,
      }}
    >
      <div
        className={`desktop-window${presentation.widget ? " desktop-widget" : ""}${presentation.widget && presentation.wide ? " desktop-widget-wide" : ""}`}
      >
        <header className="desktop-window-bar">
          <button
            type="button"
            className="quiet desktop-window-drag"
            data-ui-action="window_drag"
            aria-label="Move Darbot window"
            title="Drag to move"
            onPointerDown={(event) => {
              if (event.button !== 0) return;
              void windowAction(() => getCurrentWindow().startDragging());
            }}
          >
            darbot
          </button>
          <div className="desktop-window-actions">
            {presentation.widget && available && (
              <button
                ref={flipRef}
                type="button"
                className="quiet"
                data-ui-action="window_flip"
                onClick={() => setFront(!front)}
                aria-label={front ? "Flip to workspace" : "Flip to live tile"}
              >
                {front ? "Chat" : "Tile"}
              </button>
            )}
            {(available || presentation.widget) && (
              <button
                type="button"
                className="quiet"
                data-ui-action="window_set_widget"
                disabled={busy}
                onClick={() =>
                  void present(!presentation.widget, presentation.pinned)
                }
              >
                {presentation.widget ? "Expand" : "Widget"}
              </button>
            )}
            {presentation.widget && (
              <>
                <button
                  type="button"
                  className="quiet"
                  data-ui-action="window_set_wide"
                  aria-pressed={presentation.wide}
                  disabled={busy}
                  onClick={() =>
                    void present(true, presentation.pinned, !presentation.wide)
                  }
                  title="Switch between portrait and 640 by 480 wide widget"
                >
                  {presentation.wide ? "Portrait" : "Wide"}
                </button>
                <button
                  type="button"
                  className="quiet"
                  data-ui-action="window_set_pinned"
                  aria-pressed={presentation.pinned}
                  disabled={busy}
                  onClick={() => void present(true, !presentation.pinned)}
                  title="Keep this widget above other windows"
                >
                  {presentation.pinned ? "Unpin" : "Pin"}
                </button>
                <button
                  type="button"
                  className="quiet"
                  data-ui-action="window_hide"
                  aria-label="Hide Darbot to tray"
                  title="Hide to tray; agents keep running"
                  onClick={() =>
                    void windowAction(() => getCurrentWindow().close())
                  }
                >
                  Close
                </button>
              </>
            )}
          </div>
        </header>
        {problem && (
          <div className="desktop-window-problem">
            <InlineFailure problem={problem} />
          </div>
        )}
        <div className="desktop-window-content">{children}</div>
        {presentation.widget && (
          <button
            type="button"
            className="quiet desktop-window-resize"
            data-ui-action="window_resize"
            aria-label="Resize Darbot widget"
            title="Drag to resize"
            onPointerDown={(event) => {
              if (event.button !== 0) return;
              void windowAction(() =>
                getCurrentWindow().startResizeDragging("SouthEast"),
              );
            }}
          >
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <path d="M4 12 12 4M8 12l4-4" />
            </svg>
          </button>
        )}
      </div>
    </DesktopWindowContext>
  );
}
