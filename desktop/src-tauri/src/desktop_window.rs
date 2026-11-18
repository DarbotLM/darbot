use darbot_desktop_lib::problem::Problem;
use serde::Serialize;
use std::sync::Mutex;
use tauri::{Manager, PhysicalPosition, PhysicalSize, Size, State, WebviewWindow};

#[derive(Clone, Copy, Default, Serialize)]
pub struct Presentation {
    widget: bool,
    pinned: bool,
    wide: bool,
}

#[derive(Clone, Copy)]
struct Bounds {
    size: PhysicalSize<u32>,
    position: PhysicalPosition<i32>,
    maximized: bool,
}

#[derive(Default)]
pub struct DesktopWindowState(Mutex<(Presentation, Option<Bounds>)>);

fn problem(error: tauri::Error) -> Problem {
    Problem::with(
        "Darbot could not change the window. Try again.",
        error.to_string(),
    )
}

fn bounds(window: &WebviewWindow) -> tauri::Result<Bounds> {
    Ok(Bounds {
        size: window.inner_size()?,
        position: window.outer_position()?,
        maximized: window.is_maximized()?,
    })
}

fn apply(
    window: &WebviewWindow,
    presentation: Presentation,
    target: Option<Bounds>,
) -> tauri::Result<()> {
    let config = window.app_handle().config();
    let normal = config
        .app
        .windows
        .iter()
        .find(|config| config.label == window.label());
    let minimum = if presentation.widget {
        Some(Size::Logical(tauri::LogicalSize::new(320.0, 360.0)))
    } else {
        normal.and_then(|config| {
            config
                .min_width
                .zip(config.min_height)
                .map(|(width, height)| Size::Logical(tauri::LogicalSize::new(width, height)))
        })
    };
    window.unmaximize()?;
    window.set_min_size(minimum)?;
    window.set_decorations(!presentation.widget)?;
    if let Some(target) = target {
        window.set_size(target.size)?;
        window.set_position(target.position)?;
        if target.maximized {
            window.maximize()?;
        }
    } else if presentation.widget {
        let (width, height) = if presentation.wide {
            (640.0, 480.0)
        } else {
            (380.0, 520.0)
        };
        window.set_size(tauri::LogicalSize::new(width, height))?;
    }
    window.set_always_on_top(presentation.widget && presentation.pinned)?;
    Ok(())
}

#[tauri::command]
pub fn desktop_window_state(state: State<'_, DesktopWindowState>) -> Presentation {
    state.0.lock().unwrap().0
}

#[tauri::command]
pub fn desktop_window_present(
    window: WebviewWindow,
    state: State<'_, DesktopWindowState>,
    widget: bool,
    pinned: bool,
    wide: bool,
) -> Result<Presentation, Problem> {
    let mut state = state.0.lock().unwrap();
    let previous = state.0;
    let next = Presentation {
        widget,
        pinned: widget && pinned,
        wide,
    };
    if previous.widget == widget && (!widget || previous.wide == wide) {
        window
            .set_always_on_top(next.widget && next.pinned)
            .map_err(problem)?;
    } else {
        let current = bounds(&window).map_err(problem)?;
        if let Err(error) = apply(&window, next, if widget { None } else { state.1 }) {
            let detail = match apply(&window, previous, Some(current)) {
                Ok(()) => error.to_string(),
                Err(restore) => {
                    format!("{error}\nRestoring the previous window also failed: {restore}")
                }
            };
            return Err(Problem::with(
                "Darbot could not change the window mode.",
                detail,
            ));
        }
        if previous.widget != widget {
            state.1 = if widget { Some(current) } else { None };
        }
    }
    state.0 = next;
    Ok(next)
}
