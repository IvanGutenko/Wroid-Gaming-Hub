use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, sleep, JoinHandle};
use std::time::Duration;

const STATUS_ATTEMPTS: usize = 60;
const USER_READY_ATTEMPTS: usize = 240;
const STATUS_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_ANDROID_USER_ID: u32 = 0;
const WAYDROID_WIDTH_PROPERTY: &str = "persist.waydroid.width";
const WAYDROID_HEIGHT_PROPERTY: &str = "persist.waydroid.height";
const MAX_WAYDROID_DIMENSION: u32 = 9_999;
const GAMESCOPE_PATH: &str = "/usr/bin/gamescope";
const WAYDROID_PATH: &str = "/usr/bin/waydroid";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaydroidPresentation {
    Direct,
    Gamescope { width: u32, height: u32 },
}

pub fn presentation_for_game(
    launch_package: bool,
    gamescope_available: bool,
    width: u32,
    height: u32,
) -> WaydroidPresentation {
    // Direct presentation is the performance default: the Waydroid window is
    // composited once by the desktop compositor and presents at the monitor's
    // native refresh (measured 240 fps on a 240 Hz panel). The nested
    // Gamescope pipeline adds a second composition pass plus an FSR shader,
    // which costs frames, and its nested display reports a different refresh
    // rate to Android, which lowers the in-game FPS-limit options. It stays
    // available as an explicit opt-in for FSR upscaling scenarios.
    match std::env::var("WROID_PRESENTATION").as_deref() {
        Ok("direct") => WaydroidPresentation::Direct,
        Ok("gamescope") => {
            if launch_package && gamescope_available {
                WaydroidPresentation::Gamescope { width, height }
            } else {
                WaydroidPresentation::Direct
            }
        }
        _ => WaydroidPresentation::Direct,
    }
}

pub fn gamescope_is_available() -> bool {
    fs::metadata(GAMESCOPE_PATH)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn gamescope_arguments(width: u32, height: u32) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("-w"),
        OsString::from(width.to_string()),
        OsString::from("-h"),
        OsString::from(height.to_string()),
        OsString::from("-f"),
        // No `-g`: the session EVIOCGRABs the keyboard itself when captured,
        // and a compositor-level grab would keep Alt+Tab dead even after F12
        // releases input to the OS.
        OsString::from("--expose-wayland"),
        OsString::from("--force-windows-fullscreen"),
        // Hide the cursor sprite after ~1s without movement: in aim mode the
        // physical mouse is grabbed (cursor frozen) so it disappears, while in
        // UI cursor mode the moving pointer stays visible.
        OsString::from("-C"),
        OsString::from("1000"),
        OsString::from("-S"),
        OsString::from("fit"),
        OsString::from("-F"),
        OsString::from("fsr"),
        OsString::from("--sharpness"),
        OsString::from("5"),
    ];
    // Without `-r`, Gamescope's nested display reports its own default refresh
    // rate to clients; Android then hides every in-game FPS limit above it
    // (measured: a 240 Hz panel offered at most 120). `WROID_GAMESCOPE_REFRESH`
    // pins the nested display to the host panel's rate.
    if let Ok(refresh) = std::env::var("WROID_GAMESCOPE_REFRESH") {
        if !refresh.is_empty() && refresh.chars().all(|c| c.is_ascii_digit()) {
            arguments.push(OsString::from("-r"));
            arguments.push(OsString::from(refresh));
        }
    }
    arguments.extend([
        OsString::from("--"),
        OsString::from(WAYDROID_PATH),
        OsString::from("session"),
        OsString::from("start"),
    ]);
    arguments
}

#[derive(Debug, Clone)]
pub struct DesktopUser {
    name: String,
    home: PathBuf,
    runtime_dir: PathBuf,
    wayland_display: OsString,
    launch: DesktopUserLaunch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopUserLaunch {
    Current,
    Runuser,
}

impl DesktopUser {
    pub fn from_sudo_environment() -> io::Result<Self> {
        let name = std::env::var("SUDO_USER").map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "SUDO_USER is not set; run this integration tool with sudo from the desktop user session",
            )
        })?;
        let uid = std::env::var("SUDO_UID")
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "SUDO_UID is not set"))?
            .parse::<u32>()
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid SUDO_UID: {error}"),
                )
            })?;
        let home = home_directory(&name)?;
        let runtime_dir = PathBuf::from(format!("/run/user/{uid}"));
        if !runtime_dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "desktop runtime directory is missing: {}",
                    runtime_dir.display()
                ),
            ));
        }
        if !runtime_dir.join("bus").exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "desktop DBus socket is missing: {}/bus",
                    runtime_dir.display()
                ),
            ));
        }

        let wayland_display = detect_wayland_display(&runtime_dir)?;
        Ok(Self {
            name,
            home,
            runtime_dir,
            wayland_display,
            launch: DesktopUserLaunch::Runuser,
        })
    }

    pub fn from_current_environment() -> io::Result<Self> {
        let uid = effective_uid()?;
        if uid == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "current desktop-user discovery must not run as root",
            ));
        }
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "XDG_RUNTIME_DIR is unavailable for the desktop Waydroid session",
                )
            })?;
        let metadata = fs::metadata(&runtime_dir)?;
        if !metadata.is_dir() || metadata.uid() != uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "desktop runtime directory is not owned by the current user: {}",
                    runtime_dir.display()
                ),
            ));
        }
        if !runtime_dir.join("bus").exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "desktop DBus socket is missing: {}/bus",
                    runtime_dir.display()
                ),
            ));
        }
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unavailable"))?;
        let name = std::env::var("USER").unwrap_or_else(|_| uid.to_string());
        let wayland_display = std::env::var_os("WAYLAND_DISPLAY")
            .filter(|value| !value.is_empty())
            .map(Ok)
            .unwrap_or_else(|| detect_wayland_display(&runtime_dir))?;
        Ok(Self {
            name,
            home,
            runtime_dir,
            wayland_display,
            launch: DesktopUserLaunch::Current,
        })
    }

    pub fn from_session_environment() -> io::Result<Self> {
        if effective_uid()? == 0 {
            Self::from_sudo_environment()
        } else {
            Self::from_current_environment()
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn wayland_display(&self) -> &OsString {
        &self.wayland_display
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let arguments = arguments.iter().map(OsString::from).collect::<Vec<_>>();
        self.command_for_program("waydroid", &arguments)
    }

    fn session_command(&self, presentation: WaydroidPresentation) -> Command {
        match presentation {
            WaydroidPresentation::Direct => self.command(&["session", "start"]),
            WaydroidPresentation::Gamescope { width, height } => {
                self.command_for_program(GAMESCOPE_PATH, &gamescope_arguments(width, height))
            }
        }
    }

    fn command_for_program(&self, program: &str, arguments: &[OsString]) -> Command {
        let mut wayland = OsString::from("WAYLAND_DISPLAY=");
        wayland.push(&self.wayland_display);

        let mut command = match self.launch {
            DesktopUserLaunch::Current => {
                let mut command = Command::new(program);
                command.args(arguments);
                command
            }
            DesktopUserLaunch::Runuser => {
                let mut command = Command::new("runuser");
                command
                    .arg("-u")
                    .arg(&self.name)
                    .arg("--")
                    .arg("env")
                    .arg(format!("HOME={}", self.home.display()))
                    .arg(format!("XDG_RUNTIME_DIR={}", self.runtime_dir.display()))
                    .arg(format!(
                        "DBUS_SESSION_BUS_ADDRESS=unix:path={}/bus",
                        self.runtime_dir.display()
                    ))
                    .arg(&wayland)
                    .arg(program)
                    .args(arguments);
                command
            }
        };
        command
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/bus", self.runtime_dir.display()),
            )
            .env("WAYLAND_DISPLAY", &self.wayland_display);
        isolate_from_terminal_interrupts(&mut command);
        command
    }

    fn run(&self, arguments: &[&str]) -> io::Result<()> {
        self.output(arguments).map(|_| ())
    }

    fn output(&self, arguments: &[&str]) -> io::Result<Output> {
        let output = self.command(arguments).output()?;
        if output.status.success() {
            return Ok(output);
        }

        Err(io::Error::other(format!(
            "waydroid {} as desktop user {} failed\n{}",
            arguments.join(" "),
            self.name,
            combined_output(&output)
        )))
    }
}

fn effective_uid() -> io::Result<u32> {
    let status = fs::read_to_string("/proc/self/status")?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|uids| uids.split_whitespace().nth(1))
        .and_then(|uid| uid.parse::<u32>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "cannot read effective UID"))
}

pub struct DesktopWaydroidSession {
    child: Child,
    user: DesktopUser,
    presentation: WaydroidPresentation,
    active: bool,
    ready_users: Vec<u32>,
    readiness: Receiver<u32>,
    gamescope_displays: Receiver<String>,
    gamescope_wayland_display: Option<String>,
    output_threads: Vec<JoinHandle<()>>,
}

impl DesktopWaydroidSession {
    pub fn start_from_sudo_environment() -> io::Result<Self> {
        Self::start(DesktopUser::from_sudo_environment()?)
    }

    pub fn start(user: DesktopUser) -> io::Result<Self> {
        Self::start_presented(user, WaydroidPresentation::Direct)
    }

    pub fn start_presented(
        user: DesktopUser,
        presentation: WaydroidPresentation,
    ) -> io::Result<Self> {
        println!(
            "Starting Waydroid session as desktop user {} on {}...",
            user.name(),
            user.wayland_display().to_string_lossy()
        );
        match presentation {
            WaydroidPresentation::Direct => {
                println!("Game presentation: direct Waydroid window.");
            }
            WaydroidPresentation::Gamescope { width, height } => {
                println!(
                    "Game presentation: Gamescope fullscreen with FSR ({width}x{height} render target)."
                );
            }
        }
        let mut child = user
            .session_command(presentation)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (readiness, gamescope_displays, output_threads) =
            match capture_session_output(&mut child) {
                Ok(capture) => capture,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
        let mut session = Self {
            child,
            user,
            presentation,
            active: true,
            ready_users: Vec::new(),
            readiness,
            gamescope_displays,
            gamescope_wayland_display: None,
            output_threads,
        };

        if let Err(error) = session.wait_until_running() {
            let _ = session.stop();
            return Err(error);
        }
        Ok(session)
    }

    pub fn user(&self) -> &DesktopUser {
        &self.user
    }

    pub fn gamescope_wayland_display(&mut self) -> io::Result<Option<String>> {
        if self.presentation == WaydroidPresentation::Direct {
            return Ok(None);
        }
        if let Some(display) = self.gamescope_wayland_display.as_ref() {
            return Ok(Some(display.clone()));
        }

        for _ in 0..STATUS_ATTEMPTS {
            match self.gamescope_displays.recv_timeout(STATUS_INTERVAL) {
                Ok(display) => {
                    self.gamescope_wayland_display = Some(display.clone());
                    return Ok(Some(display));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(status) = self.child.try_wait()? {
                        return Err(io::Error::other(format!(
                            "Gamescope exited before publishing its nested Wayland display: {status}"
                        )));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "Gamescope output closed before publishing its nested Wayland display",
                    ));
                }
            }
        }

        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Gamescope did not publish its nested Wayland display",
        ))
    }

    pub fn show_full_ui(&mut self) -> io::Result<()> {
        self.wait_until_android_user_ready(DEFAULT_ANDROID_USER_ID)?;
        self.user.run(&["show-full-ui"])
    }

    pub fn launch_package(&self, package_name: &str) -> io::Result<()> {
        if package_name.trim().is_empty()
            || package_name.chars().any(|character| {
                !(character.is_ascii_alphanumeric() || matches!(character, '_' | '.'))
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid Android package name: {package_name}"),
            ));
        }
        self.user.run(&["app", "launch", package_name])
    }

    /// Persist the selected Waydroid render size.
    ///
    /// Returns `true` when the session must be restarted for the new properties
    /// to take effect.
    pub fn configure_resolution(&self, width: u32, height: u32) -> io::Result<bool> {
        configure_resolution_properties(&self.user, width, height)
    }

    pub fn confirm_resolution(&self, width: u32, height: u32) -> io::Result<()> {
        confirm_resolution_properties(&self.user, width, height)
    }

    pub fn wait_until_android_ready(&mut self) -> io::Result<()> {
        self.wait_until_android_user_ready(DEFAULT_ANDROID_USER_ID)
    }

    pub fn restart(&mut self) -> io::Result<()> {
        self.stop()?;
        let replacement = Self::start_presented(self.user.clone(), self.presentation)?;
        *self = replacement;
        Ok(())
    }

    pub fn wait_until_android_user_ready(&mut self, user_id: u32) -> io::Result<()> {
        if self.ready_users.contains(&user_id) {
            return Ok(());
        }

        for _ in 0..USER_READY_ATTEMPTS {
            if let Some(status) = self.child.try_wait()? {
                return Err(io::Error::other(format!(
                    "Waydroid user session exited before Android user {user_id} became ready: {status}"
                )));
            }

            match self.readiness.recv_timeout(STATUS_INTERVAL) {
                Ok(ready_user) => {
                    if !self.ready_users.contains(&ready_user) {
                        self.ready_users.push(ready_user);
                    }
                    if ready_user == user_id {
                        println!(
                            "Android with user {user_id} is ready (captured from Waydroid session output)."
                    );
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        format!(
                            "Waydroid session output closed before Android user {user_id} became ready"
                        ),
                    ));
                }
            }
        }

        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("Waydroid did not report Android user {user_id} readiness before UI launch"),
        ))
    }

    pub fn stop(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }

        let output = self.user.command(&["session", "stop"]).output()?;
        let user_stop_error = (!output.status.success()).then(|| {
            io::Error::other(format!(
                "waydroid session stop failed\n{}",
                combined_output(&output)
            ))
        });

        let mut last_status = String::new();
        for _ in 0..STATUS_ATTEMPTS {
            last_status = waydroid_status()?;
            if waydroid_is_stopped(&last_status) {
                self.active = false;
                self.reap_child_and_output();
                return match user_stop_error {
                    Some(error) => Err(error),
                    None => Ok(()),
                };
            }
            sleep(STATUS_INTERVAL);
        }

        let fallback = run_waydroid(&["container", "stop"]);
        self.active = false;
        let _ = self.child.kill();
        self.reap_child_and_output();
        fallback?;
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("Waydroid container did not stop cleanly\n{last_status}"),
        ))
    }

    fn wait_until_running(&mut self) -> io::Result<()> {
        let mut last_status = String::new();
        for _ in 0..STATUS_ATTEMPTS {
            if let Some(status) = self.child.try_wait()? {
                return Err(io::Error::other(format!(
                    "Waydroid user session exited before the container started: {status}\n{last_status}"
                )));
            }

            last_status = waydroid_status()?;
            if waydroid_container_started(&last_status) {
                println!(
                    "Waydroid container is {}.",
                    container_state(&last_status).unwrap_or("READY")
                );
                return Ok(());
            }
            sleep(STATUS_INTERVAL);
        }

        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("Waydroid container did not reach RUNNING state\n{last_status}"),
        ))
    }

    fn reap_child_and_output(&mut self) {
        let _ = self.child.wait();
        for output_thread in self.output_threads.drain(..) {
            let _ = output_thread.join();
        }
    }
}

impl Drop for DesktopWaydroidSession {
    fn drop(&mut self) {
        if self.active {
            let _ = self.stop();
        }
    }
}

pub fn ensure_root(tool_name: &str) -> io::Result<()> {
    if effective_uid()? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{tool_name} requires root; run it with sudo"),
        ));
    }
    Ok(())
}

pub fn ensure_container_stopped() -> io::Result<()> {
    let status = waydroid_status()?;
    ensure_container_stopped_status(&status)
}

pub(crate) fn ensure_container_stopped_status(status: &str) -> io::Result<()> {
    if container_state(status) == Some("RUNNING") {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Waydroid container is running. Stop it first with: waydroid session stop",
        ));
    }
    if session_state(status) == Some("RUNNING") && container_state(status) == Some("FROZEN") {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Waydroid session is already running but the container is FROZEN. Recover with:\n  sudo target/debug/wroid-native-keyboard --cleanup\n  waydroid session stop\n  sudo systemctl restart waydroid-container",
        ));
    }
    Ok(())
}

pub fn wait_for_android_boot_completed() -> io::Result<()> {
    let mut last_output = String::new();
    for _ in 0..STATUS_ATTEMPTS {
        let output =
            waydroid_command(&["shell", "--", "getprop", "sys.boot_completed"]).output()?;
        last_output = combined_output(&output);
        if output.status.success() && last_output.trim() == "1" {
            println!("Android boot_completed=1.");
            return Ok(());
        }
        sleep(STATUS_INTERVAL);
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("Android did not report sys.boot_completed=1\n{last_output}"),
    ))
}

pub fn wait_for_android_display_size(width: u32, height: u32) -> io::Result<()> {
    let mut last_output = String::new();
    for _ in 0..STATUS_ATTEMPTS {
        let output = waydroid_command(&["shell", "--", "wm", "size"]).output()?;
        last_output = combined_output(&output);
        if output.status.success()
            && parse_android_display_size(&last_output) == Some((width, height))
        {
            println!("Android display size confirmed at {width}x{height}.");
            return Ok(());
        }
        sleep(STATUS_INTERVAL);
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("Android display size did not become {width}x{height}\n{last_output}"),
    ))
}

pub fn wait_for_android_input_device(device_name: &str) -> io::Result<()> {
    let mut last_output = String::new();
    for _ in 0..STATUS_ATTEMPTS {
        let output = waydroid_command(&["shell", "--", "getevent", "-pl"]).output()?;
        last_output = combined_output(&output);
        if output.status.success() && last_output.contains(device_name) {
            return Ok(());
        }
        sleep(STATUS_INTERVAL);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "Android getevent did not list {device_name}; device bridge is not active\n{}",
            tail_for_error(&last_output)
        ),
    ))
}

/// Verify that Android's InputReader registered the bridged touchscreen.
///
/// `getevent -pl` only proves the event node is visible inside the container;
/// `dumpsys input` proves system_server could actually open it. A device the
/// InputReader rejects (typically because the node still carries the host
/// input GID instead of AID_INPUT) stays EventHub-listed but never delivers
/// a single touch — this check turns that silent failure into an error.
pub fn wait_for_android_input_reader(device_name: &str) -> io::Result<()> {
    let mut last_output = String::new();
    for _ in 0..STATUS_ATTEMPTS {
        let output = waydroid_command(&["shell", "--", "dumpsys", "input"]).output()?;
        last_output = combined_output(&output);
        if output.status.success() && dumpsys_lists_input_reader_device(&last_output, device_name) {
            return Ok(());
        }
        sleep(STATUS_INTERVAL);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "Android InputReader did not register {device_name}; the event node is visible but \
             unreadable for system_server (expected host group 1004/AID_INPUT, mode 0660)\n{}",
            tail_for_error(&last_output)
        ),
    ))
}

/// Check that `dumpsys input` output lists the device inside an InputReader
/// section, not only under EventHub. When the dump carries no recognizable
/// section headers at all (unexpected format), fall back to a plain mention
/// so an unknown format cannot block a healthy session; a dump that does
/// show EventHub but no reader section means the device is genuinely not
/// registered.
pub(crate) fn dumpsys_lists_input_reader_device(dump: &str, device_name: &str) -> bool {
    for section in ["Input Reader", "InputReader"] {
        if let Some(reader_state) = dump.split(section).nth(1) {
            return reader_state.contains(device_name);
        }
    }
    !dump.contains("Event Hub") && dump.contains(device_name)
}

/// Keep error output bounded: `dumpsys input` can exceed tens of kilobytes
/// while only its tail matters for triage.
fn tail_for_error(output: &str) -> String {
    const MAX_ERROR_CHARS: usize = 4 * 1024;
    let char_count = output.chars().count();
    if char_count <= MAX_ERROR_CHARS {
        return output.to_owned();
    }
    output
        .chars()
        .skip(char_count - MAX_ERROR_CHARS)
        .collect::<String>()
}

pub fn spawn_android_getevent_trace(event_node: &Path) -> io::Result<Child> {
    let event_path = event_node.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "event node path is not UTF-8")
    })?;
    let mut command = waydroid_command(&["shell", "--", "getevent", "-lt", event_path]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
}

pub fn stop_child(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    child.wait()?;
    Ok(())
}

fn android_user_ready_event(line: &str) -> Option<u32> {
    let (_, event) = line.split_once("Android with user ")?;
    event
        .trim_end()
        .strip_suffix(" is ready")?
        .parse::<u32>()
        .ok()
}

fn gamescope_wayland_display_event(line: &str) -> Option<String> {
    let (_, display) = line.split_once("Running compositor on wayland display ")?;
    let display = display
        .trim()
        .strip_prefix(char::from(39))?
        .strip_suffix(char::from(39))?;
    (!display.is_empty()).then(|| display.to_owned())
}

/// Session event pipes produced by [`capture_session_output`]: the Android
/// user id once boot completes, every nested Gamescope Wayland display name,
/// and the forwarding threads (kept for implicit join on drop).
type SessionOutputPipes = (Receiver<u32>, Receiver<String>, Vec<JoinHandle<()>>);

fn capture_session_output(child: &mut Child) -> io::Result<SessionOutputPipes> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Waydroid session stdout pipe is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("Waydroid session stderr pipe is unavailable"))?;
    let (readiness_sender, readiness) = mpsc::channel();
    let stderr_readiness_sender = readiness_sender.clone();
    let (gamescope_sender, gamescope_displays) = mpsc::channel();
    let stderr_gamescope_sender = gamescope_sender.clone();
    let stdout_thread = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let output = io::stdout();
        let _ = forward_session_output(
            reader,
            output,
            Some(readiness_sender),
            Some(gamescope_sender),
        );
    });
    let stderr_thread = thread::spawn(move || {
        let reader = BufReader::new(stderr);
        let output = io::stderr();
        let _ = forward_session_output(
            reader,
            output,
            Some(stderr_readiness_sender),
            Some(stderr_gamescope_sender),
        );
    });
    Ok((
        readiness,
        gamescope_displays,
        vec![stdout_thread, stderr_thread],
    ))
}

fn forward_session_output<R, W>(
    mut reader: R,
    mut writer: W,
    readiness: Option<Sender<u32>>,
    gamescope_displays: Option<Sender<String>>,
) -> io::Result<()>
where
    R: BufRead,
    W: Write,
{
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        writer.write_all(&line)?;
        writer.flush()?;
        if let (Some(sender), Some(user_id)) = (
            readiness.as_ref(),
            android_user_ready_event(&String::from_utf8_lossy(&line)),
        ) {
            let _ = sender.send(user_id);
        }
        if let (Some(sender), Some(display)) = (
            gamescope_displays.as_ref(),
            gamescope_wayland_display_event(&String::from_utf8_lossy(&line)),
        ) {
            let _ = sender.send(display);
        }
    }
}

fn waydroid_status() -> io::Result<String> {
    let output = waydroid_command(&["status"]).output()?;
    Ok(combined_output(&output))
}

fn status_field<'a>(status: &'a str, field: &str) -> Option<&'a str> {
    status.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == field).then_some(value.trim())
    })
}

fn session_state(status: &str) -> Option<&str> {
    status_field(status, "Session")
}

fn container_state(status: &str) -> Option<&str> {
    status_field(status, "Container")
}

fn waydroid_container_started(status: &str) -> bool {
    matches!(container_state(status), Some("RUNNING" | "FROZEN"))
}

fn waydroid_is_stopped(status: &str) -> bool {
    match container_state(status) {
        Some("STOPPED") => true,
        Some("RUNNING") => false,
        _ => session_state(status) == Some("STOPPED"),
    }
}

fn run_waydroid(arguments: &[&str]) -> io::Result<()> {
    let output = waydroid_command(arguments).output()?;
    if output.status.success() {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "waydroid {} failed\n{}",
        arguments.join(" "),
        combined_output(&output)
    )))
}

fn waydroid_command(arguments: &[&str]) -> Command {
    let mut command = Command::new("waydroid");
    command.args(arguments);
    isolate_from_terminal_interrupts(&mut command);
    command
}

fn isolate_from_terminal_interrupts(command: &mut Command) {
    command.process_group(0);
}

fn combined_output(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text
}

trait WaydroidPropertyControl {
    fn get_property(&self, key: &str) -> io::Result<String>;
    fn set_property(&self, key: &str, value: &str) -> io::Result<()>;
}

impl WaydroidPropertyControl for DesktopUser {
    fn get_property(&self, key: &str) -> io::Result<String> {
        let output = self.output(&["prop", "get", key])?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn set_property(&self, key: &str, value: &str) -> io::Result<()> {
        self.run(&["prop", "set", key, value])
    }
}

fn configure_resolution_properties<C: WaydroidPropertyControl>(
    control: &C,
    width: u32,
    height: u32,
) -> io::Result<bool> {
    if width == 0
        || height == 0
        || width > MAX_WAYDROID_DIMENSION
        || height > MAX_WAYDROID_DIMENSION
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Waydroid render size must be within 1..={MAX_WAYDROID_DIMENSION}; got {width}x{height}"
            ),
        ));
    }

    let original_width = control.get_property(WAYDROID_WIDTH_PROPERTY)?;
    let original_height = control.get_property(WAYDROID_HEIGHT_PROPERTY)?;
    let target_width = width.to_string();
    let target_height = height.to_string();
    if original_width == target_width && original_height == target_height {
        println!("Waydroid render resolution is already {width}x{height}.");
        return Ok(false);
    }

    let update = (|| -> io::Result<()> {
        if original_width != target_width {
            control.set_property(WAYDROID_WIDTH_PROPERTY, &target_width)?;
        }
        if original_height != target_height {
            control.set_property(WAYDROID_HEIGHT_PROPERTY, &target_height)?;
        }
        let saved_width = control.get_property(WAYDROID_WIDTH_PROPERTY)?;
        let saved_height = control.get_property(WAYDROID_HEIGHT_PROPERTY)?;
        if saved_width != target_width || saved_height != target_height {
            return Err(io::Error::other(format!(
                "Waydroid did not persist render size {width}x{height}; saved values are '{saved_width}'x'{saved_height}'"
            )));
        }
        Ok(())
    })();

    if let Err(update_error) = update {
        let rollback = restore_resolution_properties(control, &original_width, &original_height);
        return Err(match rollback {
            Ok(()) => io::Error::new(
                update_error.kind(),
                format!("failed to configure Waydroid render size: {update_error}"),
            ),
            Err(rollback_error) => io::Error::other(format!(
                "failed to configure Waydroid render size: {update_error}; rollback also failed: {rollback_error}"
            )),
        });
    }

    println!(
        "Configured Waydroid render resolution {width}x{height}; restarting the Android session once."
    );
    Ok(true)
}

fn confirm_resolution_properties<C: WaydroidPropertyControl>(
    control: &C,
    width: u32,
    height: u32,
) -> io::Result<()> {
    let reported_width = control.get_property(WAYDROID_WIDTH_PROPERTY)?;
    let reported_height = control.get_property(WAYDROID_HEIGHT_PROPERTY)?;
    if reported_width == width.to_string() && reported_height == height.to_string() {
        println!("Waydroid render resolution confirmed at {width}x{height}.");
        return Ok(());
    }

    Err(io::Error::other(format!(
        "Waydroid render resolution mismatch: requested {width}x{height}, reported {reported_width}x{reported_height}"
    )))
}

fn restore_resolution_properties<C: WaydroidPropertyControl>(
    control: &C,
    width: &str,
    height: &str,
) -> io::Result<()> {
    let width_result = control.set_property(WAYDROID_WIDTH_PROPERTY, width);
    let height_result = control.set_property(WAYDROID_HEIGHT_PROPERTY, height);
    match (width_result, height_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(width_error), Err(height_error)) => Err(io::Error::other(format!(
            "width restore failed: {width_error}; height restore failed: {height_error}"
        ))),
    }
}

fn parse_android_display_size(output: &str) -> Option<(u32, u32)> {
    output
        .lines()
        .rev()
        .find_map(|line| parse_size_token(line.trim()))
}

fn parse_size_token(line: &str) -> Option<(u32, u32)> {
    let token = line.split_whitespace().last()?;
    let (width, height) = token.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

fn home_directory(user: &str) -> io::Result<PathBuf> {
    let passwd = fs::read_to_string("/etc/passwd")?;
    passwd
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let _password = fields.next()?;
            let _uid = fields.next()?;
            let _gid = fields.next()?;
            let _gecos = fields.next()?;
            let home = fields.next()?;
            (name == user).then(|| PathBuf::from(home))
        })
        .next()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("desktop user {user} is missing from /etc/passwd"),
            )
        })
}

fn detect_wayland_display(runtime_dir: &Path) -> io::Result<OsString> {
    if let Some(display) = std::env::var_os("WAYLAND_DISPLAY") {
        if runtime_dir.join(&display).exists() {
            return Ok(display);
        }
    }

    let mut candidates = fs::read_dir(runtime_dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let is_wayland = name.to_string_lossy().starts_with("wayland-");
            let is_socket = entry
                .file_type()
                .map(|file_type| file_type.is_socket())
                .unwrap_or(false);
            (is_wayland && is_socket).then_some(name)
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.into_iter().next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no Wayland socket found in {}", runtime_dir.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::io::Cursor;

    use super::*;

    #[test]
    fn dumpsys_reader_check_is_strict_about_known_formats() {
        // EventHub-only listing: the format is recognized, so a missing
        // reader section correctly means "not registered".
        let event_hub_only = "Event Hub State\n  1: Wroid Gaming Touchscreen\n";
        assert!(!dumpsys_lists_input_reader_device(
            event_hub_only,
            "Wroid Gaming Touchscreen"
        ));

        let registered = "Event Hub State\n  1: Wroid Gaming Touchscreen\n\
            Input Reader State\n  Device 1: Wroid Gaming Touchscreen\n";
        assert!(dumpsys_lists_input_reader_device(
            registered,
            "Wroid Gaming Touchscreen"
        ));
    }

    #[test]
    fn dumpsys_reader_check_is_lenient_about_unknown_formats() {
        // A dump without any recognizable section headers is an unexpected
        // format, not proof of a missing registration; a plain mention is
        // accepted so the check cannot block a healthy session.
        let headerless = "  1: Wroid Gaming Touchscreen\n";
        assert!(dumpsys_lists_input_reader_device(
            headerless,
            "Wroid Gaming Touchscreen"
        ));
    }

    #[test]
    fn error_tails_are_bounded() {
        let short = "abc";
        assert_eq!(tail_for_error(short), "abc");

        let long = "x".repeat(10_000);
        let tail = tail_for_error(&long);
        assert_eq!(tail.chars().count(), 4 * 1024);
        assert!(tail.chars().all(|c| c == 'x'));
    }

    #[test]
    fn gamescope_arguments_preserve_the_render_preset_and_aspect_ratio() {
        assert_eq!(
            gamescope_arguments(1280, 720),
            [
                "-w",
                "1280",
                "-h",
                "720",
                "-f",
                "--expose-wayland",
                "--force-windows-fullscreen",
                "-C",
                "1000",
                "-S",
                "fit",
                "-F",
                "fsr",
                "--sharpness",
                "5",
                "--",
                "/usr/bin/waydroid",
                "session",
                "start",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn gamescope_arguments_append_refresh_rate_override() {
        // SAFETY: single-threaded test body writing a test-only variable.
        unsafe { std::env::set_var("WROID_GAMESCOPE_REFRESH", "240") };
        let arguments = gamescope_arguments(1280, 720);
        let position = arguments
            .iter()
            .position(|argument| argument == "--")
            .expect("separator");
        assert!(arguments[..position]
            .windows(2)
            .any(|pair| pair[0] == *"-r" && pair[1] == *"240"));
        unsafe { std::env::remove_var("WROID_GAMESCOPE_REFRESH") };
    }

    #[test]
    fn direct_presentation_is_the_default_and_gamescope_is_opt_in() {
        // SAFETY: single-threaded test body writing a test-only variable.
        unsafe { std::env::remove_var("WROID_PRESENTATION") };
        // Performance default: no env override means Direct even when
        // Gamescope could run.
        assert_eq!(
            presentation_for_game(true, true, 1280, 720),
            WaydroidPresentation::Direct
        );
        unsafe { std::env::set_var("WROID_PRESENTATION", "gamescope") };
        assert_eq!(
            presentation_for_game(true, true, 1280, 720),
            WaydroidPresentation::Gamescope {
                width: 1280,
                height: 720
            }
        );
        assert_eq!(
            presentation_for_game(false, true, 1280, 720),
            WaydroidPresentation::Direct
        );
        assert_eq!(
            presentation_for_game(true, false, 1280, 720),
            WaydroidPresentation::Direct
        );
        unsafe { std::env::set_var("WROID_PRESENTATION", "direct") };
        assert_eq!(
            presentation_for_game(true, true, 1280, 720),
            WaydroidPresentation::Direct
        );
        unsafe { std::env::remove_var("WROID_PRESENTATION") };
    }

    #[derive(Default)]
    struct FakeProperties {
        values: RefCell<BTreeMap<String, String>>,
        writes: RefCell<Vec<(String, String)>>,
        fail_once: RefCell<Option<String>>,
        ignore_once: RefCell<Option<String>>,
    }

    impl FakeProperties {
        fn with_size(width: &str, height: &str) -> Self {
            Self {
                values: RefCell::new(BTreeMap::from([
                    (WAYDROID_WIDTH_PROPERTY.to_owned(), width.to_owned()),
                    (WAYDROID_HEIGHT_PROPERTY.to_owned(), height.to_owned()),
                ])),
                ..Self::default()
            }
        }
    }

    impl WaydroidPropertyControl for FakeProperties {
        fn get_property(&self, key: &str) -> io::Result<String> {
            Ok(self.values.borrow().get(key).cloned().unwrap_or_default())
        }

        fn set_property(&self, key: &str, value: &str) -> io::Result<()> {
            self.writes
                .borrow_mut()
                .push((key.to_owned(), value.to_owned()));
            if self.fail_once.borrow().as_deref() == Some(key) {
                self.fail_once.borrow_mut().take();
                return Err(io::Error::other("synthetic property failure"));
            }
            if self.ignore_once.borrow().as_deref() == Some(key) {
                self.ignore_once.borrow_mut().take();
                return Ok(());
            }
            self.values
                .borrow_mut()
                .insert(key.to_owned(), value.to_owned());
            Ok(())
        }
    }

    #[test]
    fn current_user_waydroid_command_does_not_use_runuser() {
        let user = DesktopUser {
            name: "player".to_owned(),
            home: PathBuf::from("/home/player"),
            runtime_dir: PathBuf::from("/run/user/1000"),
            wayland_display: OsString::from("wayland-0"),
            launch: DesktopUserLaunch::Current,
        };

        let command = user.command(&["session", "start"]);

        assert_eq!(command.get_program(), "waydroid");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["session", "start"].map(std::ffi::OsStr::new)
        );
    }

    #[test]
    fn detects_explicitly_stopped_container() {
        assert!(waydroid_is_stopped(
            "Session:\tSTOPPED\nContainer:\tSTOPPED\n"
        ));
    }

    #[test]
    fn detects_stopped_waydroid_without_container_field() {
        assert!(waydroid_is_stopped("Session:\tSTOPPED\n"));
    }

    #[test]
    fn running_container_wins_over_stopped_session() {
        assert!(!waydroid_is_stopped(
            "Session:\tSTOPPED\nContainer:\tRUNNING\n"
        ));
    }

    #[test]
    fn running_or_frozen_container_satisfies_startup_gate() {
        assert!(waydroid_container_started(
            "Session:\tRUNNING\nContainer:\tRUNNING\n"
        ));
        assert!(waydroid_container_started(
            "Session:\tRUNNING\nContainer:\tFROZEN\n"
        ));
        assert!(!waydroid_container_started("Session:\tSTOPPED\n"));
    }

    #[test]
    fn rejects_running_container_before_native_setup() {
        let error = ensure_container_stopped_status("Session:\tRUNNING\nContainer:\tRUNNING\n")
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("container is running"));
    }

    #[test]
    fn rejects_frozen_container_before_native_setup() {
        let error =
            ensure_container_stopped_status("Session:\tRUNNING\nContainer:\tFROZEN\n").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("container is FROZEN"));
        assert!(error
            .to_string()
            .contains("wroid-native-keyboard --cleanup"));
        assert!(error
            .to_string()
            .contains("systemctl restart waydroid-container"));
    }

    #[test]
    fn allows_stopped_container_before_native_setup() {
        ensure_container_stopped_status("Session:\tSTOPPED\nContainer:\tSTOPPED\n").unwrap();
    }

    #[test]
    fn unchanged_render_resolution_does_not_request_restart() {
        let properties = FakeProperties::with_size("1600", "900");

        assert!(!configure_resolution_properties(&properties, 1600, 900).unwrap());
        assert!(properties.writes.borrow().is_empty());
    }

    #[test]
    fn changed_render_resolution_is_persisted_and_requests_restart() {
        let properties = FakeProperties::with_size("1920", "1080");

        assert!(configure_resolution_properties(&properties, 1280, 720).unwrap());
        assert_eq!(
            properties
                .values
                .borrow()
                .get(WAYDROID_WIDTH_PROPERTY)
                .map(String::as_str),
            Some("1280")
        );
        assert_eq!(
            properties
                .values
                .borrow()
                .get(WAYDROID_HEIGHT_PROPERTY)
                .map(String::as_str),
            Some("720")
        );
    }

    #[test]
    fn confirms_exact_rootless_render_resolution() {
        let properties = FakeProperties::with_size("1600", "900");

        confirm_resolution_properties(&properties, 1600, 900).unwrap();
    }

    #[test]
    fn rejects_mismatched_rootless_render_resolution() {
        let properties = FakeProperties::with_size("1280", "720");

        let error = confirm_resolution_properties(&properties, 1600, 900).unwrap_err();

        assert!(error.to_string().contains("requested 1600x900"));
        assert!(error.to_string().contains("reported 1280x720"));
    }

    #[test]
    fn partial_render_resolution_failure_restores_both_properties() {
        let properties = FakeProperties::with_size("1920", "1080");
        *properties.fail_once.borrow_mut() = Some(WAYDROID_HEIGHT_PROPERTY.to_owned());

        let error = configure_resolution_properties(&properties, 1280, 720).unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to configure Waydroid render size"));
        assert_eq!(
            properties
                .values
                .borrow()
                .get(WAYDROID_WIDTH_PROPERTY)
                .map(String::as_str),
            Some("1920")
        );
        assert_eq!(
            properties
                .values
                .borrow()
                .get(WAYDROID_HEIGHT_PROPERTY)
                .map(String::as_str),
            Some("1080")
        );
    }

    #[test]
    fn render_resolution_readback_mismatch_restores_both_properties() {
        let properties = FakeProperties::with_size("1920", "1080");
        *properties.ignore_once.borrow_mut() = Some(WAYDROID_WIDTH_PROPERTY.to_owned());

        let error = configure_resolution_properties(&properties, 1280, 720).unwrap_err();

        assert!(error.to_string().contains("did not persist render size"));
        assert_eq!(
            properties
                .values
                .borrow()
                .get(WAYDROID_WIDTH_PROPERTY)
                .map(String::as_str),
            Some("1920")
        );
        assert_eq!(
            properties
                .values
                .borrow()
                .get(WAYDROID_HEIGHT_PROPERTY)
                .map(String::as_str),
            Some("1080")
        );
    }

    #[test]
    fn parses_override_display_size_before_physical_size() {
        assert_eq!(
            parse_android_display_size("Physical size: 1920x1080\nOverride size: 1280x720\n"),
            Some((1280, 720))
        );
        assert_eq!(
            parse_android_display_size("Physical size: 1600x900\n"),
            Some((1600, 900))
        );
    }

    #[test]
    fn parses_gamescope_wayland_display_from_real_output() {
        assert_eq!(
            gamescope_wayland_display_event(
                "[gamescope] [Info]  wlserver: Running compositor on wayland display 'gamescope-0'
"
            ),
            Some("gamescope-0".to_owned())
        );
        assert_eq!(
            gamescope_wayland_display_event(
                "[gamescope] [Info] unrelated
"
            ),
            None
        );
    }

    #[test]
    fn parses_android_user_ready_from_real_session_stdout() {
        assert_eq!(
            android_user_ready_event("[13:07:35] Android with user 0 is ready\n"),
            Some(0)
        );
        assert_eq!(
            android_user_ready_event("[gbinder] Service manager /dev/binder has appeared\n"),
            None
        );
        assert_eq!(
            android_user_ready_event("[13:07:35] Android with user nope is ready\n"),
            None
        );
    }

    #[test]
    fn session_stdout_is_forwarded_and_publishes_readiness() {
        let input = b"[gbinder] appeared\n[13:07:35] Android with user 0 is ready\n";
        let mut output = Vec::new();
        let (sender, receiver) = std::sync::mpsc::channel();

        forward_session_output(Cursor::new(input), &mut output, Some(sender), None).unwrap();

        assert_eq!(output, input);
        assert_eq!(receiver.try_recv().unwrap(), 0);
        assert!(receiver.try_recv().is_err());
    }
}
