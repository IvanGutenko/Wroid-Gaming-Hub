use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use dbus::blocking::{Connection, SyncConnection};
use dbus::channel::MatchingReceiver;
use dbus::message::MatchRule;

const DBUS_TIMEOUT: Duration = Duration::from_secs(5);
const LOOP_INTERVAL: Duration = Duration::from_millis(20);
/// How long the relay keeps trying to fullscreen the game surface after
/// session start (Android boot takes tens of seconds).
const FULLSCREEN_CLAIM_WINDOW: Duration = Duration::from_secs(240);
/// Pause between claim attempts while the surface has not focused yet.
const FULLSCREEN_CLAIM_RETRY: Duration = Duration::from_millis(1000);

pub(crate) struct KwinFocusRelay {
    socket_path: PathBuf,
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl KwinFocusRelay {
    pub(crate) fn start() -> Result<Self> {
        ensure_supported_desktop()?;
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .context("XDG_RUNTIME_DIR is unavailable")?;
        let relay_dir = runtime_dir.join("wroid");
        fs::create_dir_all(&relay_dir)
            .with_context(|| format!("failed to create {}", relay_dir.display()))?;
        fs::set_permissions(&relay_dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure {}", relay_dir.display()))?;

        let token = random_token()?;
        let socket_path = relay_dir.join(format!("focus-{token}.sock"));
        let script_path = relay_dir.join(format!("focus-{token}.js"));
        let plugin_name = format!("wroid-focus-{token}");
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("failed to bind {}", socket_path.display()))?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure {}", socket_path.display()))?;
        listener
            .set_nonblocking(true)
            .context("failed to configure the focus relay socket")?;

        let (stop_tx, stop_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_socket = socket_path.clone();
        let thread_handle = thread::spawn(move || {
            let startup_error = ready_tx.clone();
            let result = run_relay(listener, &script_path, &plugin_name, stop_rx, ready_tx);
            if let Err(error) = result {
                let _ = startup_error.send(Err(format!("{error:#}")));
                eprintln!("Wroid focus relay stopped: {error:#}");
            }
            let _ = fs::remove_file(script_path);
            let _ = fs::remove_file(thread_socket);
        });

        match ready_rx.recv_timeout(DBUS_TIMEOUT + Duration::from_secs(1)) {
            Ok(Ok(())) => Ok(Self {
                socket_path,
                stop: stop_tx,
                thread: Some(thread_handle),
            }),
            Ok(Err(message)) => {
                let _ = stop_tx.send(());
                let _ = thread_handle.join();
                Err(anyhow!(message))
            }
            Err(error) => {
                let _ = stop_tx.send(());
                let _ = thread_handle.join();
                Err(anyhow!("focus relay did not become ready: {error}"))
            }
        }
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for KwinFocusRelay {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_relay(
    listener: UnixListener,
    script_path: &Path,
    plugin_name: &str,
    stop: mpsc::Receiver<()>,
    ready: mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    let owner_pid = std::process::id();
    let callback_connection =
        SyncConnection::new_session().context("failed to connect to the desktop D-Bus")?;
    let callback_address = callback_connection.unique_name().to_string();
    let script = focus_script(&callback_address, owner_pid);
    fs::write(script_path, script)
        .with_context(|| format!("failed to write {}", script_path.display()))?;
    fs::set_permissions(script_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", script_path.display()))?;

    let kwin_connection =
        Connection::new_session().context("failed to connect to KWin over D-Bus")?;
    let kwin_proxy = kwin_connection.with_proxy("org.kde.KWin", "/Scripting", DBUS_TIMEOUT);
    let (script_id,): (i32,) = kwin_proxy
        .method_call(
            "org.kde.kwin.Scripting",
            "loadScript",
            (script_path.to_string_lossy().as_ref(), plugin_name),
        )
        .context("KWin rejected the focus protection script")?;
    if script_id < 0 {
        bail!("KWin could not load the focus protection script");
    }

    let script_proxy = kwin_connection.with_proxy(
        "org.kde.KWin",
        format!("/Scripting/Script{script_id}"),
        DBUS_TIMEOUT,
    );
    if let Err(error) = script_proxy.method_call::<(), _, _, _>("org.kde.kwin.Script", "run", ()) {
        let _: Result<(bool,), _> =
            kwin_proxy.method_call("org.kde.kwin.Scripting", "unloadScript", (plugin_name,));
        return Err(error).context("failed to start the KWin focus protection script");
    }

    let claimed_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let claimed_in_callback = claimed_flag.clone();
    let (focus_tx, focus_rx) = mpsc::channel();
    let _receiver = callback_connection.start_receive(
        MatchRule::new_method_call(),
        Box::new(move |message, _connection| {
            match message.member() {
                Some(member) if member == "focusChanged" => {
                    if let Ok((value, output)) = message.read2::<String, String>() {
                        let _ = focus_tx.send((
                            focus_event_is_owned(&value, owner_pid, Path::new("/proc")),
                            output,
                        ));
                    }
                }
                Some(member) if member == "claimResult" => {
                    claimed_in_callback.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                _ => {}
            }
            true
        }),
    );

    let _ = ready.send(Ok(()));
    let mut client: Option<UnixStream> = None;
    let mut last_focus = None;
    let mut last_output: Option<String> = None;
    // Boot-time fullscreen claim: the render surface appears tens of seconds
    // into Android boot, and the creation-time hook races window placement,
    // which used to leave the game maximized behind the Plasma panel. Until
    // the surface reports focus once, re-run a one-shot claim script every
    // second.
    let claim_path = script_path.with_file_name("claim.js");
    let claim_deadline = std::time::Instant::now() + FULLSCREEN_CLAIM_WINDOW;
    let mut next_claim = std::time::Instant::now();
    let relay_result = (|| -> Result<()> {
        while stop.try_recv().is_err() {
            callback_connection
                .process(LOOP_INTERVAL)
                .context("desktop D-Bus focus listener failed")?;
            accept_client(&listener, &mut client, last_focus, last_output.as_deref())?;
            while let Ok((focused, output)) = focus_rx.try_recv() {
                last_focus = Some(focused);
                last_output = Some(output);
                write_focus(&mut client, focused, last_output.as_deref());
            }
            let claim_done =
                claimed_flag.load(std::sync::atomic::Ordering::Relaxed) || last_focus == Some(true);
            if !claim_done
                && std::time::Instant::now() >= next_claim
                && std::time::Instant::now() < claim_deadline
            {
                next_claim = std::time::Instant::now() + FULLSCREEN_CLAIM_RETRY;
                let _ = run_fullscreen_claim(
                    &kwin_connection,
                    &claim_path,
                    owner_pid,
                    &format!("{plugin_name}_claim"),
                    &callback_address,
                );
            }
        }
        Ok(())
    })();

    let _: Result<(bool,), _> =
        kwin_proxy.method_call("org.kde.kwin.Scripting", "unloadScript", (plugin_name,));
    relay_result
}

fn accept_client(
    listener: &UnixListener,
    client: &mut Option<UnixStream>,
    last_focus: Option<bool>,
    last_output: Option<&str>,
) -> Result<()> {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .context("failed to configure focus client")?;
                // The KWin callback can lag behind socket startup while the
                // compositor is busy creating/fullscreening the Waydroid
                // surface. Always complete the handshake immediately and
                // fail closed until the first authoritative callback arrives.
                // Previously the worker timed out with EAGAIN, disabled focus
                // protection for the whole session, and kept input captured
                // even when another monitor gained focus.
                stream
                    .write_all(&focus_line(last_focus.unwrap_or(false), last_output))
                    .context("failed to initialize focus client")?;
                *client = Some(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("failed to accept focus client"),
        }
    }
}

fn write_focus(client: &mut Option<UnixStream>, focused: bool, output: Option<&str>) {
    if client
        .as_mut()
        .is_some_and(|stream| stream.write_all(&focus_line(focused, output)).is_err())
    {
        *client = None;
    }
}

/// One focus state line, followed by an optional `output <name>` line naming
/// the focused window's output so the session can anchor its cursor overlay.
fn focus_line(focused: bool, output: Option<&str>) -> Vec<u8> {
    let mut line = if focused {
        b"focused\n".to_vec()
    } else {
        b"unfocused\n".to_vec()
    };
    if let Some(name) = output.filter(|name| !name.is_empty()) {
        line.extend_from_slice(format!("output {name}\n").as_bytes());
    }
    line
}

fn focus_script(callback_address: &str, owner_pid: u32) -> String {
    format!(
        r#"
const OWNER_PID = {owner_pid};
function surfaceIdentity(window) {{
    if (window == null) {{
        return "other";
    }}
    const windowClass = String(window.resourceClass || "").toLowerCase();
    const windowName = String(window.resourceName || "").toLowerCase();
    const isWaydroid = value => value === "waydroid" || value.startsWith("waydroid.");
    if (isWaydroid(windowClass) || isWaydroid(windowName)) {{
        return "waydroid";
    }}
    const pid = Number(window.pid || 0);
    if (Number.isInteger(pid) && pid > 1) {{
        return "pid:" + String(pid);
    }}
    return "other";
}}

function outputName(window) {{
    if (window == null || window.output == null) {{
        return "";
    }}
    return String(window.output.name || "");
}}

// The direct Waydroid window opens maximized, which keeps the Plasma panel
// visible; a real fullscreen hides it. `fullScreen` is read-only in the
// scripting engine, but slotWindowFullScreen toggles the active window.
function isGameSurface(window) {{
    if (window == null) {{
        return false;
    }}
    const windowClass = String(window.resourceClass || "").toLowerCase();
    if (windowClass.startsWith("waydroid")) {{
        return true;
    }}
    // The direct-presentation render surface is owned by the session worker
    // itself (class "wroid", empty caption) — match it by process id so the
    // hub window (also class "wroid") is never captured.
    return Number(window.pid || 0) === OWNER_PID;
}}

function fullscreenGameWindow(window) {{
    if (!isGameSurface(window)) {{
        return;
    }}
    if (window.fullScreen || window.frameGeometry.width < 100) {{
        return;
    }}
    workspace.activeWindow = window;
    // slotWindowFullScreen toggles WHATEVER is active; if the activation did
    // not land yet, skipping beats accidentally fullscreening the user's
    // browser.
    if (workspace.activeWindow !== window) {{
        return;
    }}
    if (!window.fullScreen) {{
        workspace.slotWindowFullScreen();
    }}
}}

function claimExisting() {{
    for (const candidate of workspace.windowList()) {{
        if (isGameSurface(candidate)) {{
            fullscreenGameWindow(candidate);
        }}
    }}
}}

function reportFocus(window) {{
    callDBus("{callback_address}", "/", "", "focusChanged", surfaceIdentity(window), outputName(window));
    fullscreenGameWindow(window);
}}

workspace.windowActivated.connect(reportFocus);
// The render surface may appear only after Android finishes booting; grab,
// raise, and fullscreen it the moment it is created.
workspace.windowAdded.connect(fullscreenGameWindow);
claimExisting();
reportFocus(workspace.activeWindow);
"#
    )
}

/// One fullscreen-claim pass: activate and fullscreen the game surface if it
/// exists and is not already fullscreen.
fn run_fullscreen_claim(
    kwin_connection: &Connection,
    claim_path: &Path,
    owner_pid: u32,
    plugin_name: &str,
    callback_address: &str,
) -> Result<()> {
    let script = format!(
        r#"
for (const candidate of workspace.windowList()) {{
    const windowClass = String(candidate.resourceClass || "").toLowerCase();
    const owned = Number(candidate.pid || 0) === {owner_pid};
    if ((!windowClass.startsWith("waydroid") && !owned) || candidate.fullScreen) {{
        continue;
    }}
    if (candidate.frameGeometry.width < 100) {{
        continue;
    }}
    workspace.activeWindow = candidate;
    if (workspace.activeWindow !== candidate) {{
        continue;
    }}
    if (!candidate.fullScreen) {{
        workspace.slotWindowFullScreen();
    }}
    callDBus("{callback_address}", "/", "", "claimResult");
}}
"#
    );
    fs::write(claim_path, script)
        .with_context(|| format!("failed to write {}", claim_path.display()))?;
    fs::set_permissions(claim_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", claim_path.display()))?;

    let proxy = kwin_connection.with_proxy("org.kde.KWin", "/Scripting", DBUS_TIMEOUT);
    let _: Result<(bool,), _> =
        proxy.method_call("org.kde.kwin.Scripting", "unloadScript", (plugin_name,));
    let (script_id,): (i32,) = proxy
        .method_call(
            "org.kde.kwin.Scripting",
            "loadScript",
            (claim_path.to_string_lossy().as_ref(), plugin_name),
        )
        .context("KWin rejected the fullscreen claim script")?;
    if script_id < 0 {
        bail!("KWin could not load the fullscreen claim script");
    }
    let script_proxy = kwin_connection.with_proxy(
        "org.kde.KWin",
        format!("/Scripting/Script{script_id}"),
        DBUS_TIMEOUT,
    );
    let _ = script_proxy.method_call::<(), _, _, _>("org.kde.kwin.Script", "run", ());
    let _: Result<(bool,), _> =
        proxy.method_call("org.kde.kwin.Scripting", "unloadScript", (plugin_name,));
    Ok(())
}

fn focus_event_is_owned(value: &str, owner_pid: u32, proc_root: &Path) -> bool {
    if value == "waydroid" {
        return true;
    }
    let Some(pid) = value
        .strip_prefix("pid:")
        .and_then(|pid| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 1)
    else {
        return false;
    };
    process_descends_from(pid, owner_pid, proc_root)
}

fn process_descends_from(mut pid: u32, ancestor: u32, proc_root: &Path) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        let Ok(stat) = fs::read_to_string(proc_root.join(pid.to_string()).join("stat")) else {
            return false;
        };
        let Some(parent) = stat
            .rfind(')')
            .and_then(|end| stat.get(end + 1..))
            .and_then(|fields| fields.split_whitespace().nth(1))
            .and_then(|parent| parent.parse::<u32>().ok())
        else {
            return false;
        };
        if parent == pid {
            return false;
        }
        pid = parent;
    }
    false
}

fn ensure_supported_desktop() -> Result<()> {
    if std::env::var("KDE_SESSION_VERSION").as_deref() != Ok("6") {
        bail!("automatic focus protection currently requires KDE Plasma 6");
    }
    if !std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("kde")
    {
        bail!("automatic focus protection currently requires a KDE desktop session");
    }
    Ok(())
}

fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 12];
    fs::File::open("/dev/urandom")
        .context("failed to open the system random source")?
        .read_exact(&mut bytes)
        .context("failed to generate a focus relay token")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn client_handshake_fails_closed_before_kwin_reports_focus() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("focus.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = UnixStream::connect(&socket_path).unwrap();
        let mut accepted = None;

        accept_client(&listener, &mut accepted, None, None).unwrap();

        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        assert_eq!(line, "unfocused\n");
        assert!(accepted.is_some());
    }

    #[test]
    fn script_matches_waydroid_and_reports_process_identity() {
        let script = focus_script(":1.42", 4242);
        assert!(script.contains("window.resourceClass"));
        assert!(script.contains("window.resourceName"));
        assert!(script.contains("startsWith(\"waydroid.\")"));
        assert!(script.contains("window.pid"));
        assert!(script.contains("pid:"));
        assert!(script.contains("workspace.windowActivated.connect(reportFocus)"));
        assert!(script.contains("reportFocus(workspace.activeWindow)"));
        assert!(script.contains("callDBus(\":1.42\""));
        // Fullscreen-on-focus: the Waydroid window or the worker's own render
        // surface, only when not already fullscreen, toggled through the
        // active-window slot. New surfaces are claimed on creation and
        // existing ones at script start so a window that appeared during
        // Android boot is raised instead of hiding behind other windows.
        assert!(script.contains("function fullscreenGameWindow(window)"));
        assert!(script.contains("function isGameSurface(window)"));
        assert!(script.contains("Number(window.pid || 0) === OWNER_PID"));
        assert!(script.contains("workspace.windowAdded.connect(fullscreenGameWindow)"));
        assert!(script.contains("claimExisting();"));
        assert!(script.contains("workspace.slotWindowFullScreen()"));
    }

    #[test]
    fn focused_surface_is_limited_to_the_worker_process_tree() {
        let directory = tempfile::tempdir().unwrap();
        let proc_root = directory.path();
        for (pid, parent) in [(4200, 4100), (4100, 4000), (9000, 1)] {
            let process = proc_root.join(pid.to_string());
            fs::create_dir(&process).unwrap();
            fs::write(
                process.join("stat"),
                format!("{pid} (game surface) S {parent} 0 0 0 0"),
            )
            .unwrap();
        }

        assert!(focus_event_is_owned("pid:4200", 4000, proc_root));
        assert!(!focus_event_is_owned("pid:9000", 4000, proc_root));
        assert!(!focus_event_is_owned("pid:not-a-pid", 4000, proc_root));
        assert!(focus_event_is_owned("waydroid", 4000, proc_root));
        assert!(!focus_event_is_owned("other", 4000, proc_root));
    }

    #[test]
    fn focus_wire_protocol_is_unambiguous() {
        assert_eq!(focus_line(true, None), b"focused\n".to_vec());
        assert_eq!(focus_line(false, None), b"unfocused\n".to_vec());
        assert_eq!(
            focus_line(true, Some("DP-2")),
            b"focused\noutput DP-2\n".to_vec()
        );
        // The KWin script reports an empty name when the window has no
        // output; that must not emit a misleading empty output line.
        assert_eq!(focus_line(true, Some("")), b"focused\n".to_vec());
    }

    #[test]
    #[ignore = "requires a live KDE Plasma 6 session"]
    fn live_relay_reports_initial_focus() {
        let relay = KwinFocusRelay::start().unwrap();
        let stream = UnixStream::connect(relay.socket_path()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut state = String::new();
        reader.read_line(&mut state).unwrap();
        assert!(matches!(state.trim(), "focused" | "unfocused"));
    }
}
