//! Machine-readable recorder status for automation drivers.
//!
//! The recorder bar and tray are excluded from screen capture, so a script that
//! drives Capptivo through the `--action` channel (see `lib.rs`) can't see them.
//! This writes the recorder's live state to a small JSON file the driver can
//! poll: `<app-data>/automation-state.json`.
//!
//! Poll contract:
//! - after `--action start`, wait for `status == "recording"` before acting;
//! - after `--action stop`, wait for `status == "idle"` **and** `screenPath`
//!   non-null — that pair means the take is finalized on disk at `screenPath`.
//!   `screenPath` is null while recording and stays null on a discarded/failed
//!   take, so it doubles as the "file is ready" signal.

use crate::recorder::types::RecorderState;
use crate::state::AppState;
use std::io::Write;
use tauri::{AppHandle, Manager};

const STATE_FILE: &str = "automation-state.json";

/// Stable status string for the state machine (kept in sync with the TS
/// `RecorderState` discriminants).
pub fn status_str(state: &RecorderState) -> &'static str {
    match state {
        RecorderState::Idle => "idle",
        RecorderState::Countdown { .. } => "countdown",
        RecorderState::Recording => "recording",
        RecorderState::Paused => "paused",
        RecorderState::Finalizing => "finalizing",
        RecorderState::Error { .. } => "error",
    }
}

/// Project id currently being captured, if any.
fn current_project_id(app: &AppHandle) -> Option<String> {
    app.try_state::<AppState>()
        .and_then(|s| s.current_project.lock().as_ref().map(|p| p.id.clone()))
}

/// Write the status file from a state-machine transition. No finalized path is
/// known here, so `screenPath` is cleared — that's also what resets a stale path
/// when a new take starts.
pub fn write_from_state(app: &AppHandle, state: &RecorderState) {
    write_state(app, status_str(state), current_project_id(app), None);
}

/// Write the status file. `screen_path` is set only once a take is finalized on
/// disk (see `do_stop_recording`), so a poller can treat
/// `status == "idle" && screenPath != null` as "recording complete, file ready".
///
/// Best-effort: any failure is swallowed — the app must never fall over because
/// it couldn't write a status file.
pub fn write_state(
    app: &AppHandle,
    status: &str,
    project_id: Option<String>,
    screen_path: Option<String>,
) {
    let Ok(dir) = app.path().app_data_dir() else {
        return;
    };
    let path = dir.join(STATE_FILE);
    let body = serde_json::json!({
        "status": status,
        "projectId": project_id,
        "screenPath": screen_path,
        "updatedAt": chrono::Utc::now().to_rfc3339(),
    })
    .to_string();

    // Write to a temp file and rename so a polling reader never observes a
    // half-written document.
    let _ = std::fs::create_dir_all(&dir);
    let tmp = path.with_extension("json.tmp");
    let wrote = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(body.as_bytes())?;
        f.sync_all()
    });
    if wrote.is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}
