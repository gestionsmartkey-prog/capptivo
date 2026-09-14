//! Capptivo Desktop — app entry (library form; `main.rs` just calls [`run`]).
//!
//! The builder stays thin: register plugins + the `media://` protocol, build
//! state and the tray in `setup`, and register the command handler. All logic
//! lives in the domain modules, none of which import `tauri` except `commands`,
//! `state`, `tray`, `windows`, and this file (§14).

pub mod captions;
mod capabilities;
mod commands;
mod proc;
mod export_h264;
mod export_rawvideo;
mod webview_gpu;
#[cfg(any(
    all(target_os = "macos", feature = "scap-capture"),
    all(target_os = "windows", feature = "wgc-capture")
))]
mod area_picker;
#[cfg(not(any(
    all(target_os = "macos", feature = "scap-capture"),
    all(target_os = "windows", feature = "wgc-capture")
)))]
mod area_picker {
    use crate::error::{AppError, AppResult};
    use crate::recorder::types::CaptureAreaSelection;
    use tauri::{AppHandle, State};

    pub struct AreaPickState;
    impl AreaPickState {
        pub fn new() -> Self {
            Self
        }
    }
    pub async fn pick_capture_area(
        _app: AppHandle,
        _pick_state: State<'_, AreaPickState>,
    ) -> AppResult<CaptureAreaSelection> {
        Err(AppError::Unsupported)
    }
    pub fn complete_area_pick(
        _app: &AppHandle,
        _pick_state: &AreaPickState,
        _x: f64,
        _y: f64,
        _width: f64,
        _height: f64,
    ) -> AppResult<()> {
        Err(AppError::Unsupported)
    }
    pub fn cancel_area_pick(_app: &AppHandle, _pick_state: &AreaPickState) {}
    pub fn show_area_frame_guide(
        _app: &AppHandle,
        _selection: &crate::recorder::types::CaptureAreaSelection,
    ) -> AppResult<()> {
        Err(AppError::Unsupported)
    }
    pub fn hide_area_frame_guide(_app: &AppHandle) {}
}
mod backgrounds;
mod cursor;
mod error;
mod error_log;
mod media_protocol;
mod permissions;
mod project;
mod recorder;
mod state;
mod tray;
mod updater;
mod windows;

use state::AppState;
use tauri::Manager;

// ponytail: file logging disabled for release — re-enable with the block in `init_tracing`.
// use std::sync::OnceLock;
// use tracing_appender::non_blocking::WorkerGuard;
// static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Global show/hide hotkey for the recorder popover.
const RECORDER_HOTKEY: &str = "Alt+Shift+R";

/// System-wide recorder-control hotkeys → action names dispatched by
/// [`windows::handle_recorder_hotkey`]. `Ctrl+Shift+F{n}` is deliberate: it
/// dodges Chrome / Windows shortcuts and stays clear of AltGr on LATAM
/// keyboards, so the whole start → stop → new loop can be driven from any app.
const RECORDER_HOTKEYS: &[(&str, &str)] = &[
    ("Ctrl+Shift+F9", "start"),
    ("Ctrl+Shift+F10", "stop"),
    ("Ctrl+Shift+F8", "pause"),
    ("Ctrl+Shift+F7", "cancel"),
    ("Ctrl+Shift+F6", "new"),
    ("Ctrl+Shift+F5", "mic"),
    ("Ctrl+Shift+F4", "camera"),
    ("Ctrl+Shift+F3", "system-audio"),
];

/// Parse `--action <name>` / `--action=<name>` out of a forwarded command line,
/// accepting only known recorder actions so a stray argument can't emit garbage.
/// This is the non-keystroke automation channel: `Capptivo.exe --action stop`
/// launches a throwaway second process, the single-instance plugin hands its
/// argv to the running app, and it exits.
fn recorder_action_from_argv(argv: &[String]) -> Option<String> {
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        let candidate = if let Some(v) = arg.strip_prefix("--action=") {
            Some(v.to_string())
        } else if arg == "--action" {
            it.next().cloned()
        } else {
            None
        };
        if let Some(action) = candidate {
            if RECORDER_HOTKEYS.iter().any(|(_, a)| *a == action) {
                return Some(action);
            }
        }
    }
    None
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

    tauri::Builder::default()
        // Must register first — later plugins depend on the single-instance lock order.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // `Capptivo.exe --action <name>` from a second launch is forwarded here
            // over the single-instance IPC and drives the recorder exactly like the
            // global hotkeys — but without injected keystrokes, so it works for
            // automation that Windows' input hooks / UIPI would otherwise block.
            if let Some(action) = recorder_action_from_argv(&argv) {
                windows::handle_recorder_hotkey(app, &action);
                return;
            }
            if let Err(e) = windows::show_recorder_popover(app) {
                tracing::warn!(%e, "single-instance: failed to show recorder");
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        // media:// — Range-capable local media (§10). Project files under
        // `projects/<id>/…`; custom backgrounds under `_backgrounds/<file>`.
        .register_uri_scheme_protocol("media", |ctx, request| {
            let app = ctx.app_handle();
            let app_data = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("Capptivo"));
            media_protocol::serve(&app_data, &request)
        })
        .setup(|app| {
            let handle = app.handle().clone();
            app.manage(AppState::build(&handle));
            app.manage(area_picker::AreaPickState::new());
            tray::build(&handle)?;
            register_global_hotkey(&handle);
            register_recorder_hotkeys(&handle);

            std::thread::Builder::new()
                .name("encoder-probe-warm".into())
                .spawn(|| {
                    let _ = recorder::hw_encoder::pick(&recorder::encoder::ffmpeg_path());
                })
                .ok();

            // Start menubar-only; `windows::sync_dock_policy` flips to
            // Regular while a library or editor window is open (native
            // fullscreen + green expand arrows need Regular) and back
            // when the last closes.
            #[cfg(target_os = "macos")]
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                windows::refresh_activation_policy(window.app_handle(), window.label());
            }
        })
        .invoke_handler(crate::command_handlers!())
        .build(tauri::generate_context!())
        .expect("error while building Capptivo Desktop")
        .run(|app, event| match event {
            // Launch straight into the recorder — clicking the app means the
            // user wants to record, not hunt the tray icon first. Deferred to
            // `Ready` (event loop live, Accessory policy applied) so the
            // window's first orderFront happens in a settled app — see the
            // comment in `setup`.
            tauri::RunEvent::Ready => {
                if let Err(e) = windows::show_recorder_popover(app) {
                    tracing::warn!(%e, "failed to open recorder on launch");
                }
                updater::schedule_startup_check(app);
            }
            // Dock / Finder reopen (macOS): show the recorder rather than
            // silently sitting in the menubar.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => {
                if let Err(e) = windows::show_recorder_popover(app) {
                    tracing::warn!(%e, "failed to open recorder on reopen");
                }
            }
            _ => {}
        });
}

fn register_global_hotkey(app: &tauri::AppHandle) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;

    let result = app.global_shortcut().on_shortcut(RECORDER_HOTKEY, |app, _shortcut, event| {
        // Fire once, on key-down.
        if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
            if let Err(e) = windows::toggle_recorder_popover(app) {
                tracing::warn!(%e, "hotkey: failed to toggle recorder popover");
            }
        }
    });
    if let Err(e) = result {
        tracing::warn!(%e, hotkey = RECORDER_HOTKEY, "failed to register global hotkey");
    }
}

/// Register the system-wide recorder-control hotkeys. Each is best-effort:
/// registration fails on Wayland (no global-shortcut portal) and if another app
/// already owns the combo, and a warning is the right outcome either way — the
/// tray menu and in-bar controls still work.
fn register_recorder_hotkeys(app: &tauri::AppHandle) {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

    for (accel, action) in RECORDER_HOTKEYS {
        let action = *action;
        let result = app
            .global_shortcut()
            .on_shortcut(*accel, move |app, _shortcut, event| {
                // Fire once, on key-down.
                if event.state() == ShortcutState::Pressed {
                    windows::handle_recorder_hotkey(app, action);
                }
            });
        if let Err(e) = result {
            tracing::warn!(%e, hotkey = *accel, "failed to register recorder hotkey");
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    // ponytail: persistent file logs + errors.log off for release builds.
    // Uncomment the block below (and LOG_GUARD above) to re-enable friend-install diagnostics.
    //
    // use tracing_subscriber::filter::LevelFilter;
    // crate::error_log::init();
    // let dir = crate::error_log::logs_dir();
    // let _ = std::fs::create_dir_all(&dir);
    // let file_appender = tracing_appender::rolling::daily(&dir, "capptivo");
    // let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    // let _ = LOG_GUARD.set(guard);
    // let rolling_filter = EnvFilter::new(&env);

    let env = std::env::var("RUST_LOG").unwrap_or_else(|_| "info,desktop_lib=debug".into());
    let console_filter = EnvFilter::new(&env);

    // Console only (`tauri dev` / stderr). No on-disk `capptivo.*` / `errors.log`.
    let _ = tracing_subscriber::registry()
        .with(fmt::layer().with_target(false).with_filter(console_filter))
        // .with(
        //     fmt::layer()
        //         .with_ansi(false)
        //         .with_target(true)
        //         .with_writer(non_blocking)
        //         .with_filter(rolling_filter),
        // )
        // .with(crate::error_log::ErrorFileLayer.with_filter(LevelFilter::WARN))
        .try_init();

    // tracing::info!(dir = %dir.display(), "file logging enabled");
}

#[cfg(test)]
mod tests {
    use super::recorder_action_from_argv;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_space_and_equals_forms() {
        assert_eq!(
            recorder_action_from_argv(&argv(&["Capptivo.exe", "--action", "start"])),
            Some("start".into())
        );
        assert_eq!(
            recorder_action_from_argv(&argv(&["Capptivo.exe", "--action=stop"])),
            Some("stop".into())
        );
    }

    #[test]
    fn every_hotkey_action_is_accepted() {
        for (_, action) in super::RECORDER_HOTKEYS {
            assert_eq!(
                recorder_action_from_argv(&argv(&["Capptivo.exe", "--action", action])),
                Some((*action).into()),
                "action {action} should be accepted"
            );
        }
    }

    #[test]
    fn rejects_unknown_or_missing_action() {
        assert_eq!(recorder_action_from_argv(&argv(&["Capptivo.exe"])), None);
        assert_eq!(
            recorder_action_from_argv(&argv(&["Capptivo.exe", "--action", "explode"])),
            None
        );
        // `--action` with no value must not panic or consume a bogus flag.
        assert_eq!(
            recorder_action_from_argv(&argv(&["Capptivo.exe", "--action"])),
            None
        );
    }
}
