//! Velotype - a block-based Markdown editor built with GPUI.
//!
//! Reads file paths from command-line arguments and opens one GPUI window per
//! file. With no arguments, a single empty window is created.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::borrow::Cow;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[cfg(target_os = "macos")]
use futures::{StreamExt, channel::mpsc};
use gpui::*;

mod app_identity;
mod app_menu;
mod components;
mod config;
mod editor;
mod export;
#[cfg(any(target_os = "macos", test))]
mod file_url;
mod i18n;
mod net;
mod theme;
mod window_chrome;

use app_menu::{init as init_app_menu, open_editor_window};
use components::init_with_keybindings as init_editor;
#[cfg(target_os = "macos")]
use file_url::parse_file_url;
use i18n::I18nManager;
use theme::ThemeManager;

struct VelotypeAssets;

fn open_startup_window(cx: &mut App, startup_open: config::StartupOpenPreference) {
    if startup_open == config::StartupOpenPreference::LastOpenedFile
        && let Some(path) = config::first_existing_recent_markdown_file()
    {
        match std::fs::read_to_string(&path) {
            Ok(markdown) => {
                open_editor_window(cx, markdown, Some(path));
                return;
            }
            Err(err) => {
                eprintln!(
                    "failed to read last opened file '{}': {err}",
                    path.display()
                );
            }
        }
    }

    open_editor_window(cx, String::new(), None);
}

impl AssetSource for VelotypeAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        match path {
            "icon/workspace/folder.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/workspace/folder.svg"
            )))),
            "icon/workspace/markdown.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/workspace/markdown.svg"
            )))),
            "icon/titlebar/chrome-close.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/titlebar/chrome-close.svg"
            )))),
            "icon/titlebar/chrome-minimize.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/titlebar/chrome-minimize.svg"
            )))),
            "icon/titlebar/chrome-maximize.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/titlebar/chrome-maximize.svg"
            )))),
            "icon/titlebar/chrome-restore.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/titlebar/chrome-restore.svg"
            )))),
            _ => Ok(None),
        }
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// Re-launches a fresh, detached GUI process that outlives this one, then lets
/// `main` return so the shell is freed immediately (non-blocking launch).
///
/// The child runs with `--no-detach` (so it does not detach again) and its
/// stdin on null (so its stdin auto-detection does not try to read a pipe this
/// parent owns). Any markdown drained from a stdin pipe here is staged to a
/// temp file and passed via `--stdin-file`; the child reads and deletes it.
#[cfg(target_os = "macos")]
fn relaunch_detached(args: &[String], piped_stdin: Option<&str>) {
    use std::process::{Command, Stdio};

    let exe_path = std::env::current_exe().expect("Failed to get executable path");

    // Forward the original args minus the detach/wait flags (the child must not
    // detach again) and the bare '-' marker.
    let forwarded: Vec<&String> = args[1..]
        .iter()
        .filter(|arg| {
            !matches!(
                arg.as_str(),
                "--detach"
                    | "-d"
                    | "--background"
                    | "--wait"
                    | "-w"
                    | "--single-instance"
                    | "--new-instance"
                    | "-"
            )
        })
        .collect();

    let mut command = Command::new(&exe_path);
    command.args(&forwarded).arg("--no-detach").stdin(Stdio::null());

    // Stage piped markdown to a temp file so the detached child can open it.
    // pid is unique among live processes, enough to avoid collisions between
    // concurrent pipes into velotype.
    let staged_temp = piped_stdin.and_then(|markdown| {
        let temp_path =
            std::env::temp_dir().join(format!("velotype-stdin-{}.md", std::process::id()));
        match std::fs::write(&temp_path, markdown) {
            Ok(()) => {
                command.arg("--stdin-file").arg(&temp_path);
                Some(temp_path)
            }
            Err(err) => {
                eprintln!("failed to stage piped stdin: {err}");
                None
            }
        }
    });

    if let Err(err) = command.spawn() {
        eprintln!("failed to launch editor: {err}");
        if let Some(temp_path) = staged_temp {
            let _ = std::fs::remove_file(&temp_path);
        }
    }
}

/// Resolves the enclosing `.app` bundle for the current executable, if any.
/// An installed app lives at `<Name>.app/Contents/MacOS/<bin>`; a raw binary
/// (e.g. `cargo run`, `target/debug/velotype`) is not in a bundle and yields
/// None, so callers fall back to spawning a fresh process.
#[cfg(target_os = "macos")]
fn macos_app_bundle_path() -> Option<PathBuf> {
    // Canonicalize first: `current_exe()` returns the path used to launch, which
    // is the symlink itself (e.g. ~/.local/bin/velotype) when invoked through
    // one. The bundle only shows up once symlinks are resolved to the real
    // <Name>.app/Contents/MacOS/<bin> location.
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    bundle_path_for_exe(&exe)
}

/// Pure `<Name>.app/Contents/MacOS/<bin>` → `<Name>.app` derivation, split out
/// from [`macos_app_bundle_path`] so the path-walking is unit-testable without
/// depending on the real executable location.
#[cfg(target_os = "macos")]
fn bundle_path_for_exe(exe: &std::path::Path) -> Option<PathBuf> {
    let macos_dir = exe.parent()?; // <Name>.app/Contents/MacOS
    let contents = macos_dir.parent()?; // <Name>.app/Contents
    let bundle = contents.parent()?; // <Name>.app
    let looks_like_bundle = macos_dir.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app";
    looks_like_bundle.then(|| bundle.to_path_buf())
}

/// Routes a launch to the app bundle via LaunchServices `open`, so a new
/// invocation joins the already-running instance (windows grouped under one
/// runtime) instead of spawning its own process. macOS delivers the file-open
/// events to the running instance's `on_open_urls` handler.
///
/// Returns false when no `.app` bundle is available (raw binary), so the caller
/// can fall back to a detached process. Returns true once `open` has been
/// invoked — even if it errored — so the caller never double-launches.
#[cfg(target_os = "macos")]
fn launch_via_open(paths: &[PathBuf], activate: bool) -> bool {
    use std::process::Command;

    let Some(bundle) = macos_app_bundle_path() else {
        return false;
    };

    let mut command = Command::new("open");
    // -g opens without bringing the app to the front, honoring the
    // don't-steal-focus preference; the default activates it.
    if !activate {
        command.arg("-g");
    }
    command.arg("-a").arg(&bundle);
    // Files after the bundle are opened by the (possibly already running) app.
    command.args(paths);

    if let Err(err) = command.status() {
        eprintln!("failed to route launch to running instance: {err}");
    }
    true
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Parse command-line arguments
    let mut input_paths = Vec::new();
    // Per-launch detach override. None means "use the default" (detach when
    // launched from a terminal). Some(true) forces detach; Some(false) forces
    // foreground/blocking (--wait).
    let mut explicit_detach: Option<bool> = None;
    // Per-launch single-instance override. None means "use the preference".
    // Some(true) routes into the running app; Some(false) forces a fresh one.
    let mut explicit_single_instance: Option<bool> = None;
    // Internal: set on a re-launched child so it runs the GUI directly instead
    // of detaching again (loop guard). Not shown in --help.
    let mut no_detach = false;
    // Internal flag: the path of a temp file staged by a parent process during
    // the piped-stdin handoff (see below). Not shown in --help.
    let mut stdin_temp_file: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--version" | "-v" => {
                println!("velotype {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                println!(
                    "velotype {} - A block-based Markdown editor",
                    env!("CARGO_PKG_VERSION")
                );
                println!();
                println!("USAGE:");
                println!("    velotype [OPTIONS] [FILES...]");
                println!();
                println!("OPTIONS:");
                println!("    -v, --version    Print version information");
                println!("    -h, --help       Print this help message");
                println!(
                    "    -w, --wait       Block until the editor exits (default is non-blocking)"
                );
                println!(
                    "    -d, --detach     Launch non-blocking (default; forces detach with no tty)"
                );
                println!("    --background     Alias for --detach");
                println!(
                    "    --new-instance   Force a fresh process instead of reusing a running app"
                );
                println!(
                    "    --single-instance  Route into the already-running app (installed .app only)"
                );
                println!();
                println!("FILES:");
                println!("    One or more markdown files to open. If no files are specified");
                println!("    and markdown is piped in, it opens in a scratch window");
                println!("    (e.g. 'cat notes.md | velotype'). Otherwise opens an empty");
                println!("    document.");
                return;
            }
            // Non-blocking launch is the default; these flags request it
            // explicitly and force it even when stdin is not a terminal.
            // --background is a kept alias. Focus is governed solely by the
            // foreground_on_launch preference, not by these flags.
            "--detach" | "-d" | "--background" => {
                explicit_detach = Some(true);
            }
            // Opt back into blocking: run the editor in the foreground and wait
            // for it to exit (like 'subl -w'), useful as a $EDITOR.
            "--wait" | "-w" => {
                explicit_detach = Some(false);
            }
            // Route this launch into the already-running app (windows grouped),
            // or force a fresh process. Overrides the single_instance preference
            // for this launch only. Only effective for an installed .app bundle.
            "--single-instance" => {
                explicit_single_instance = Some(true);
            }
            "--new-instance" => {
                explicit_single_instance = Some(false);
            }
            // Internal loop guard: a re-launched child carries this so it runs
            // the GUI directly instead of detaching again. Not shown in --help.
            "--no-detach" => {
                no_detach = true;
            }
            // Stdin is auto-detected, so '-' is accepted but redundant. Keep it
            // as a no-op so 'cmd | velotype -' invocations stay valid.
            "-" => {}
            // Internal handoff flag: the next arg is a temp file holding markdown
            // a parent process drained from a stdin pipe. Treated like a file
            // launch but opened as an unsaved scratch window, then deleted.
            "--stdin-file" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--stdin-file requires a path");
                    std::process::exit(1);
                }
                stdin_temp_file = Some(PathBuf::from(&args[i]));
            }
            option if option.starts_with('-') => {
                eprintln!("Unknown option: {}", option);
                std::process::exit(1);
            }
            path => {
                input_paths.push(PathBuf::from(path));
            }
        }
        i += 1;
    }

    // Drain a stdin pipe once, up front, in whatever process the shell invoked.
    // A tty stdin would block waiting for input, so only read a non-terminal
    // stdin; a GUI launch (Finder, dock) has stdin on /dev/null which reads as
    // empty and is treated as "no pipe". Explicit file args win over the pipe,
    // and a --stdin-file child already has its content staged on disk.
    let piped_stdin: Option<String> = if input_paths.is_empty()
        && stdin_temp_file.is_none()
        && !std::io::stdin().is_terminal()
    {
        let mut buf = String::new();
        match std::io::stdin().read_to_string(&mut buf) {
            Ok(0) => None,
            Ok(_) => Some(buf),
            Err(err) => {
                eprintln!("failed to read markdown from stdin: {err}");
                None
            }
        }
    } else {
        None
    };

    // Decide whether to detach (non-blocking). Non-blocking is the default for
    // terminal launches; --wait forces blocking, --detach forces non-blocking.
    // A re-launched child (--no-detach) or a staged-stdin child always runs the
    // GUI directly, so it never detaches again (loop guard).
    let detach = if no_detach || stdin_temp_file.is_some() {
        false
    } else {
        // stderr is the most reliable "attached to a terminal" signal: stdin
        // may be piped and stdout redirected, but stderr usually is not.
        explicit_detach.unwrap_or_else(|| std::io::stderr().is_terminal())
    };

    // Load preferences once, up front, so the launch path (single-instance
    // routing, focus) and the GUI below both read the same values without
    // touching disk twice.
    let preferences = config::load_or_create_app_preferences().unwrap_or_else(|err| {
        eprintln!("failed to initialize app preferences: {err}");
        Default::default()
    });

    // Whether a new launch routes into an already-running instance instead of
    // spawning its own process. --new-instance / --single-instance override the
    // persisted preference for this launch only.
    let single_instance = explicit_single_instance.unwrap_or(preferences.single_instance);

    #[cfg(not(target_os = "macos"))]
    let _ = (detach, single_instance);

    // On macOS, detach from the terminal by handing the launch off to a GUI
    // process that outlives this one. The parent returns immediately, freeing
    // the shell.
    // TODO: Other platforms may also need to be adapted
    #[cfg(target_os = "macos")]
    if detach {
        // With single-instance on, route into the running app via LaunchServices
        // so windows group under one runtime. Piped stdin can't be handed to a
        // running instance that way, so it always spawns a fresh detached process
        // (scratch window); a raw binary with no .app bundle also falls back.
        if single_instance
            && piped_stdin.is_none()
            && launch_via_open(&input_paths, preferences.foreground_on_launch)
        {
            return;
        }
        relaunch_detached(&args, piped_stdin.as_deref());
        return;
    }

    // The GUI child reads the staged temp file and deletes it; otherwise use the
    // pipe drained above. A failed read falls through to an empty scratch window.
    let piped_markdown = match stdin_temp_file.as_ref() {
        Some(path) => {
            let content = std::fs::read_to_string(path);
            let _ = std::fs::remove_file(path);
            match content {
                Ok(markdown) => Some(markdown),
                Err(err) => {
                    eprintln!("failed to read staged stdin file: {err}");
                    None
                }
            }
        }
        None => piped_stdin,
    };

    #[cfg(target_os = "macos")]
    let (open_file_tx, mut open_file_rx) = mpsc::unbounded::<PathBuf>();
    #[cfg(target_os = "macos")]
    let open_file_requested = Arc::new(AtomicBool::new(false));

    let app = Application::new().with_assets(VelotypeAssets);

    #[cfg(target_os = "macos")]
    {
        let open_file_requested_for_callback = open_file_requested.clone();
        app.on_open_urls(move |urls| {
            for url in urls {
                let Some(path) = parse_file_url(&url) else {
                    continue;
                };
                open_file_requested_for_callback.store(true, Ordering::SeqCst);
                let _ = open_file_tx.unbounded_send(path);
            }
        });
    }

    app.run(move |cx: &mut App| {
        I18nManager::init_with_language_id(cx, &preferences.default_language_id);
        ThemeManager::init_with_theme_id(cx, &preferences.default_theme_id);
        config::EditorSettings::init(cx, preferences.show_table_headers);
        net::install_http_client(cx);
        init_editor(cx, &preferences.keybindings);
        // Whether the app comes to the foreground on launch is governed solely
        // by the persisted preference (no CLI override); --detach/--background
        // affect blocking, not focus.
        init_app_menu(cx, preferences.foreground_on_launch);

        #[cfg(target_os = "macos")]
        cx.spawn(async move |cx| {
            while let Some(path) = open_file_rx.next().await {
                let _ = cx.update(move |cx| {
                    if let Err(err) = app_menu::open_file_in_new_window(cx, &path) {
                        eprintln!("failed to open '{}': {err}", path.display());
                    }
                });
            }
        })
        .detach();

        // Markdown piped via stdin opens in a scratch window with no backing
        // path, the same as an empty startup document but pre-filled.
        let opened_piped_window = piped_markdown.is_some();
        if let Some(markdown) = piped_markdown {
            open_editor_window(cx, markdown, None);
        }

        if input_paths.is_empty() {
            if opened_piped_window {
                app_menu::install_menus(cx);
                cx.refresh_windows();
                return;
            }

            #[cfg(target_os = "macos")]
            {
                let startup_open = preferences.startup_open;
                let open_file_requested = open_file_requested.clone();
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(150))
                        .await;
                    if !open_file_requested.load(Ordering::SeqCst) {
                        let _ = cx.update(move |cx| open_startup_window(cx, startup_open));
                    }
                })
                .detach();
            }

            #[cfg(not(target_os = "macos"))]
            open_startup_window(cx, preferences.startup_open);

            return;
        }

        for path in &input_paths {
            let absolute_path = if path.is_absolute() {
                path.clone()
            } else {
                match std::env::current_dir() {
                    Ok(cwd) => cwd.join(path),
                    Err(_) => path.clone(),
                }
            };

            let markdown = match std::fs::read_to_string(&absolute_path) {
                Ok(content) => {
                    if let Err(err) = config::record_recent_file(&absolute_path) {
                        eprintln!("failed to update recent file history: {err}");
                    }
                    content
                }
                Err(err) => {
                    eprintln!(
                        "failed to read '{}': {err}. opened as empty document.",
                        absolute_path.display()
                    );
                    String::new()
                }
            };
            open_editor_window(cx, markdown, Some(absolute_path));
        }
        app_menu::install_menus(cx);
        cx.refresh_windows();
    });
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::bundle_path_for_exe;
    use std::path::{Path, PathBuf};

    #[test]
    fn bundle_path_resolves_from_installed_layout() {
        let exe = Path::new("/Applications/Velotype.app/Contents/MacOS/velotype");
        assert_eq!(
            bundle_path_for_exe(exe),
            Some(PathBuf::from("/Applications/Velotype.app"))
        );
    }

    #[test]
    fn bundle_path_is_none_for_raw_binary() {
        // A dev build (e.g. cargo run) is not inside a .app bundle.
        assert_eq!(
            bundle_path_for_exe(Path::new("/Users/dev/velotype/target/debug/velotype")),
            None
        );
    }

    #[test]
    fn bundle_path_is_none_when_marker_dirs_wrong() {
        // Right depth, wrong directory names must not be mistaken for a bundle.
        assert_eq!(
            bundle_path_for_exe(Path::new("/opt/Velotype.app/Resources/bin/velotype")),
            None
        );
    }
}
