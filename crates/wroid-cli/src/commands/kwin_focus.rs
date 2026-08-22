use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use dbus::blocking::{Connection, SyncConnection};
use dbus::channel::MatchingReceiver;
use dbus::message::MatchRule;

const DBUS_TIMEOUT: Duration = Duration::from_secs(5);
const LOOP_INTERVAL: Duration = Duration::from_millis(20);

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
    let script = focus_script(&callback_address);
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

    let (focus_tx, focus_rx) = mpsc::channel();
    let _receiver = callback_connection.start_receive(
        MatchRule::new_method_call(),
        Box::new(move |message, _connection| {
            if message
                .member()
                .is_some_and(|member| member == "focusChanged")
            {
                if let Some(value) = message.get1::<String>() {
                    let _ =
                        focus_tx.send(focus_event_is_owned(&value, owner_pid, Path::new("/proc")));
                }
            }
            true
        }),
    );

    let _ = ready.send(Ok(()));
    let mut client: Option<UnixStream> = None;
    let mut last_focus = None;
    let mut client_buffer = String::new();
    let relay_result = (|| -> Result<()> {
        while stop.try_recv().is_err() {
            callback_connection
                .process(LOOP_INTERVAL)
                .context("desktop D-Bus focus listener failed")?;
            accept_client(&listener, &mut client, last_focus)?;
            handle_client_commands(
                &mut client,
                &mut client_buffer,
                &kwin_connection,
                relay_dir(script_path),
                plugin_name,
            )?;
            while let Ok(focused) = focus_rx.try_recv() {
                last_focus = Some(focused);
                write_focus(&mut client, focused);
            }
        }
        Ok(())
    })();

    let _: Result<(bool,), _> =
        kwin_proxy.method_call("org.kde.kwin.Scripting", "unloadScript", (plugin_name,));
    relay_result
}

fn relay_dir(script_path: &Path) -> &Path {
    script_path.parent().unwrap_or_else(|| Path::new("."))
}

/// Read warp requests ("warp corner" / "warp x y") from the session client.
///
/// The mouse stays EVIOCGRABbed while the session owns input, which freezes
/// the host cursor sprite wherever it was; parking it in the corner of the
/// game window (and restoring it on release) keeps exactly one visible
/// cursor: the session-drawn one.
fn handle_client_commands(
    client: &mut Option<UnixStream>,
    buffer: &mut String,
    kwin_connection: &Connection,
    script_dir: &Path,
    plugin_prefix: &str,
) -> Result<()> {
    let Some(stream) = client.as_mut() else {
        return Ok(());
    };
    // Poll the client socket instead of arming a read timeout: a zero-length
    // timeout is rejected by the kernel and used to kill the whole relay.
    stream
        .set_nonblocking(true)
        .context("failed to configure focus client for warp requests")?;
    loop {
        let mut chunk = [0_u8; 256];
        match stream.read(&mut chunk) {
            Ok(0) => {
                *client = None;
                return Ok(());
            }
            Ok(read) => buffer.push_str(&String::from_utf8_lossy(&chunk[..read])),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(error) => {
                eprintln!("Wroid focus client read failed: {error}");
                *client = None;
                return Ok(());
            }
        }
    }
    while let Some(position) = buffer.find('\n') {
        let request = buffer[..position].trim().to_owned();
        buffer.drain(..=position);
        if request.is_empty() {
            continue;
        }
        if let Err(error) = warp_cursor(kwin_connection, script_dir, plugin_prefix, &request) {
            eprintln!("Wroid host cursor warp failed: {error:#}");
        }
    }
    Ok(())
}

/// Warp the host cursor with a one-shot KWin script. Best-effort: failures
/// only leave the cursor where it was.
fn warp_cursor(
    connection: &Connection,
    script_dir: &Path,
    plugin_prefix: &str,
    request: &str,
) -> Result<()> {
    let script = if request == "warp corner" {
        "(function() {\n\
          const active = workspace.activeWindow;\n\
          const output = active ? active.output : null;\n\
          const area = (output && output.geometry) ? output.geometry : workspace.geometry;\n\
          workspace.cursorPos = Qt.point(area.x + area.width - 2, area.y + area.height - 2);\n\
         })();\n"
            .to_owned()
    } else {
        let mut parts = request.split_whitespace();
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some("warp"), Some(x), Some(y), None) => {
                format!("workspace.cursorPos = Qt.point({x}, {y});\n")
            }
            _ => bail!("unsupported warp request: {request}"),
        }
    };

    let sequence = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let plugin_name = format!("{plugin_prefix}-warp-{sequence}");
    let script_path = script_dir.join(format!("{plugin_name}.js"));
    fs::write(&script_path, script)
        .with_context(|| format!("failed to write {}", script_path.display()))?;
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", script_path.display()))?;

    let result = (|| -> Result<()> {
        let kwin_proxy = connection.with_proxy("org.kde.KWin", "/Scripting", DBUS_TIMEOUT);
        let (script_id,): (i32,) = kwin_proxy
            .method_call(
                "org.kde.kwin.Scripting",
                "loadScript",
                (script_path.to_string_lossy().as_ref(), &plugin_name),
            )
            .context("KWin rejected the cursor warp script")?;
        if script_id < 0 {
            bail!("KWin could not load the cursor warp script");
        }
        let script_proxy = connection.with_proxy(
            "org.kde.KWin",
            format!("/Scripting/Script{script_id}"),
            DBUS_TIMEOUT,
        );
        script_proxy
            .method_call::<(), _, _, _>("org.kde.kwin.Script", "run", ())
            .context("failed to run the KWin cursor warp script")?;
        let _: Result<(bool,), _> =
            kwin_proxy.method_call("org.kde.kwin.Scripting", "unloadScript", (&plugin_name,));
        Ok(())
    })();
    let _ = fs::remove_file(&script_path);
    result
}

fn accept_client(
    listener: &UnixListener,
    client: &mut Option<UnixStream>,
    last_focus: Option<bool>,
) -> Result<()> {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .context("failed to configure focus client")?;
                if let Some(focused) = last_focus {
                    stream
                        .write_all(focus_line(focused))
                        .context("failed to initialize focus client")?;
                }
                *client = Some(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("failed to accept focus client"),
        }
    }
}

fn write_focus(client: &mut Option<UnixStream>, focused: bool) {
    if client
        .as_mut()
        .is_some_and(|stream| stream.write_all(focus_line(focused)).is_err())
    {
        *client = None;
    }
}

const fn focus_line(focused: bool) -> &'static [u8] {
    if focused {
        b"focused\n"
    } else {
        b"unfocused\n"
    }
}

fn focus_script(callback_address: &str) -> String {
    format!(
        r#"
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

function reportFocus(window) {{
    callDBus("{callback_address}", "/", "", "focusChanged", surfaceIdentity(window));
}}

workspace.windowActivated.connect(reportFocus);
reportFocus(workspace.activeWindow);
"#
    )
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
    fn script_matches_waydroid_and_reports_process_identity() {
        let script = focus_script(":1.42");
        assert!(script.contains("window.resourceClass"));
        assert!(script.contains("window.resourceName"));
        assert!(script.contains("startsWith(\"waydroid.\")"));
        assert!(script.contains("window.pid"));
        assert!(script.contains("pid:"));
        assert!(script.contains("workspace.windowActivated.connect(reportFocus)"));
        assert!(script.contains("reportFocus(workspace.activeWindow)"));
        assert!(script.contains("callDBus(\":1.42\""));
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
        assert_eq!(focus_line(true), b"focused\n");
        assert_eq!(focus_line(false), b"unfocused\n");
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
