use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::privileged_bridge::{settle_wroid_input_udev, AndroidInputAccess};
use crate::{
    ensure_container_stopped, ensure_root, gamescope_is_available, presentation_for_game,
    remove_default_bridge, spawn_android_getevent_trace, stop_child, wait_for_android_input_device,
    wait_for_android_input_reader, BridgeBrokerClient, DesktopUser, DesktopWaydroidSession,
    DeviceConfig, InputDeviceNode, InstalledWaydroidBridge, UinputTouchInjector,
    WaydroidBridgeLease, WaydroidPresentation, WROID_TOUCHSCREEN_NAME,
};
use wroid_core::profile_v2::{InputV2, JoystickMode, JoystickWalkMode, ProfileV2};
use wroid_core::{Point, Resolution};
use wroid_input::mouse::{
    EvdevMouse, MouseButton, MouseButtonTransition, MouseEvent, RelativeMouseMotion,
};
use wroid_input::{EvdevKeyboard, HostKey, HostKeyEvent, KeyTransition};
use wroid_runtime::{
    AnalogInput, ContactId, DirectionalInput, HostKeyName, HostMouseButton, LayerId, LayerMode,
    ModifierMask, MouseAimController, MouseAimDelta, MouseAimUpdate, RuntimeControlAction,
    RuntimeControlBinding, RuntimeControlPlan, RuntimePhysicalInput, TouchEngine, TouchEngineError,
    TouchEvent, TouchFrame, TouchInjectionError, TouchInjector, TouchPhase,
};

const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;
const IDLE_POLL: Duration = Duration::from_millis(50);
/// How often the event loop checks the profile file for editor saves.
const PROFILE_RELOAD_POLL: Duration = Duration::from_millis(1000);

/// Attempts a live control-plan reload when the profile file changed.
///
/// A failed load or validation is logged and skipped; the running controls
/// stay in effect until the next successful save. `last_modified` is updated
/// either way so one save triggers exactly one attempt.
fn reload_profile_if_changed<I: TouchInjector>(
    runtime: &mut UnifiedRuntime<I>,
    spec: &mut ProfileReloadSpec,
) -> bool {
    let Ok(metadata) = fs::metadata(&spec.path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    if modified <= spec.last_modified {
        return false;
    }
    let outcome = (|| -> Result<RuntimeControlPlan, Box<dyn Error + Send + Sync>> {
        let profile = ProfileV2::load_from_path(&spec.path)?;
        profile
            .validate()
            .map_err(|error| io::Error::other(error.errors.join("; ")))?;
        Ok(RuntimeControlPlan::from_profile_v2(
            &profile,
            spec.resolution,
        )?)
    })();
    let mut swapped = false;
    match outcome {
        Ok(plan) => {
            let bindings = plan.controls.len();
            match runtime.replace_plan(plan) {
                Ok(()) => {
                    println!("[reload] controls updated live: {bindings} bindings");
                    swapped = true;
                }
                Err(error) => eprintln!("[reload] swap failed: {error}"),
            }
        }
        Err(error) => eprintln!("[reload] rejected profile changes: {error}"),
    }
    // Consume the change either way so one save triggers exactly one attempt.
    if let Ok(fresh) = fs::metadata(&spec.path) {
        if let Ok(mtime) = fresh.modified() {
            spec.last_modified = mtime;
        }
    }
    swapped
}

const INPUT_CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const FOCUS_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Grace period for a connected focus relay to report its first window event.
/// A relay that stays silent past this leaves the session dead (no grabs, no
/// input), so the loop degrades to manual F12 capture instead of waiting
/// forever.
const FOCUS_FALLBACK_AFTER: Duration = Duration::from_secs(10);
/// Upper bound on how long an input reader parks inside `poll`. Control
/// commands are delivered over a channel that `poll` cannot observe, so the
/// wait is bounded to keep capture toggles and shutdown responsive.
const INPUT_READER_WAIT_MS: libc::c_int = 20;
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
const SIGNAL_ERROR: usize = usize::MAX;
const PHYSICAL_INPUT_COUNT: usize = 62;

static INTERRUPT_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AndroidOpenAction {
    None,
    FullUi,
    Package,
}

fn android_open_action(show_ui: bool, launch_package: bool) -> AndroidOpenAction {
    if launch_package {
        AndroidOpenAction::Package
    } else if show_ui {
        AndroidOpenAction::FullUi
    } else {
        AndroidOpenAction::None
    }
}

type SignalHandler = extern "C" fn(i32);

unsafe extern "C" {
    fn signal(signum: i32, handler: SignalHandler) -> usize;
}

pub type GameSessionResult<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatencyMetrics {
    pub samples: u64,
    pub p50_micros: u64,
    pub p95_micros: u64,
    pub p99_micros: u64,
    pub max_micros: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GameSessionReport {
    pub frames_submitted: u64,
    pub peak_simultaneous_contacts: u64,
    pub mouse_aim_recenters: u64,
    pub reader_to_inject: LatencyMetrics,
    pub kernel_to_inject: Option<LatencyMetrics>,
    pub rejected_kernel_timestamps: u64,
}

pub fn run_game_session_cli(args: impl IntoIterator<Item = String>) -> GameSessionResult<()> {
    let Some(options) = GameSessionOptions::parse(args)? else {
        print_usage();
        return Ok(());
    };

    if options.cleanup {
        ensure_root("Wroid unified game session cleanup")?;
        let _bridge_lease = WaydroidBridgeLease::acquire_default("game session recovery")?;
        remove_default_bridge()?;
        println!("Removed the managed Wroid input bridge.");
        return Ok(());
    }

    run_game_session(options).map(|_| ())
}

pub fn run_game_session(mut options: GameSessionOptions) -> GameSessionResult<GameSessionReport> {
    let is_root = ensure_root("Wroid unified game session").is_ok();
    let bridge_broker = select_bridge_broker(is_root, options.bridge_broker.take())?;
    let lease_owner = format!(
        "game session {}",
        options
            .profile_path
            .as_deref()
            .map_or_else(|| "<unknown>".into(), |path| path.display().to_string())
    );
    let _bridge_lease = is_root
        .then(|| WaydroidBridgeLease::acquire_default(&lease_owner))
        .transpose()?;
    install_interrupt_handler()?;
    ensure_container_stopped()?;
    if is_root {
        remove_default_bridge()?;
    }

    let profile_path = required_path(&options.profile_path, "profile")?;
    let keyboard_path = required_path(&options.keyboard_path, "keyboard")?;
    let resolution = Resolution {
        width: options.width,
        height: options.height,
    };

    let profile = ProfileV2::load_from_path(profile_path)?;
    if let Err(error) = profile.validate() {
        return Err(invalid_input(format!(
            "invalid profile v2: {}",
            error.errors.join("; ")
        )));
    }
    let plan = RuntimeControlPlan::from_profile_v2(&profile, resolution)?;

    let mut keyboard = EvdevKeyboard::open(keyboard_path)?;
    let mut mouse = options
        .mouse_path
        .as_deref()
        .map(EvdevMouse::open)
        .transpose()?;
    let config = DeviceConfig::new(options.width, options.height)?;
    let session_resolution = Resolution {
        width: config.width(),
        height: config.height(),
    };
    let mut injector = UinputTouchInjector::open(config)?;
    let event_node = injector
        .sink_mut()
        .event_nodes()?
        .into_iter()
        .find(|path| {
            path.parent() == Some(Path::new("/dev/input"))
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("event"))
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "uinput event node not found"))?;
    let mut bridge = if is_root {
        let input_node = InputDeviceNode::from_path(&event_node)?;
        // Mirror the privileged helper's handoff: wait for udev to finish
        // device initialization, then hand the node to Android's AID_INPUT
        // group. Without this, InputReader cannot open the node and touches
        // silently never reach Android even though getevent lists it.
        settle_wroid_input_udev()?;
        let input_access = AndroidInputAccess::prepare(&input_node)?;
        SessionBridge::InProcess(
            input_access,
            InstalledWaydroidBridge::install_default(&input_node)?,
        )
    } else {
        let mut broker = bridge_broker.expect("rootless bridge was validated before setup");
        broker.open(&event_node)?;
        SessionBridge::Broker(broker)
    };
    let desktop_user = DesktopUser::from_session_environment()?;
    let gamescope_available = gamescope_is_available();
    let presentation = presentation_for_game(
        options.launch_package,
        gamescope_available,
        options.width,
        options.height,
    );
    if options.launch_package && presentation == WaydroidPresentation::Direct {
        eprintln!(
            "Fullscreen scaling unavailable: /usr/bin/gamescope is missing or not executable; using a direct Waydroid window."
        );
    }
    let mut waydroid = DesktopWaydroidSession::start_presented(desktop_user, presentation)?;
    if let Some(display) = waydroid.gamescope_wayland_display()?.as_deref() {
        println!("Gamescope nested Wayland display: {display}.");
    }

    let session_result = (|| -> GameSessionResult<GameSessionReport> {
        wait_for_android_ready_recovering(&mut waydroid)?;
        if waydroid.configure_resolution(options.width, options.height)? {
            waydroid.restart()?;
            wait_for_android_ready_recovering(&mut waydroid)?;
        }
        waydroid.confirm_resolution(options.width, options.height)?;
        // getevent visibility is not delivery: make sure Android's
        // InputReader actually opened the bridged touchscreen. A node with
        // host-only ownership passes getevent and drops every touch. The
        // check runs per bridge path (root: waydroid shell; rootless: the
        // privileged helper via lxc-attach, because `waydroid shell` needs
        // root and the daemon worker is unprivileged).
        bridge.verify_android_input()?;
        match android_open_action(options.show_ui, options.launch_package) {
            AndroidOpenAction::Package => {
                waydroid.launch_package(&plan.package_name)?;
                println!("Launched Android package {}.", plan.package_name);
            }
            AndroidOpenAction::FullUi => waydroid.show_full_ui()?,
            AndroidOpenAction::None => {}
        }
        let focus_connection = if options.grab {
            options
                .focus_socket
                .as_deref()
                .and_then(|path| match FocusConnection::connect(path) {
                    Ok(connection) => Some(connection),
                    Err(error) => {
                        eprintln!(
                            "Focus protection unavailable: could not connect to {}: {error}",
                            path.display()
                        );
                        eprintln!(
                            "Use F12 to release or recapture input; Ctrl+Esc stops the session."
                        );
                        None
                    }
                })
        } else {
            None
        };
        let focus_protected = focus_connection.is_some();
        let initially_focused = focus_connection
            .as_ref()
            .is_none_or(|connection| connection.focused);

        keyboard.set_nonblocking(true)?;
        if let Some(mouse) = mouse.as_mut() {
            mouse.set_nonblocking(true)?;
        }
        if options.grab && initially_focused {
            keyboard.grab()?;
            if let Some(mouse) = mouse.as_mut() {
                mouse.grab()?;
            }
        }

        println!("Unified game session is live.");
        println!("Profile: {} ({})", plan.profile_name, plan.package_name);
        println!(
            "Keyboard: {} ({})",
            keyboard.name(),
            keyboard.path().display()
        );
        if let Some(mouse) = mouse.as_ref() {
            println!("Mouse: {} ({})", mouse.name(), mouse.path().display());
        } else {
            println!("Mouse: not required by this profile");
        }
        println!("Android touchscreen: {}", event_node.display());
        println!("Controls: {}", plan.controls.len());
        if options.trace_input {
            println!("Input tracing: enabled");
        }
        if focus_protected {
            println!(
                "Focus protection: {}.",
                if initially_focused {
                    "Waydroid focused; devices captured"
                } else {
                    "Waydroid is not focused; devices released"
                }
            );
        } else if options.grab {
            println!("Focus protection: compositor fallback; F12 controls capture manually.");
        } else {
            println!("Focus protection: capture disabled by --no-grab.");
        }
        println!(
            "Press F12 to release/reacquire input, F11 to switch aim/UI-cursor modes. Press Ctrl+Esc to stop."
        );

        let (sender, receiver) = mpsc::channel();
        let input_active = !focus_protected || initially_focused;
        let keyboard_control = spawn_keyboard_reader(keyboard, sender.clone(), input_active);
        let mut mouse_control = None;
        if let Some(mouse) = mouse {
            mouse_control = Some(spawn_mouse_reader(mouse, sender.clone(), input_active));
        }
        let mut game_output: Option<String> = None;
        let focus_receiver = focus_connection.map(|connection| {
            let (focus_sender, focus_receiver) = mpsc::channel();
            game_output = connection.output;
            spawn_focus_reader(connection.reader, focus_sender);
            focus_receiver
        });
        let input_readers = InputReaderControls {
            keyboard: keyboard_control,
            mouse: mouse_control,
        };

        let mut runtime = UnifiedRuntime::new(
            plan,
            SessionMetricsInjector::new(injector),
            options.trace_input,
        )?;

        // Cursor overlay: a Wayland layer surface on the desktop compositor.
        // Gamescope never composites nested layer-shell surfaces, so the
        // virtual cursor sprite must be rendered above the fullscreen
        // Gamescope window by the host compositor. Its emit closure forwards
        // any compositor-delivered pointer events into the event loop.
        let host_wayland_display = waydroid
            .user()
            .wayland_display()
            .to_string_lossy()
            .into_owned();
        let cursor_overlay = {
            let cursor_sender = sender.clone();
            match crate::cursor_overlay::CursorOverlay::spawn(
                waydroid.user().runtime_dir(),
                &host_wayland_display,
                game_output.as_deref(),
                move |event| {
                    let _ = cursor_sender.send(HostEvent::Cursor {
                        event,
                        received_at: Instant::now(),
                    });
                },
            ) {
                Ok(overlay) => {
                    println!("Cursor overlay: Android UI clicks available.");
                    Some(overlay)
                }
                Err(error) => {
                    eprintln!("Cursor overlay unavailable: {error}; UI cursor mode disabled.");
                    if !runtime.has_always_on_aim() && runtime.has_toggle_key_aim() {
                        eprintln!(
                            "This profile boots in UI cursor mode: without the overlay, left \
                                 clicks cannot reach Android menus. Aim bindings still work once \
                                 the aim key is pressed. Please report the error above."
                        );
                    }
                    None
                }
            }
        };

        // Initial input mode: profiles with any always-on aim boot in aim mode;
        // toggle-key-only aim profiles (with a working overlay) boot in UI
        // cursor mode so LMB clicks Android UI until the toggle key is pressed.
        let initial_mode = if !runtime.has_always_on_aim()
            && runtime.has_toggle_key_aim()
            && cursor_overlay.is_some()
        {
            InputMode::Ui
        } else {
            InputMode::Aim
        };
        if initial_mode == InputMode::Ui {
            println!(
                "Input mode: ui (virtual cursor clicks the Android UI; press the aim key to aim)."
            );
        }

        if initially_focused {
            runtime.start()?;
        }
        // Optional Android-side trace: streams the bridged node's raw events
        // from inside the container so delivery problems are visible at once.
        let android_input_trace = options
            .trace_android_input
            .then(|| spawn_android_getevent_trace(&event_node))
            .transpose()?;
        if android_input_trace.is_some() {
            println!(
                "Android input trace: streaming getevent for {}.",
                event_node.display()
            );
        }
        let loop_result = run_event_loop(
            &receiver,
            &input_readers,
            &mut runtime,
            EventLoopOptions {
                trace_input: options.trace_input,
                exit_after: options.exit_after,
                focus_protected,
                focused: initially_focused,
                focus_receiver: focus_receiver.as_ref(),
                grab_allowed: options.grab,
                initial_mode,
                overlay: cursor_overlay.as_ref(),
                profile_reload: Some(ProfileReloadSpec {
                    path: profile_path.to_path_buf(),
                    resolution: session_resolution,
                    last_modified: fs::metadata(profile_path)
                        .and_then(|meta| meta.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                }),
            },
        );
        if let Some(mut trace) = android_input_trace {
            if let Err(error) = stop_child(&mut trace) {
                eprintln!("Android input trace stop failed: {error}");
            }
        }
        let cleanup_result = runtime.stop();
        let exit = loop_result?;
        cleanup_result?;
        let report = runtime.report();
        UnifiedRuntime::print_report(&report);
        match exit {
            EventLoopExit::ExitHotkeyRequested => println!("Stop requested by Ctrl+Esc."),
            EventLoopExit::InterruptRequested => {
                println!("Stop requested by Ctrl+C or a termination signal.")
            }
            EventLoopExit::TimeLimitReached => println!("Diagnostic time limit reached."),
        }
        Ok(report)
    })();

    let stop_result = waydroid.stop();
    let bridge_result = bridge.cleanup(stop_result.is_ok());
    let report = combine_session_results(session_result, stop_result, bridge_result)?;
    println!("Unified game session stopped cleanly.");
    Ok(report)
}

enum SessionBridge {
    InProcess(AndroidInputAccess, InstalledWaydroidBridge),
    Broker(BridgeBrokerClient),
}

/// Wait for Android user readiness, recovering a stalled boot exactly once.
///
/// A fresh container start right after a desktop-session stop can wedge
/// mid-boot with no readiness output and a live session child (observed as a
/// silent stall after container init). A full stop and clean restart boots
/// fine, so retry once before failing the whole game session.
fn wait_for_android_ready_recovering(
    waydroid: &mut DesktopWaydroidSession,
) -> GameSessionResult<()> {
    if let Err(first_error) = waydroid.wait_until_android_ready() {
        eprintln!(
            "Android did not become ready: {first_error}\nRestarting the Waydroid session once \
             to recover a stalled Android boot…"
        );
        waydroid.restart()?;
        waydroid.wait_until_android_ready()?;
    }
    Ok(())
}

impl SessionBridge {
    fn verify_android_input(&mut self) -> io::Result<()> {
        match self {
            // Root sessions can run `waydroid shell` directly: check both
            // EventHub visibility and InputReader registration.
            Self::InProcess(..) => {
                wait_for_android_input_device(WROID_TOUCHSCREEN_NAME)?;
                wait_for_android_input_reader(WROID_TOUCHSCREEN_NAME)
            }
            // The daemon worker is unprivileged and `waydroid shell` needs
            // root, so the helper performs the full verification (getevent +
            // dumpsys via lxc-attach) inside VERIFY_ANDROID_INPUT.
            Self::Broker(broker) => broker.verify_android_input(),
        }
    }

    fn cleanup(self, waydroid_stopped: bool) -> io::Result<()> {
        match self {
            Self::InProcess(input_access, bridge) => {
                // Restore the bridge config first (container stopped), then
                // give the event node back to the host ownership.
                let bridge_result = bridge.cleanup();
                let access_result = input_access.cleanup();
                match (bridge_result, access_result) {
                    (Ok(()), Ok(())) => Ok(()),
                    (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                    (Err(bridge_error), Err(access_error)) => Err(io::Error::other(format!(
                        "bridge cleanup failed: {bridge_error}; input ownership restore failed: \
                         {access_error}"
                    ))),
                }
            }
            Self::Broker(broker) => broker.finish(waydroid_stopped),
        }
    }
}

fn select_bridge_broker(
    is_root: bool,
    broker: Option<BridgeBrokerClient>,
) -> io::Result<Option<BridgeBrokerClient>> {
    if is_root {
        return Ok(None);
    }
    broker.map(Some).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "rootless game session requires a daemon-owned bridge channel",
        )
    })
}

fn combine_session_results<T>(
    session_result: GameSessionResult<T>,
    stop_result: io::Result<()>,
    bridge_result: io::Result<()>,
) -> GameSessionResult<T> {
    let (session_value, session_error) = match session_result {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error)),
    };
    let stop_error = stop_result.err();
    let bridge_error = bridge_result.err();
    let error_count = usize::from(session_error.is_some())
        + usize::from(stop_error.is_some())
        + usize::from(bridge_error.is_some());

    if error_count == 0 {
        return Ok(session_value.expect("an error-free session has a value"));
    }
    if error_count == 1 {
        if let Some(error) = session_error {
            return Err(error);
        }
        if let Some(error) = stop_error {
            return Err(error.into());
        }
        return Err(bridge_error
            .expect("one error exists and it is the bridge error")
            .into());
    }

    let mut failures = Vec::with_capacity(error_count);
    if let Some(error) = session_error {
        failures.push(format!("game session failed: {error}"));
    }
    if let Some(error) = stop_error {
        failures.push(format!("Waydroid shutdown failed: {error}"));
    }
    if let Some(error) = bridge_error {
        failures.push(format!("input bridge cleanup failed: {error}"));
    }
    Err(io::Error::other(failures.join("\nAdditionally, ")).into())
}

fn required_path<'a>(path: &'a Option<PathBuf>, label: &str) -> GameSessionResult<&'a Path> {
    path.as_deref()
        .ok_or_else(|| invalid_input(format!("missing {label} path")))
}

#[derive(Debug)]
enum HostEvent {
    Keyboard {
        events: Vec<HostKeyEvent>,
        received_at: Instant,
        kernel_timestamp: Option<SystemTime>,
    },
    Mouse {
        events: Vec<MouseEvent>,
        received_at: Instant,
        kernel_timestamp: Option<SystemTime>,
    },
    Cursor {
        event: crate::cursor_overlay::CursorEvent,
        received_at: Instant,
    },
    ReaderFailed {
        reader: &'static str,
        message: String,
    },
}

#[derive(Debug)]
enum FocusEvent {
    Changed(bool),
    Unavailable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventLoopExit {
    ExitHotkeyRequested,
    InterruptRequested,
    TimeLimitReached,
}

enum InputReaderCommand {
    SetCapture {
        enabled: bool,
        reply: Sender<Result<(), String>>,
    },
    SetMouseMode {
        forward: bool,
        grab: bool,
        reply: Sender<Result<(), String>>,
    },
}

struct InputReaderControls {
    keyboard: Sender<InputReaderCommand>,
    mouse: Option<Sender<InputReaderCommand>>,
}

impl InputReaderControls {
    fn set_keyboard_capture(&self, enabled: bool) -> GameSessionResult<()> {
        set_reader_capture(&self.keyboard, "keyboard", enabled)
    }

    fn set_mouse_mode(&self, forward: bool, grab: bool) -> GameSessionResult<()> {
        if let Some(mouse) = self.mouse.as_ref() {
            let (reply, response) = mpsc::channel();
            mouse
                .send(InputReaderCommand::SetMouseMode {
                    forward,
                    grab,
                    reply,
                })
                .map_err(|_| io::Error::other("mouse control disconnected"))?;
            response
                .recv_timeout(INPUT_CONTROL_TIMEOUT)
                .map_err(|error| {
                    io::Error::other(format!(
                        "mouse input reader did not update mouse mode: {error}"
                    ))
                })?
                .map_err(|message| io::Error::other(format!("mouse mode failed: {message}")))?;
        }
        Ok(())
    }
}

fn set_reader_capture(
    control: &Sender<InputReaderCommand>,
    reader: &'static str,
    enabled: bool,
) -> GameSessionResult<()> {
    let (reply, response) = mpsc::channel();
    control
        .send(InputReaderCommand::SetCapture { enabled, reply })
        .map_err(|_| io::Error::other(format!("{reader} input reader disconnected")))?;
    response
        .recv_timeout(INPUT_CONTROL_TIMEOUT)
        .map_err(|error| {
            io::Error::other(format!(
                "{reader} input reader did not update capture state: {error}"
            ))
        })?
        .map_err(|message| io::Error::other(format!("{reader} capture failed: {message}")))?;
    Ok(())
}

fn spawn_keyboard_reader(
    mut keyboard: EvdevKeyboard,
    sender: Sender<HostEvent>,
    mut captured: bool,
) -> Sender<InputReaderCommand> {
    let (control, commands) = mpsc::channel();
    let keyboard_fd = keyboard.as_raw_fd();
    thread::spawn(move || loop {
        match commands.try_recv() {
            Ok(InputReaderCommand::SetCapture { enabled, reply }) => {
                let result = update_keyboard_capture(&mut keyboard, enabled);
                if result.is_ok() {
                    captured = enabled;
                }
                let _ = reply.send(result.map_err(|error| error.to_string()));
            }
            // Mouse-only command; never routed to the keyboard reader.
            Ok(InputReaderCommand::SetMouseMode { .. }) => {}
            Err(mpsc::TryRecvError::Disconnected) => return,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let had_events = match keyboard.next_host_key_batch() {
            Ok(batch) => {
                let had_events = !batch.events.is_empty();
                let forwarded = keyboard_events_for_capture(batch.events, captured);
                if !forwarded.is_empty()
                    && sender
                        .send(HostEvent::Keyboard {
                            events: forwarded,
                            received_at: Instant::now(),
                            kernel_timestamp: batch.kernel_timestamp,
                        })
                        .is_err()
                {
                    return;
                }
                had_events
            }
            Err(error) if error.is_would_block() => false,
            Err(error) => {
                let _ = sender.send(HostEvent::ReaderFailed {
                    reader: "keyboard",
                    message: error.to_string(),
                });
                return;
            }
        };
        if had_events {
            continue;
        }
        // Park in the kernel until the device is readable. A bounded wait still
        // lets queued control commands be observed on the next iteration.
        wait_for_input(keyboard_fd, INPUT_READER_WAIT_MS);
        match commands.try_recv() {
            Ok(InputReaderCommand::SetCapture { enabled, reply }) => {
                let result = update_keyboard_capture(&mut keyboard, enabled);
                if result.is_ok() {
                    captured = enabled;
                }
                let _ = reply.send(result.map_err(|error| error.to_string()));
            }
            // Mouse-only command; never routed to the keyboard reader.
            Ok(InputReaderCommand::SetMouseMode { .. }) => {}
            Err(mpsc::TryRecvError::Disconnected) => return,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    });
    control
}

/// Block until the evdev descriptor has readable data or the bound elapses.
///
/// Returning on either condition lets the reader sleep in the kernel between
/// events instead of spinning on a fixed timer, which removes the fixed poll
/// delay from every keystroke and mouse report.
fn wait_for_input(fd: libc::c_int, timeout_ms: libc::c_int) {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `descriptor` is a single valid, initialized pollfd owned by this
    // call, and the device fd stays open for the lifetime of the reader.
    unsafe {
        libc::poll(&mut descriptor, 1, timeout_ms);
    }
}

fn keyboard_events_for_capture(events: Vec<HostKeyEvent>, captured: bool) -> Vec<HostKeyEvent> {
    if captured {
        return events;
    }
    events
        .into_iter()
        .filter(|event| event.key == HostKey::F12)
        .collect()
}

fn spawn_mouse_reader(
    mut mouse: EvdevMouse,
    sender: Sender<HostEvent>,
    captured: bool,
) -> Sender<InputReaderCommand> {
    let (control, commands) = mpsc::channel();
    let mouse_fd = mouse.as_raw_fd();
    let mut forwarding = captured;
    let mut grabbed = captured; // Assuming initial capture implies both.
    thread::spawn(move || loop {
        match commands.try_recv() {
            Ok(InputReaderCommand::SetCapture { enabled, reply }) => {
                let result = update_mouse_capture(&mut mouse, enabled);
                if result.is_ok() {
                    forwarding = enabled;
                    grabbed = enabled;
                }
                let _ = reply.send(result.map_err(|error| error.to_string()));
            }
            Ok(InputReaderCommand::SetMouseMode {
                forward,
                grab,
                reply,
            }) => {
                let result = if grab != grabbed {
                    if grab {
                        let _ = drain_mouse(&mut mouse);
                        mouse.grab()
                    } else {
                        mouse.ungrab()
                    }
                    .map_err(|e| e.to_string())
                } else {
                    Ok(())
                };
                if result.is_ok() {
                    forwarding = forward;
                    grabbed = grab;
                }
                let _ = reply.send(result);
            }
            Err(mpsc::TryRecvError::Disconnected) => return,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let had_events = match mouse.next_event_batch() {
            Ok(batch) => {
                let had_events = !batch.events.is_empty();
                if forwarding
                    && had_events
                    && sender
                        .send(HostEvent::Mouse {
                            events: batch.events,
                            received_at: Instant::now(),
                            kernel_timestamp: batch.kernel_timestamp,
                        })
                        .is_err()
                {
                    return;
                }
                had_events
            }
            Err(error) if error.is_would_block() => false,
            Err(error) => {
                let _ = sender.send(HostEvent::ReaderFailed {
                    reader: "mouse",
                    message: error.to_string(),
                });
                return;
            }
        };
        if had_events {
            continue;
        }
        // Park in the kernel until the device is readable. A bounded wait still
        // lets queued control commands be observed on the next iteration.
        wait_for_input(mouse_fd, INPUT_READER_WAIT_MS);
        match commands.try_recv() {
            Ok(InputReaderCommand::SetCapture { enabled, reply }) => {
                let result = update_mouse_capture(&mut mouse, enabled);
                if result.is_ok() {
                    forwarding = enabled;
                    grabbed = enabled;
                }
                let _ = reply.send(result.map_err(|error| error.to_string()));
            }
            Ok(InputReaderCommand::SetMouseMode {
                forward,
                grab,
                reply,
            }) => {
                let result = if grab != grabbed {
                    if grab {
                        let _ = drain_mouse(&mut mouse);
                        mouse.grab()
                    } else {
                        mouse.ungrab()
                    }
                    .map_err(|e| e.to_string())
                } else {
                    Ok(())
                };
                if result.is_ok() {
                    forwarding = forward;
                    grabbed = grab;
                }
                let _ = reply.send(result);
            }
            Err(mpsc::TryRecvError::Disconnected) => return,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    });
    control
}

fn update_keyboard_capture(
    keyboard: &mut EvdevKeyboard,
    enabled: bool,
) -> Result<(), wroid_input::KeyboardDeviceError> {
    if enabled {
        drain_keyboard(keyboard)?;
        keyboard.grab()
    } else {
        keyboard.ungrab()
    }
}

fn update_mouse_capture(
    mouse: &mut EvdevMouse,
    enabled: bool,
) -> Result<(), wroid_input::mouse::MouseDeviceError> {
    if enabled {
        drain_mouse(mouse)?;
        mouse.grab()
    } else {
        mouse.ungrab()
    }
}

fn drain_keyboard(keyboard: &mut EvdevKeyboard) -> Result<(), wroid_input::KeyboardDeviceError> {
    for _ in 0..64 {
        match keyboard.next_host_key_events() {
            Ok(_) => {}
            Err(error) if error.is_would_block() => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// RAII probe: logs any loop phase that overstays its budget. Cheap enough
/// to keep armed in production; silent unless something regresses.
struct LoopPhaseTimer {
    phase: &'static str,
    started: Instant,
    budget: Duration,
}

impl LoopPhaseTimer {
    fn new(phase: &'static str) -> Self {
        Self {
            phase,
            started: Instant::now(),
            budget: Duration::from_millis(30),
        }
    }

    #[allow(dead_code)]
    fn with_budget(phase: &'static str, budget: Duration) -> Self {
        Self {
            phase,
            started: Instant::now(),
            budget,
        }
    }
}

impl Drop for LoopPhaseTimer {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        if elapsed >= self.budget {
            eprintln!(
                "[stall] loop phase '{}' took {:.1} ms",
                self.phase,
                elapsed.as_secs_f64() * 1000.0
            );
        }
    }
}

fn drain_mouse(mouse: &mut EvdevMouse) -> Result<(), wroid_input::mouse::MouseDeviceError> {
    for _ in 0..64 {
        match mouse.next_events() {
            Ok(_) => {}
            Err(error) if error.is_would_block() => {
                mouse.clear_pending_report();
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
    mouse.clear_pending_report();
    Ok(())
}

struct FocusConnection {
    reader: BufReader<UnixStream>,
    focused: bool,
    /// Output hosting the focused game window, when the relay reported one;
    /// the cursor overlay anchors itself to this output.
    output: Option<String>,
}

impl FocusConnection {
    fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(FOCUS_CONNECT_TIMEOUT))?;
        let mut reader = BufReader::new(stream);
        // The relay emits its initial focus state and may attach an
        // `output <name>` metadata line before or after it. Line order is
        // not guaranteed under startup load, so read lines until a valid
        // state line arrives instead of assuming a fixed sequence.
        let started = Instant::now();
        let mut focused = None;
        let mut output = None;
        while focused.is_none() {
            if started.elapsed() >= FOCUS_CONNECT_TIMEOUT {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "focus relay did not report an initial focus state",
                ));
            }
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "focus relay closed before reporting initial state",
                ));
            }
            match classify_relay_line(&line)? {
                RelayLine::State(value) => focused = Some(value),
                RelayLine::Output(name) => output = Some(name),
            }
        }
        // Opportunistically pick up an output line that follows the state
        // line; late ones are tolerated by the background reader too.
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(100)))?;
        let mut trailing = String::new();
        match reader.read_line(&mut trailing) {
            Ok(0) | Err(_) => {}
            Ok(_) => {
                if let Ok(RelayLine::Output(name)) = classify_relay_line(&trailing) {
                    output = Some(name);
                }
            }
        }
        reader.get_ref().set_read_timeout(None)?;
        Ok(Self {
            reader,
            focused: focused.unwrap_or(false),
            output,
        })
    }
}

#[derive(Debug)]
enum RelayLine {
    State(bool),
    Output(String),
}

fn classify_relay_line(line: &str) -> io::Result<RelayLine> {
    let trimmed = line.trim();
    if let Some(name) = trimmed.strip_prefix("output ") {
        let name = name.trim();
        if !name.is_empty() {
            return Ok(RelayLine::Output(name.to_owned()));
        }
    }
    match trimmed {
        "focused" => Ok(RelayLine::State(true)),
        "unfocused" => Ok(RelayLine::State(false)),
        value => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid focus relay state: {value}"),
        )),
    }
}

fn spawn_focus_reader(mut reader: BufReader<UnixStream>, sender: Sender<FocusEvent>) {
    thread::spawn(move || loop {
        let mut state = String::new();
        match reader.read_line(&mut state) {
            Ok(0) => {
                let _ = sender.send(FocusEvent::Unavailable(
                    "desktop focus relay disconnected".to_owned(),
                ));
                return;
            }
            Ok(_) => match classify_relay_line(&state) {
                // Metadata lines can trail the handshake at any time; they
                // never carry focus transitions.
                Ok(RelayLine::Output(_)) => {}
                Ok(RelayLine::State(focused)) => {
                    if sender.send(FocusEvent::Changed(focused)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(FocusEvent::Unavailable(error.to_string()));
                    return;
                }
            },
            Err(error) => {
                let _ = sender.send(FocusEvent::Unavailable(format!(
                    "desktop focus relay failed: {error}"
                )));
                return;
            }
        }
    });
}

struct EventLoopOptions<'a> {
    trace_input: bool,
    exit_after: Option<Duration>,
    focus_protected: bool,
    focused: bool,
    focus_receiver: Option<&'a Receiver<FocusEvent>>,
    /// Whether EVIOCGRAB is permitted at all (`--no-grab` turns it off).
    grab_allowed: bool,
    /// Initial Aim/UI mode when input is captured.
    initial_mode: InputMode,
    /// Optional UI cursor renderer. Startup chooses whether UI mode is
    /// supported; losing this renderer later must not change input routing.
    overlay: Option<&'a crate::cursor_overlay::CursorOverlay>,
    /// Live profile reload: watch the profile file and hot-swap the control
    /// plan when the editor saves changes.
    profile_reload: Option<ProfileReloadSpec>,
}

/// Everything the event loop needs to hot-reload the control plan.
struct ProfileReloadSpec {
    path: PathBuf,
    resolution: Resolution,
    last_modified: SystemTime,
}

/// Input ownership mode: who currently owns the physical mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    /// Aim/gameplay mode: the mouse is forwarded to profile bindings (`fire`, aim)
    /// and grabbed so the game owns the pointer.
    Aim,
    /// UI cursor mode: raw mouse events are not forwarded (gating the `fire`
    /// binding at the source); the cursor overlay clicks the Android UI.
    Ui,
}

impl fmt::Display for InputMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputMode::Aim => f.write_str("aim"),
            InputMode::Ui => f.write_str("ui"),
        }
    }
}

/// The session-owned virtual cursor for UI mode.
///
/// Gamescope's nested (xdg) backend neither delivers pointer events to
/// nested clients nor controls the host cursor, so the session keeps its own
/// cursor: relative mouse deltas move it, the overlay draws the sprite, and
/// left clicks become Android touches at the mapped position.
#[derive(Debug, Clone, Copy, PartialEq)]
struct VirtualCursor {
    x: f64,
    y: f64,
    pressed: bool,
}

impl VirtualCursor {
    fn centered(width: u32, height: u32) -> Self {
        Self {
            x: f64::from(width) / 2.0,
            y: f64::from(height) / 2.0,
            pressed: false,
        }
    }

    fn move_by(&mut self, dx: i32, dy: i32, width: u32, height: u32) {
        self.x = (self.x + f64::from(dx)).clamp(0.0, f64::from(width.saturating_sub(1)));
        self.y = (self.y + f64::from(dy)).clamp(0.0, f64::from(height.saturating_sub(1)));
    }

    fn point(&self) -> Point {
        Point {
            x: self.x.floor().clamp(0.0, f64::from(u32::MAX)) as u32,
            y: self.y.floor().clamp(0.0, f64::from(u32::MAX)) as u32,
        }
    }
}

/// Overlay handle that degrades instead of failing the session.
///
/// If the sprite thread dies mid-session the overlay is dropped from the
/// loop with a single warning: Android clicks keep working through the
/// virtual cursor (uinput injection is independent of the sprite), so a dead
/// sprite must not abort gameplay or spam the log on every mouse batch.
struct OverlayGuard<'a> {
    overlay: Option<&'a crate::cursor_overlay::CursorOverlay>,
    warned: bool,
}

impl<'a> OverlayGuard<'a> {
    fn new(overlay: Option<&'a crate::cursor_overlay::CursorOverlay>) -> Self {
        Self {
            overlay,
            warned: false,
        }
    }

    fn set_cursor(&mut self, visible: bool, x: f64, y: f64) {
        if let Some(overlay) = self.overlay {
            // Fire-and-forget: the overlay coalesces to the latest position,
            // and waiting for its round-trip here used to throttle every
            // mouse batch to the overlay poll interval.
            let result = overlay.set_cursor_async(visible, x, y);
            if let Err(error) = result {
                self.degrade(error);
            }
        }
    }

    fn degrade(&mut self, error: std::io::Error) {
        self.overlay = None;
        if !self.warned {
            self.warned = true;
            eprintln!(
                "Cursor overlay stopped ({error}); the on-screen cursor sprite is unavailable, \
                 but Android clicks and profile controls keep working. Restart the session to \
                 restore the sprite."
            );
        }
    }
}

/// Apply an input mode transition while the session is active (captured).
///
/// Keyboard capture and the mouse grab stay on in both modes — the mouse
/// deltas either feed profile bindings (aim) or the virtual cursor (UI).
/// The grab freezes the host cursor at its pre-capture position; the overlay
/// sprite (UI mode) or its absence plus the game's own hidden cursor
/// (aim mode) keep exactly one visible pointer.
// The parameter set mirrors the loop's full transition context; grouping it
// further would hide which side effects each call site relies on.
#[allow(clippy::too_many_arguments)]
fn apply_input_mode<I: TouchInjector>(
    runtime: &mut UnifiedRuntime<I>,
    input_readers: &InputReaderControls,
    overlay: &mut OverlayGuard<'_>,
    mode: InputMode,
    grab_allowed: bool,
    reactivate_toggle_aim: bool,
    virtual_cursor: &VirtualCursor,
) -> GameSessionResult<()> {
    match mode {
        InputMode::Aim => {
            input_readers.set_mouse_mode(true, grab_allowed)?;
            overlay.set_cursor(false, 0.0, 0.0);
            runtime.restore_aim(reactivate_toggle_aim)?;
        }
        InputMode::Ui => {
            runtime.set_aim_enabled(false)?;
            runtime.release_mouse_buttons()?;
            // The virtual cursor consumes the deltas itself; keep forwarding
            // (and the grab) so the host cursor does not drift in parallel.
            input_readers.set_mouse_mode(true, grab_allowed)?;
            overlay.set_cursor(true, virtual_cursor.x, virtual_cursor.y);
        }
    }
    Ok(())
}

/// Fully release input to the host OS (F12 release / focus loss).
///
/// Stops keyboard forwarding, drops the mouse (no forwarding, no grab),
/// disables the overlay's input region and sprite, and suspends the runtime.
/// The host cursor reappears frozen where it was grabbed; KWin no longer
/// allows scripts to reposition it (`cursorPos` is read-only there), so no
/// restore warp is attempted. The current [`InputMode`] is deliberately not
/// touched so recapture restores it.
fn release_input_to_os<I: TouchInjector>(
    runtime: &mut UnifiedRuntime<I>,
    input_readers: &InputReaderControls,
    overlay: &mut OverlayGuard<'_>,
) -> GameSessionResult<()> {
    input_readers.set_keyboard_capture(false)?;
    input_readers.set_mouse_mode(false, false)?;
    overlay.set_cursor(false, 0.0, 0.0);
    runtime.suspend()?;
    Ok(())
}

fn run_event_loop<I: TouchInjector>(
    receiver: &Receiver<HostEvent>,
    input_readers: &InputReaderControls,
    runtime: &mut UnifiedRuntime<I>,
    mut options: EventLoopOptions<'_>,
) -> GameSessionResult<EventLoopExit> {
    let trace_input = options.trace_input;
    let exit_after = options.exit_after;
    let grab_allowed = options.grab_allowed;
    let mut overlay = OverlayGuard::new(options.overlay);
    let mut focus_protected = options.focus_protected;
    let mut waydroid_focused = options.focused;
    let mut input_active = options.focused;
    let mut manually_released = false;
    // Whether the relay ever delivered a window event since the loop began.
    // A silent relay is indistinguishable from a broken one until it speaks.
    let mut saw_focus_event = false;
    let mut mode = options.initial_mode;
    // Overlay health is a rendering concern, not an input-routing signal.
    // Freeze the capability selected at session startup so a failed cursor
    // sprite cannot silently turn an inactive toggle-aim profile into Aim and
    // route LMB back to its gameplay binding.
    let ui_mode_supported = options.initial_mode == InputMode::Ui;
    // Aim activity at the moment of the last OS release, so recapture can
    // restore the mode that was in effect rather than resetting.
    let mut toggle_aim_active_before_release = false;
    // Set when the user switched modes with F11; the automatic aim-state sync
    // must not fight that choice until the user overrides it again.
    let mut mode_manually_overridden = false;
    let loop_started_at = Instant::now();
    let mut next_profile_check = Duration::ZERO;
    let mut control_pressed = false;
    let (surface_width, surface_height) = runtime.resolution();
    let mut virtual_cursor = VirtualCursor::centered(surface_width, surface_height);

    // Sync mode with the current runtime aim state. Aim mode follows any
    // active aim controller; when no aim is active and the session started
    // with UI support, a toggle-key-only profile sits in UI cursor mode. The
    // fixed startup capability deliberately outlives the fallible renderer.
    let desired_mode = |runtime: &UnifiedRuntime<I>| -> InputMode {
        if runtime.aim_active() || !runtime.has_toggle_key_aim() || !ui_mode_supported {
            InputMode::Aim
        } else {
            InputMode::Ui
        }
    };

    if input_active && mode == InputMode::Ui {
        // Apply the non-default initial mode (Aim needs no apply: the readers
        // already start forwarded/grabbed like legacy sessions).
        apply_input_mode(
            runtime,
            input_readers,
            &mut overlay,
            InputMode::Ui,
            grab_allowed,
            false,
            &virtual_cursor,
        )?;
        println!(
            "Cursor mode: the virtual cursor clicks the Android UI; press the aim key to aim."
        );
    }

    loop {
        if interrupt_requested() {
            return Ok(EventLoopExit::InterruptRequested);
        }
        if let Some(focus_receiver) = options.focus_receiver {
            while let Ok(event) = focus_receiver.try_recv() {
                match event {
                    FocusEvent::Changed(next_focused) if focus_protected => {
                        saw_focus_event = true;
                        waydroid_focused = next_focused;
                        control_pressed = false;
                        if next_focused {
                            if manually_released {
                                println!(
                                    "Focus protection: Waydroid focused; press F12 to recapture input."
                                );
                            } else if !input_active {
                                input_readers.set_keyboard_capture(true)?;
                                apply_input_mode(
                                    runtime,
                                    input_readers,
                                    &mut overlay,
                                    mode,
                                    grab_allowed,
                                    toggle_aim_active_before_release,
                                    &virtual_cursor,
                                )?;
                                input_active = true;
                                println!("Focus protection: Waydroid focused; input captured ({mode} mode).");
                            }
                        } else if input_active {
                            input_active = false;
                            // Keep the mode in effect (including a manual F11
                            // override) so recapture restores it.
                            toggle_aim_active_before_release = runtime.aim_active();
                            release_input_to_os(runtime, input_readers, &mut overlay)?;
                            println!("Focus protection: Waydroid unfocused; input released.");
                        }
                    }
                    FocusEvent::Unavailable(message) if focus_protected => {
                        focus_protected = false;
                        waydroid_focused = false;
                        manually_released = true;
                        control_pressed = false;
                        if input_active {
                            input_active = false;
                            toggle_aim_active_before_release = runtime.aim_active();
                            release_input_to_os(runtime, input_readers, &mut overlay)?;
                        }
                        eprintln!("Focus protection stopped: {message}");
                        eprintln!(
                            "Input was released. Press F12 to recapture with manual protection."
                        );
                    }
                    FocusEvent::Changed(next_focused) => {
                        // Focus signal arriving after protection was degraded
                        // (relay fallback or disconnect): capture stays manual,
                        // but remember that the relay is alive after all.
                        saw_focus_event = true;
                        waydroid_focused = next_focused;
                        if next_focused && manually_released {
                            println!(
                                "Focus protection: game window focused; press F12 to capture input."
                            );
                        }
                    }
                    FocusEvent::Unavailable(_) => {}
                }
            }
            // A connected relay that never reports the game window leaves the
            // session dead: no grabs, no input, and F12 alone cannot recover
            // while the loop still believes focus protection is active.
            // Degrade to manual capture once it stays silent past the grace
            // period.
            if focus_protected
                && !input_active
                && !manually_released
                && !saw_focus_event
                && loop_started_at.elapsed() >= FOCUS_FALLBACK_AFTER
            {
                focus_protected = false;
                manually_released = true;
                eprintln!(
                    "Focus protection: no focus events within {} seconds; the relay may not be \
                     tracking the game window.",
                    FOCUS_FALLBACK_AFTER.as_secs()
                );
                eprintln!("Falling back to manual capture: press F12 to capture input.");
            }
        }
        match receiver.recv_timeout(IDLE_POLL) {
            Ok(HostEvent::Keyboard {
                events,
                received_at,
                kernel_timestamp,
            }) => {
                let _phase = LoopPhaseTimer::new("keyboard");
                let mut submitted = false;
                for event in events {
                    if trace_input {
                        println!(
                            "[trace] host keyboard key={} transition={:?}",
                            event.key.profile_name(),
                            event.transition
                        );
                    }
                    if event.key == HostKey::F11 {
                        if event.transition == KeyTransition::Pressed && input_active {
                            // Manual Aim/UI-cursor switch. This is the only way
                            // into UI mode for always-on aim profiles (no
                            // toggle key), so it must win over the automatic
                            // mode sync until pressed again.
                            mode = if mode == InputMode::Aim {
                                InputMode::Ui
                            } else {
                                InputMode::Aim
                            };
                            mode_manually_overridden = true;
                            apply_input_mode(
                                runtime,
                                input_readers,
                                &mut overlay,
                                mode,
                                grab_allowed,
                                true,
                                &virtual_cursor,
                            )?;
                            println!("Input mode switched to {mode} (F11).");
                        }
                        continue;
                    }
                    if event.key == HostKey::F12 {
                        if event.transition == KeyTransition::Pressed {
                            control_pressed = false;
                            if input_active {
                                manually_released = true;
                                input_active = false;
                                // Keep the mode in effect (including a manual
                                // F11 override) so recapture restores it.
                                toggle_aim_active_before_release = runtime.aim_active();
                                release_input_to_os(runtime, input_readers, &mut overlay)?;
                                println!(
                                    "Manual capture: input released. Alt+Tab is available; focus Waydroid and press F12 to recapture."
                                );
                            } else if manually_released && (waydroid_focused || !focus_protected) {
                                input_readers.set_keyboard_capture(true)?;
                                apply_input_mode(
                                    runtime,
                                    input_readers,
                                    &mut overlay,
                                    mode,
                                    grab_allowed,
                                    toggle_aim_active_before_release,
                                    &virtual_cursor,
                                )?;
                                input_active = true;
                                manually_released = false;
                                println!(
                                    "Manual capture: Waydroid input recaptured ({mode} mode)."
                                );
                            } else if manually_released {
                                println!("Manual capture: focus Waydroid before pressing F12.");
                            }
                        }
                        continue;
                    }
                    if input_active {
                        if event.key.profile_name() == "ctrl" {
                            control_pressed = event.transition != KeyTransition::Released;
                        }
                        if control_pressed
                            && event.key.profile_name() == "esc"
                            && event.transition == KeyTransition::Pressed
                        {
                            return Ok(EventLoopExit::ExitHotkeyRequested);
                        }
                        if control_pressed
                            && event.key.profile_name() == "c"
                            && event.transition == KeyTransition::Pressed
                        {
                            return Ok(EventLoopExit::InterruptRequested);
                        }
                        submitted |= runtime.handle_keyboard(event)?;
                    }
                }
                if submitted {
                    runtime.record_pipeline_latency(received_at.elapsed(), kernel_timestamp);
                }
                // Sync the input mode with aim state: pressing a toggle key
                // flips between aim and UI cursor mode. A manual F11 choice
                // takes precedence until the next F11.
                if input_active && !mode_manually_overridden {
                    let next_mode = desired_mode(runtime);
                    if next_mode != mode {
                        apply_input_mode(
                            runtime,
                            input_readers,
                            &mut overlay,
                            next_mode,
                            grab_allowed,
                            true,
                            &virtual_cursor,
                        )?;
                        mode = next_mode;
                        println!("Input mode switched to {mode}.");
                    }
                }
            }
            Ok(HostEvent::Mouse {
                events,
                received_at,
                kernel_timestamp,
            }) => {
                let queued = received_at.elapsed();
                if queued >= Duration::from_millis(100) {
                    eprintln!(
                        "[stall] mouse batch waited {:.1} ms in queue ({} events)",
                        queued.as_secs_f64() * 1000.0,
                        events.len()
                    );
                }
                let _phase = LoopPhaseTimer::new("mouse");
                // Aim mode feeds profile bindings; UI mode drives the virtual
                // cursor: deltas move the sprite, LMB becomes an Android touch.
                if !input_active {
                    // Idle.
                } else if mode == InputMode::Aim {
                    let mut submitted = false;
                    for event in events {
                        if trace_input {
                            println!("[trace] host mouse {event:?}");
                        }
                        submitted |= runtime.handle_mouse(event)?;
                    }
                    if submitted {
                        runtime.record_pipeline_latency(received_at.elapsed(), kernel_timestamp);
                    }
                } else {
                    let mut submitted = false;
                    let mut sprite_dirty = false;
                    for event in events {
                        if trace_input {
                            println!("[trace] host mouse {event:?}");
                        }
                        match event {
                            MouseEvent::Motion(motion) => {
                                virtual_cursor.move_by(
                                    motion.dx,
                                    motion.dy,
                                    surface_width,
                                    surface_height,
                                );
                                sprite_dirty = true;
                                if virtual_cursor.pressed {
                                    runtime.cursor_move(virtual_cursor.point())?;
                                    submitted = true;
                                }
                            }
                            MouseEvent::Button(button) if button.button == MouseButton::Left => {
                                match button.transition {
                                    MouseButtonTransition::Pressed => {
                                        virtual_cursor.pressed = true;
                                        runtime.cursor_down(virtual_cursor.point())?;
                                        submitted = true;
                                    }
                                    MouseButtonTransition::Released => {
                                        virtual_cursor.pressed = false;
                                        runtime.cursor_up()?;
                                        submitted = true;
                                    }
                                    MouseButtonTransition::Repeated => {}
                                }
                            }
                            MouseEvent::Button(_) | MouseEvent::Wheel(_) => {}
                        }
                    }
                    if sprite_dirty {
                        overlay.set_cursor(true, virtual_cursor.x, virtual_cursor.y);
                    }
                    if submitted {
                        runtime.record_pipeline_latency(received_at.elapsed(), kernel_timestamp);
                    }
                }
            }
            Ok(HostEvent::Cursor { event, received_at }) => {
                let _phase = LoopPhaseTimer::new("cursor-overlay");
                // Overlay clicks only produce Android touches in UI cursor mode.
                if input_active && mode == InputMode::Ui {
                    let submitted = match event {
                        crate::cursor_overlay::CursorEvent::Motion {
                            x,
                            y,
                            surface_width,
                            surface_height,
                        } => {
                            if let Some(point) =
                                runtime.cursor_point(x, y, surface_width, surface_height)
                            {
                                if trace_input {
                                    println!(
                                        "[trace] cursor motion surface=({x:.1},{y:.1}) android=({},{})",
                                        point.x, point.y
                                    );
                                }
                                runtime.cursor_move(point)?;
                                runtime.engine.state().is_active(runtime.cursor_contact)
                            } else {
                                false
                            }
                        }
                        crate::cursor_overlay::CursorEvent::LeftButton {
                            pressed,
                            x,
                            y,
                            surface_width,
                            surface_height,
                        } => {
                            if let Some(point) =
                                runtime.cursor_point(x, y, surface_width, surface_height)
                            {
                                if trace_input {
                                    println!(
                                        "[trace] cursor left pressed={pressed} surface=({x:.1},{y:.1}) android=({},{})",
                                        point.x, point.y
                                    );
                                }
                                if pressed {
                                    runtime.cursor_down(point)?;
                                } else {
                                    runtime.cursor_up()?;
                                }
                                true
                            } else {
                                false
                            }
                        }
                    };
                    if submitted {
                        runtime.record_pipeline_latency(received_at.elapsed(), None);
                    }
                }
            }
            Ok(HostEvent::ReaderFailed { reader, message }) => {
                return Err(
                    io::Error::other(format!("{reader} input reader failed: {message}")).into(),
                );
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other(
                    "all input readers disconnected unexpectedly while the game session was running",
                )
                .into());
            }
        }
        if input_active {
            let _phase = LoopPhaseTimer::with_budget("runtime tick", Duration::from_millis(20));
            runtime.tick()?;
        }
        if let Some(spec) = options.profile_reload.as_mut() {
            if loop_started_at.elapsed() >= next_profile_check {
                next_profile_check = loop_started_at.elapsed() + PROFILE_RELOAD_POLL;
                let plan_swapped = reload_profile_if_changed(runtime, spec);
                if plan_swapped && input_active {
                    // Rebind the current mode to the fresh plan: always-on
                    // aims reactivate, UI cursor stays put.
                    apply_input_mode(
                        runtime,
                        input_readers,
                        &mut overlay,
                        mode,
                        grab_allowed,
                        false,
                        &virtual_cursor,
                    )?;
                }
            }
        }
        if exit_after.is_some_and(|limit| loop_started_at.elapsed() >= limit) {
            return Ok(EventLoopExit::TimeLimitReached);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedControlInput {
    Key(HostKeyName),
    KeyCluster {
        up: HostKeyName,
        left: HostKeyName,
        down: HostKeyName,
        right: HostKeyName,
    },
    MouseButton(HostMouseButton),
    MouseMove,
}

impl ResolvedControlInput {
    fn from_profile(input: &InputV2) -> Option<Self> {
        match input {
            InputV2::Key { key } => HostKeyName::parse(key).map(Self::Key),
            InputV2::KeyCluster {
                up,
                left,
                down,
                right,
            } => Some(Self::KeyCluster {
                up: HostKeyName::parse(up)?,
                left: HostKeyName::parse(left)?,
                down: HostKeyName::parse(down)?,
                right: HostKeyName::parse(right)?,
            }),
            InputV2::MouseButton { button } => parse_mouse_button(button).map(Self::MouseButton),
            InputV2::MouseMove => Some(Self::MouseMove),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct LayerControl {
    mask: u64,
    mode: LayerMode,
}

struct UnifiedRuntime<I: TouchInjector> {
    plan: RuntimeControlPlan,
    engine: TouchEngine<I>,
    control_inputs: Vec<ResolvedControlInput>,
    layer_controls: [Option<LayerControl>; 57],
    directions: Vec<DirectionalInput>,
    desired_directions: Vec<DirectionalInput>,
    active_bindings: Vec<bool>,
    desired_bindings: Vec<bool>,
    selected_input_layers: [Option<LayerId>; PHYSICAL_INPUT_COUNT],
    point_contacts: Vec<Option<ContactId>>,
    cursor_contact: ContactId,
    aim_controllers: Vec<Option<MouseAimController>>,
    aim_toggle_keys: Vec<Option<HostKeyName>>,
    last_joystick_frame: Vec<Option<Duration>>,
    last_walk_scales: Vec<f64>,
    /// Sustained taps ("tap or hold"): Some while the key is physically held.
    sustained_taps: Vec<Option<ContactId>>,
    /// Toggle-mode walk state per joystick binding (slow_mode: toggle).
    walk_toggled: Vec<bool>,
    pending_radials: Vec<PendingRadial>,
    active_layers: u64,
    held_keys: ModifierMask,
    held_modifiers: ModifierMask,
    held_mouse_buttons: u8,
    started_at: Instant,
    pipeline_latencies: Vec<Duration>,
    kernel_latencies: Vec<Duration>,
    rejected_kernel_timestamps: u64,
    trace_input: bool,
}

impl<I: TouchInjector> UnifiedRuntime<I> {
    fn new(plan: RuntimeControlPlan, injector: I, trace_input: bool) -> GameSessionResult<Self> {
        const MAX_LATENCY_SAMPLES: usize = 100_000;

        let mut next_contact = plan
            .controls
            .iter()
            .filter_map(|control| match &control.action {
                RuntimeControlAction::VirtualJoystick { joystick, .. } => {
                    Some(joystick.contact_id().get())
                }
                RuntimeControlAction::MouseAim { aim, settings } => Some(
                    aim.contact_id()
                        .get()
                        .max(settings.alternate_contact_id.get()),
                ),
                RuntimeControlAction::Tap { .. }
                | RuntimeControlAction::Hold { .. }
                | RuntimeControlAction::RadialSelect { .. } => None,
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let control_count = plan.controls.len();
        let mut control_inputs = Vec::with_capacity(control_count);
        let mut point_contacts = Vec::with_capacity(control_count);
        let mut aim_controllers = Vec::with_capacity(control_count);
        let mut aim_toggle_keys = Vec::with_capacity(control_count);
        for control in &plan.controls {
            control_inputs.push(
                ResolvedControlInput::from_profile(&control.input).ok_or_else(|| {
                    io::Error::other("validated control input did not resolve at runtime")
                })?,
            );
            if matches!(
                control.action,
                RuntimeControlAction::Tap { .. }
                    | RuntimeControlAction::Hold { .. }
                    | RuntimeControlAction::RadialSelect { .. }
            ) {
                point_contacts.push(Some(ContactId::new(next_contact)));
                next_contact = next_contact.saturating_add(1);
            } else {
                point_contacts.push(None);
            }
            if let RuntimeControlAction::MouseAim { aim, settings } = &control.action {
                aim_controllers.push(Some(MouseAimController::new(
                    aim.clone(),
                    settings.clone(),
                )?));
                aim_toggle_keys.push(settings.toggle_key.as_deref().and_then(HostKeyName::parse));
            } else {
                aim_controllers.push(None);
                aim_toggle_keys.push(None);
            }
        }

        let cursor_contact = ContactId::new(next_contact);

        let mut layer_controls = [None; 57];
        for layer in &plan.layers {
            let mask = layer_mask_bit(layer.id)
                .ok_or_else(|| io::Error::other("declared runtime layer id is out of range"))?;
            layer_controls[usize::from(layer.activation_key.index())] = Some(LayerControl {
                mask,
                mode: layer.mode,
            });
        }

        Ok(Self {
            plan,
            engine: TouchEngine::new(injector),
            control_inputs,
            layer_controls,
            directions: vec![DirectionalInput::default(); control_count],
            desired_directions: vec![DirectionalInput::default(); control_count],
            active_bindings: vec![false; control_count],
            desired_bindings: vec![false; control_count],
            selected_input_layers: [None; PHYSICAL_INPUT_COUNT],
            point_contacts,
            cursor_contact,
            aim_controllers,
            aim_toggle_keys,
            last_joystick_frame: vec![None; control_count],
            last_walk_scales: vec![1.0; control_count],
            sustained_taps: vec![None; control_count],
            walk_toggled: vec![false; control_count],
            pending_radials: Vec::new(),
            active_layers: 0,
            held_keys: ModifierMask::EMPTY,
            held_modifiers: ModifierMask::EMPTY,
            held_mouse_buttons: 0,
            started_at: Instant::now(),
            pipeline_latencies: Vec::with_capacity(MAX_LATENCY_SAMPLES),
            kernel_latencies: Vec::with_capacity(MAX_LATENCY_SAMPLES),
            rejected_kernel_timestamps: 0,
            trace_input,
        })
    }

    /// Swaps in a freshly compiled control plan (live profile reload).
    ///
    /// Every contact is cancelled and all per-control state is rebuilt, so
    /// bindings added or moved in the editor apply without restarting the
    /// game. Physical keys held during the swap are forgotten; the next key
    /// event re-establishes them.
    fn replace_plan(&mut self, plan: RuntimeControlPlan) -> GameSessionResult<()> {
        self.suspend()?;

        let mut next_contact = plan
            .controls
            .iter()
            .filter_map(|control| match &control.action {
                RuntimeControlAction::VirtualJoystick { joystick, .. } => {
                    Some(joystick.contact_id().get())
                }
                RuntimeControlAction::MouseAim { aim, settings } => Some(
                    aim.contact_id()
                        .get()
                        .max(settings.alternate_contact_id.get()),
                ),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let control_count = plan.controls.len();
        let mut control_inputs = Vec::with_capacity(control_count);
        let mut point_contacts = Vec::with_capacity(control_count);
        let mut aim_controllers = Vec::with_capacity(control_count);
        let mut aim_toggle_keys = Vec::with_capacity(control_count);
        for control in &plan.controls {
            control_inputs.push(
                ResolvedControlInput::from_profile(&control.input).ok_or_else(|| {
                    io::Error::other("validated control input did not resolve at runtime")
                })?,
            );
            if matches!(
                control.action,
                RuntimeControlAction::Tap { .. }
                    | RuntimeControlAction::Hold { .. }
                    | RuntimeControlAction::RadialSelect { .. }
            ) {
                point_contacts.push(Some(ContactId::new(next_contact)));
                next_contact = next_contact.saturating_add(1);
            } else {
                point_contacts.push(None);
            }
            if let RuntimeControlAction::MouseAim { aim, settings } = &control.action {
                aim_controllers.push(Some(MouseAimController::new(
                    aim.clone(),
                    settings.clone(),
                )?));
                aim_toggle_keys.push(settings.toggle_key.as_deref().and_then(HostKeyName::parse));
            } else {
                aim_controllers.push(None);
                aim_toggle_keys.push(None);
            }
        }

        let mut layer_controls = [None; 57];
        for layer in &plan.layers {
            let mask = layer_mask_bit(layer.id)
                .ok_or_else(|| io::Error::other("declared runtime layer id is out of range"))?;
            layer_controls[usize::from(layer.activation_key.index())] = Some(LayerControl {
                mask,
                mode: layer.mode,
            });
        }

        self.plan = plan;
        self.control_inputs = control_inputs;
        self.layer_controls = layer_controls;
        self.directions = vec![DirectionalInput::default(); control_count];
        self.desired_directions = vec![DirectionalInput::default(); control_count];
        self.active_bindings = vec![false; control_count];
        self.desired_bindings = vec![false; control_count];
        self.point_contacts = point_contacts;
        self.cursor_contact = ContactId::new(next_contact);
        self.aim_controllers = aim_controllers;
        self.aim_toggle_keys = aim_toggle_keys;
        self.last_joystick_frame = vec![None; control_count];
        self.last_walk_scales = vec![1.0; control_count];
        self.sustained_taps = vec![None; control_count];
        self.walk_toggled = vec![false; control_count];
        Ok(())
    }

    fn start(&mut self) -> GameSessionResult<()> {
        let now = self.now();
        for controller in self.aim_controllers.iter_mut().flatten() {
            if controller.settings().toggle_key.is_none() {
                controller.activate(&mut self.engine, now)?;
            }
        }
        Ok(())
    }

    fn has_toggle_key_aim(&self) -> bool {
        self.aim_controllers
            .iter()
            .flatten()
            .any(|c| c.settings().toggle_key.is_some())
    }

    /// True when any aim controller runs without a toggle key.
    ///
    /// Always-on aim starts active, so the session boots in aim mode; profiles
    /// whose aim controllers all declare toggle keys boot in UI cursor mode
    /// instead and switch to aim mode when a toggle key is pressed.
    fn has_always_on_aim(&self) -> bool {
        self.aim_controllers
            .iter()
            .flatten()
            .any(|c| c.settings().toggle_key.is_none())
    }

    /// Restore aim controllers for aim mode.
    ///
    /// Always-on controllers activate unconditionally; toggle-key controllers
    /// activate only when `reactivate_toggle_aim` is set (i.e. aim was active
    /// before the release being undone). This avoids wrongly enabling a
    /// toggle-key controller when restoring aim after a transition that left
    /// it inactive by the user's choice.
    fn restore_aim(&mut self, reactivate_toggle_aim: bool) -> GameSessionResult<()> {
        let now = self.now();
        for controller in self.aim_controllers.iter_mut().flatten() {
            let wanted = controller.settings().toggle_key.is_none() || reactivate_toggle_aim;
            if wanted && !controller.is_active() {
                controller.activate(&mut self.engine, now)?;
            }
        }
        Ok(())
    }

    /// Deactivate all aim controllers (UI cursor mode entry).
    fn set_aim_enabled(&mut self, enabled: bool) -> GameSessionResult<()> {
        if enabled {
            return self.restore_aim(true);
        }
        for controller in self.aim_controllers.iter_mut().flatten() {
            if controller.is_active() {
                controller.deactivate(&mut self.engine)?;
            }
        }
        Ok(())
    }

    /// Release any physically-held mouse buttons and their bindings.
    ///
    /// Called when leaving aim mode: raw mouse events stop being forwarded, so
    /// a pending release would never arrive and a held fire binding would stay
    /// stuck. Zeroing the held-button mask and reconciling lifts those contacts.
    fn release_mouse_buttons(&mut self) -> GameSessionResult<()> {
        if self.held_mouse_buttons == 0 {
            return Ok(());
        }
        self.held_mouse_buttons = 0;
        for controller in self.aim_controllers.iter_mut().flatten() {
            controller.set_ads_active(false);
        }
        let now = self.now();
        self.reconcile_continuous(now)?;
        Ok(())
    }

    fn aim_active(&self) -> bool {
        self.aim_controllers
            .iter()
            .flatten()
            .any(MouseAimController::is_active)
    }

    /// The session render resolution: also the coordinate space of the
    /// virtual cursor.
    fn resolution(&self) -> (u32, u32) {
        (self.plan.resolution.width, self.plan.resolution.height)
    }

    fn cursor_point(
        &self,
        x: f64,
        y: f64,
        surface_width: u32,
        surface_height: u32,
    ) -> Option<Point> {
        if surface_width == 0
            || surface_height == 0
            || self.plan.resolution.width == 0
            || self.plan.resolution.height == 0
        {
            return None;
        }

        let max_x = self.plan.resolution.width.saturating_sub(1);
        let max_y = self.plan.resolution.height.saturating_sub(1);
        let x = x.clamp(0.0, f64::from(surface_width.saturating_sub(1)));
        let y = y.clamp(0.0, f64::from(surface_height.saturating_sub(1)));

        Some(Point {
            x: ((x * f64::from(self.plan.resolution.width) / f64::from(surface_width)).floor()
                as u32)
                .min(max_x),
            y: ((y * f64::from(self.plan.resolution.height) / f64::from(surface_height)).floor()
                as u32)
                .min(max_y),
        })
    }

    fn cursor_down(&mut self, position: Point) -> GameSessionResult<()> {
        if self.engine.state().is_active(self.cursor_contact) {
            self.engine.move_contact(self.cursor_contact, position)?;
        } else {
            self.engine.begin_contact(self.cursor_contact, position)?;
        }
        Ok(())
    }

    fn cursor_move(&mut self, position: Point) -> GameSessionResult<()> {
        if self.engine.state().is_active(self.cursor_contact) {
            self.engine.move_contact(self.cursor_contact, position)?;
        }
        Ok(())
    }

    fn cursor_up(&mut self) -> GameSessionResult<()> {
        if self.engine.state().is_active(self.cursor_contact) {
            self.engine.end_contact(self.cursor_contact)?;
        }
        Ok(())
    }

    fn handle_keyboard(&mut self, event: HostKeyEvent) -> GameSessionResult<bool> {
        let pressed = match event.transition {
            KeyTransition::Pressed => true,
            KeyTransition::Released => false,
            KeyTransition::Repeated => return Ok(false),
        };
        let Some(key) = runtime_key(event.key) else {
            return Ok(false);
        };
        if pressed {
            self.held_keys.insert(key);
        } else {
            self.held_keys.remove(key);
        }
        self.update_walk_toggles(key, pressed);

        let activation_consumed = if let Some(layer) = self.layer_controls[key.index() as usize] {
            match (layer.mode, pressed) {
                (LayerMode::Hold, true) => self.active_layers |= layer.mask,
                (LayerMode::Hold, false) => self.active_layers &= !layer.mask,
                (LayerMode::Toggle, true) => self.active_layers ^= layer.mask,
                (LayerMode::Toggle, false) => {}
            }
            true
        } else {
            false
        };

        if self.plan.modifier_keys.contains(key) {
            if pressed {
                self.held_modifiers.insert(key);
            } else {
                self.held_modifiers.remove(key);
            }
        }

        let now = self.now();
        let mut submitted = self.reconcile_continuous(now)?;
        if !pressed {
            submitted |= self.release_sustained_taps(RuntimePhysicalInput::Key(key))?;
        }
        if activation_consumed {
            return Ok(submitted);
        }

        if pressed {
            for index in 0..self.aim_controllers.len() {
                if self.aim_toggle_keys[index] != Some(key) {
                    continue;
                }
                let controller = self.aim_controllers[index]
                    .as_mut()
                    .expect("aim toggle keys belong to aim controllers");
                let update = controller.toggle(&mut self.engine, now)?;
                submitted |= update != MouseAimUpdate::Ignored;
                if self.trace_input {
                    println!(
                        "[trace] runtime aim binding={} toggle={update:?}",
                        self.plan.controls[index].name
                    );
                }
            }
            submitted |= self.dispatch_keyboard_press(key, now)?;
        }
        Ok(submitted)
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> GameSessionResult<bool> {
        let now = self.now();
        let mut submitted = false;
        match event {
            MouseEvent::Motion(RelativeMouseMotion { dx, dy }) if dx != 0 || dy != 0 => {
                for (index, controller) in self.aim_controllers.iter_mut().enumerate() {
                    let Some(controller) = controller else {
                        continue;
                    };
                    let update =
                        controller.move_by(&mut self.engine, MouseAimDelta::new(dx, dy), now)?;
                    submitted |= update != MouseAimUpdate::Ignored;
                    if self.trace_input {
                        println!(
                            "[trace] runtime aim binding={} delta=({dx},{dy}) update={update:?} recenter_count={}",
                            self.plan.controls[index].name,
                            controller.recenter_count(),
                        );
                    }
                }
            }
            MouseEvent::Button(button_event) => {
                let pressed = match button_event.transition {
                    MouseButtonTransition::Pressed => true,
                    MouseButtonTransition::Released => false,
                    MouseButtonTransition::Repeated => return Ok(false),
                };
                let button = runtime_mouse_button(button_event.button);
                set_mouse_button_held(&mut self.held_mouse_buttons, button, pressed);
                if button == HostMouseButton::Right {
                    for controller in self.aim_controllers.iter_mut().flatten() {
                        controller.set_ads_active(pressed);
                    }
                }
                submitted |= self.reconcile_continuous(now)?;
                if pressed {
                    submitted |= self.dispatch_mouse_press(button)?;
                } else {
                    submitted |=
                        self.release_sustained_taps(RuntimePhysicalInput::MouseButton(button))?;
                }
            }
            MouseEvent::Motion(_) | MouseEvent::Wheel(_) => {}
        }
        Ok(submitted)
    }

    fn reconcile_continuous(&mut self, now: Duration) -> GameSessionResult<bool> {
        self.refresh_selected_input_layers();
        for index in 0..self.plan.controls.len() {
            let control = &self.plan.controls[index];
            let input = self.control_inputs[index];
            self.desired_bindings[index] = false;
            self.desired_directions[index] = DirectionalInput::default();
            match (&control.action, input) {
                (RuntimeControlAction::Hold { .. }, ResolvedControlInput::Key(key)) => {
                    let physical = RuntimePhysicalInput::Key(key);
                    self.desired_bindings[index] = self.layer_controls[key.index() as usize]
                        .is_none()
                        && self.held_keys.contains(key)
                        && binding_available(
                            control,
                            physical,
                            self.active_layers,
                            self.held_modifiers,
                            &self.selected_input_layers,
                        );
                }
                (RuntimeControlAction::Hold { .. }, ResolvedControlInput::MouseButton(button)) => {
                    let physical = RuntimePhysicalInput::MouseButton(button);
                    self.desired_bindings[index] =
                        mouse_button_held(self.held_mouse_buttons, button)
                            && binding_available(
                                control,
                                physical,
                                self.active_layers,
                                self.held_modifiers,
                                &self.selected_input_layers,
                            );
                }
                (
                    RuntimeControlAction::VirtualJoystick {
                        mode: JoystickMode::Hold,
                        ..
                    },
                    ResolvedControlInput::KeyCluster {
                        up,
                        left,
                        down,
                        right,
                    },
                ) => {
                    self.desired_directions[index] = DirectionalInput {
                        up: self.layer_controls[up.index() as usize].is_none()
                            && self.held_keys.contains(up)
                            && binding_available(
                                control,
                                RuntimePhysicalInput::Key(up),
                                self.active_layers,
                                self.held_modifiers,
                                &self.selected_input_layers,
                            ),
                        left: self.layer_controls[left.index() as usize].is_none()
                            && self.held_keys.contains(left)
                            && binding_available(
                                control,
                                RuntimePhysicalInput::Key(left),
                                self.active_layers,
                                self.held_modifiers,
                                &self.selected_input_layers,
                            ),
                        down: self.layer_controls[down.index() as usize].is_none()
                            && self.held_keys.contains(down)
                            && binding_available(
                                control,
                                RuntimePhysicalInput::Key(down),
                                self.active_layers,
                                self.held_modifiers,
                                &self.selected_input_layers,
                            ),
                        right: self.layer_controls[right.index() as usize].is_none()
                            && self.held_keys.contains(right)
                            && binding_available(
                                control,
                                RuntimePhysicalInput::Key(right),
                                self.active_layers,
                                self.held_modifiers,
                                &self.selected_input_layers,
                            ),
                    };
                    self.desired_bindings[index] =
                        self.desired_directions[index] != DirectionalInput::default();
                }
                (
                    RuntimeControlAction::VirtualJoystick {
                        mode: JoystickMode::Toggle,
                        ..
                    },
                    ResolvedControlInput::KeyCluster {
                        up,
                        left,
                        down,
                        right,
                    },
                ) => {
                    let mut desired = self.directions[index];
                    desired.up &= binding_available(
                        control,
                        RuntimePhysicalInput::Key(up),
                        self.active_layers,
                        self.held_modifiers,
                        &self.selected_input_layers,
                    );
                    desired.left &= binding_available(
                        control,
                        RuntimePhysicalInput::Key(left),
                        self.active_layers,
                        self.held_modifiers,
                        &self.selected_input_layers,
                    );
                    desired.down &= binding_available(
                        control,
                        RuntimePhysicalInput::Key(down),
                        self.active_layers,
                        self.held_modifiers,
                        &self.selected_input_layers,
                    );
                    desired.right &= binding_available(
                        control,
                        RuntimePhysicalInput::Key(right),
                        self.active_layers,
                        self.held_modifiers,
                        &self.selected_input_layers,
                    );
                    self.desired_directions[index] = desired;
                    self.desired_bindings[index] = desired != DirectionalInput::default();
                }
                _ => {}
            }
        }

        let mut submitted = false;
        for index in 0..self.plan.controls.len() {
            let RuntimeControlAction::VirtualJoystick { joystick, .. } =
                &self.plan.controls[index].action
            else {
                continue;
            };
            let retained =
                direction_intersection(self.directions[index], self.desired_directions[index]);
            let scale = self.joystick_walk_scale(index);
            if self.directions[index] == retained && self.last_walk_scales[index] == scale {
                continue;
            }
            let frame_submitted =
                apply_joystick_input(&mut self.engine, joystick, retained, scale)?;
            self.last_walk_scales[index] = scale;
            submitted |= frame_submitted;
            if frame_submitted {
                self.last_joystick_frame[index] = Some(now);
            }
            self.directions[index] = retained;
            self.active_bindings[index] = retained != DirectionalInput::default();
            trace_joystick(
                self.trace_input,
                &self.plan.controls[index].name,
                joystick,
                retained,
                frame_submitted,
                &self.engine,
            );
        }

        for index in 0..self.plan.controls.len() {
            let RuntimeControlAction::Hold { point } = &self.plan.controls[index].action else {
                continue;
            };
            if self.active_bindings[index] && !self.desired_bindings[index] {
                let contact_id = point_contact(self.point_contacts[index])?;
                submitted |= set_hold_binding(
                    &mut self.engine,
                    contact_id,
                    &self.plan.controls[index].name,
                    *point,
                    false,
                    self.trace_input,
                )?;
                self.active_bindings[index] = false;
            }
        }

        for index in 0..self.plan.controls.len() {
            match &self.plan.controls[index].action {
                RuntimeControlAction::Hold { point }
                    if !self.active_bindings[index] && self.desired_bindings[index] =>
                {
                    let contact_id = point_contact(self.point_contacts[index])?;
                    submitted |= set_hold_binding(
                        &mut self.engine,
                        contact_id,
                        &self.plan.controls[index].name,
                        *point,
                        true,
                        self.trace_input,
                    )?;
                    self.active_bindings[index] = true;
                }
                RuntimeControlAction::VirtualJoystick { joystick, .. }
                    if self.directions[index] != self.desired_directions[index]
                        || self.last_walk_scales[index] != self.joystick_walk_scale(index) =>
                {
                    let desired = self.desired_directions[index];
                    let scale = self.joystick_walk_scale(index);
                    let frame_submitted =
                        apply_joystick_input(&mut self.engine, joystick, desired, scale)?;
                    self.last_walk_scales[index] = scale;
                    submitted |= frame_submitted;
                    if frame_submitted {
                        self.last_joystick_frame[index] = Some(now);
                    }
                    self.directions[index] = desired;
                    self.active_bindings[index] = desired != DirectionalInput::default();
                    trace_joystick(
                        self.trace_input,
                        &self.plan.controls[index].name,
                        joystick,
                        desired,
                        frame_submitted,
                        &self.engine,
                    );
                }
                _ => {}
            }
        }
        Ok(submitted)
    }

    fn refresh_selected_input_layers(&mut self) {
        self.selected_input_layers.fill(None);
        for (index, control) in self.plan.controls.iter().enumerate() {
            if control.layer == LayerId::BASE {
                continue;
            }
            match self.control_inputs[index] {
                ResolvedControlInput::Key(key) => select_input_layer(
                    &mut self.selected_input_layers,
                    control,
                    RuntimePhysicalInput::Key(key),
                    self.active_layers,
                ),
                ResolvedControlInput::KeyCluster {
                    up,
                    left,
                    down,
                    right,
                } => {
                    for key in [up, left, down, right] {
                        select_input_layer(
                            &mut self.selected_input_layers,
                            control,
                            RuntimePhysicalInput::Key(key),
                            self.active_layers,
                        );
                    }
                }
                ResolvedControlInput::MouseButton(button) => select_input_layer(
                    &mut self.selected_input_layers,
                    control,
                    RuntimePhysicalInput::MouseButton(button),
                    self.active_layers,
                ),
                ResolvedControlInput::MouseMove => {}
            }
        }
    }

    fn dispatch_keyboard_press(
        &mut self,
        key: HostKeyName,
        now: Duration,
    ) -> GameSessionResult<bool> {
        let physical = RuntimePhysicalInput::Key(key);
        let mut submitted = false;
        for index in 0..self.plan.controls.len() {
            let control = &self.plan.controls[index];
            match (self.control_inputs[index], &control.action) {
                (
                    ResolvedControlInput::Key(binding_key),
                    RuntimeControlAction::Tap { point, sustain },
                ) if binding_key == key
                    && binding_available(
                        control,
                        physical,
                        self.active_layers,
                        self.held_modifiers,
                        &self.selected_input_layers,
                    ) =>
                {
                    let contact_id = point_contact(self.point_contacts[index])?;
                    let binding_name = control.name.clone();
                    let point = *point;
                    if *sustain {
                        // Tap-or-hold: contact goes down NOW and stays until
                        // key release; a quick press reads as a plain tap.
                        if self.sustained_taps[index].is_some() {
                            continue;
                        }
                        engine_submit_down(&mut self.engine, contact_id, point)?;
                        self.sustained_taps[index] = Some(contact_id);
                        if self.trace_input {
                            println!(
                                "[trace] runtime tap binding={binding_name} contact={} down=({},{}) sustained",
                                contact_id.get(),
                                point.x,
                                point.y
                            );
                        }
                    } else {
                        tap_binding(
                            &mut self.engine,
                            contact_id,
                            &binding_name,
                            point,
                            self.trace_input,
                        )?;
                    }
                    submitted = true;
                }
                (
                    ResolvedControlInput::Key(binding_key),
                    RuntimeControlAction::RadialSelect { center, target },
                ) if binding_key == key
                    && binding_available(
                        control,
                        physical,
                        self.active_layers,
                        self.held_modifiers,
                        &self.selected_input_layers,
                    ) =>
                {
                    let contact_id = point_contact(self.point_contacts[index])?;
                    let binding_name = control.name.clone();
                    let (center, target) = (*center, *target);
                    // A re-press while the previous gesture is still in
                    // flight would begin an already-active contact and kill
                    // the session; finish the old gesture first.
                    self.cancel_pending_radial(index)?;
                    radial_down_phase(
                        &mut self.engine,
                        contact_id,
                        &binding_name,
                        center,
                        self.trace_input,
                    )?;
                    self.pending_radials.push(PendingRadial {
                        binding_index: index,
                        contact_id,
                        binding_name,
                        target,
                        phase: RadialPhase::Move,
                        due_at: now.saturating_add(RADIAL_DOWN_DWELL),
                    });
                    submitted = true;
                }
                (
                    ResolvedControlInput::KeyCluster {
                        up,
                        left,
                        down,
                        right,
                    },
                    RuntimeControlAction::VirtualJoystick {
                        joystick,
                        mode: JoystickMode::Toggle,
                        ..
                    },
                ) => {
                    let mut desired = self.directions[index];
                    let mut changed = false;
                    if up == key
                        && binding_available(
                            control,
                            physical,
                            self.active_layers,
                            self.held_modifiers,
                            &self.selected_input_layers,
                        )
                    {
                        desired.up = !desired.up;
                        changed = true;
                    }
                    if left == key
                        && binding_available(
                            control,
                            physical,
                            self.active_layers,
                            self.held_modifiers,
                            &self.selected_input_layers,
                        )
                    {
                        desired.left = !desired.left;
                        changed = true;
                    }
                    if down == key
                        && binding_available(
                            control,
                            physical,
                            self.active_layers,
                            self.held_modifiers,
                            &self.selected_input_layers,
                        )
                    {
                        desired.down = !desired.down;
                        changed = true;
                    }
                    if right == key
                        && binding_available(
                            control,
                            physical,
                            self.active_layers,
                            self.held_modifiers,
                            &self.selected_input_layers,
                        )
                    {
                        desired.right = !desired.right;
                        changed = true;
                    }
                    if !changed {
                        continue;
                    }
                    let scale = self.joystick_walk_scale(index);
                    let frame_submitted =
                        apply_joystick_input(&mut self.engine, joystick, desired, scale)?;
                    self.last_walk_scales[index] = scale;
                    submitted |= frame_submitted;
                    if frame_submitted {
                        self.last_joystick_frame[index] = Some(now);
                    }
                    self.directions[index] = desired;
                    self.active_bindings[index] = desired != DirectionalInput::default();
                    trace_joystick(
                        self.trace_input,
                        &control.name,
                        joystick,
                        desired,
                        frame_submitted,
                        &self.engine,
                    );
                }
                _ => {}
            }
        }
        Ok(submitted)
    }

    fn dispatch_mouse_press(&mut self, button: HostMouseButton) -> GameSessionResult<bool> {
        let physical = RuntimePhysicalInput::MouseButton(button);
        let mut submitted = false;
        for index in 0..self.plan.controls.len() {
            let control = &self.plan.controls[index];
            if self.control_inputs[index] != ResolvedControlInput::MouseButton(button)
                || !binding_available(
                    control,
                    physical,
                    self.active_layers,
                    self.held_modifiers,
                    &self.selected_input_layers,
                )
            {
                continue;
            }
            match &control.action {
                RuntimeControlAction::Tap { point, sustain } => {
                    let contact_id = point_contact(self.point_contacts[index])?;
                    let binding_name = control.name.clone();
                    let point = *point;
                    if *sustain {
                        if self.sustained_taps[index].is_some() {
                            continue;
                        }
                        engine_submit_down(&mut self.engine, contact_id, point)?;
                        self.sustained_taps[index] = Some(contact_id);
                    } else {
                        tap_binding(
                            &mut self.engine,
                            contact_id,
                            &binding_name,
                            point,
                            self.trace_input,
                        )?;
                    }
                    submitted = true;
                }
                RuntimeControlAction::RadialSelect { center, target } => {
                    let contact_id = point_contact(self.point_contacts[index])?;
                    let binding_name = control.name.clone();
                    let (center, target) = (*center, *target);
                    // Same re-press safety as keyboard-triggered gestures.
                    self.cancel_pending_radial(index)?;
                    radial_down_phase(
                        &mut self.engine,
                        contact_id,
                        &binding_name,
                        center,
                        self.trace_input,
                    )?;
                    self.pending_radials.push(PendingRadial {
                        binding_index: index,
                        contact_id,
                        binding_name,
                        target,
                        phase: RadialPhase::Move,
                        due_at: self.now().saturating_add(RADIAL_DOWN_DWELL),
                    });
                    submitted = true;
                }
                _ => {}
            }
        }
        Ok(submitted)
    }

    /// Walk-key scale for the binding: `walk_scale` while its slow key is
    /// held, full deflection otherwise.
    fn joystick_walk_scale(&self, index: usize) -> f64 {
        let RuntimeControlAction::VirtualJoystick {
            slow_key,
            walk_scale,
            walk_mode,
            ..
        } = &self.plan.controls[index].action
        else {
            return 1.0;
        };
        let slow_active = match (slow_key, walk_mode) {
            (Some(key), JoystickWalkMode::Hold) => self.held_keys.contains(*key),
            (Some(_), JoystickWalkMode::Toggle) => self.walk_toggled[index],
            _ => false,
        };
        if slow_active {
            *walk_scale
        } else {
            1.0
        }
    }

    /// Flip toggle-walk for every joystick whose slow key was just pressed.
    fn update_walk_toggles(&mut self, key: HostKeyName, pressed: bool) {
        if !pressed {
            return;
        }
        for index in 0..self.plan.controls.len() {
            if let RuntimeControlAction::VirtualJoystick {
                slow_key: Some(slow_key),
                walk_mode: JoystickWalkMode::Toggle,
                ..
            } = &self.plan.controls[index].action
            {
                if *slow_key == key {
                    self.walk_toggled[index] = !self.walk_toggled[index];
                    if self.trace_input {
                        println!(
                            "[trace] runtime walk toggle {}={}",
                            self.plan.controls[index].name,
                            if self.walk_toggled[index] {
                                "on"
                            } else {
                                "off"
                            }
                        );
                    }
                }
            }
        }
    }

    /// Lift every sustained tap bound to `physical` (key/button released).
    fn release_sustained_taps(
        &mut self,
        physical: RuntimePhysicalInput,
    ) -> GameSessionResult<bool> {
        let mut submitted = false;
        for index in 0..self.plan.controls.len() {
            let Some(contact_id) = self.sustained_taps[index] else {
                continue;
            };
            let matches_input = match (&self.control_inputs[index], &physical) {
                (ResolvedControlInput::Key(bound), RuntimePhysicalInput::Key(pressed)) => {
                    bound == pressed
                }
                (
                    ResolvedControlInput::MouseButton(bound),
                    RuntimePhysicalInput::MouseButton(pressed),
                ) => bound == pressed,
                _ => false,
            };
            if !matches_input {
                continue;
            }
            self.sustained_taps[index] = None;
            if self.engine.state().is_active(contact_id) {
                self.engine.end_contact(contact_id)?;
                if self.trace_input {
                    println!(
                        "[trace] runtime sustained tap {}={} released",
                        self.plan.controls[index].name,
                        contact_id.get()
                    );
                }
                submitted = true;
            }
        }
        Ok(submitted)
    }

    fn tick(&mut self) -> GameSessionResult<()> {
        let now = self.now();
        for controller in self.aim_controllers.iter_mut().flatten() {
            controller.tick(&mut self.engine, now)?;
        }
        if !self.pending_radials.is_empty() {
            self.advance_pending_radials(now)?;
        }
        for (index, control) in self.plan.controls.iter().enumerate() {
            let RuntimeControlAction::VirtualJoystick {
                joystick,
                reaffirm_interval: Some(interval),
                ..
            } = &control.action
            else {
                continue;
            };
            let Some(position) = self.engine.state().position(joystick.contact_id()) else {
                continue;
            };
            if self.last_joystick_frame[index]
                .is_some_and(|last| now.saturating_sub(last) < *interval)
            {
                continue;
            }
            self.engine.move_contact(joystick.contact_id(), position)?;
            self.last_joystick_frame[index] = Some(now);
        }
        Ok(())
    }

    /// Finish an in-flight radial gesture for `binding_index` so a re-press
    /// can safely reuse the binding's contact id.
    fn cancel_pending_radial(&mut self, binding_index: usize) -> GameSessionResult<()> {
        let Some(position) = self
            .pending_radials
            .iter()
            .position(|pending| pending.binding_index == binding_index)
        else {
            return Ok(());
        };
        let pending = self.pending_radials.remove(position);
        if self.engine.state().is_active(pending.contact_id) {
            if let Err(error) = self.engine.end_contact(pending.contact_id) {
                if self.trace_input {
                    eprintln!(
                        "[trace] runtime radial {} cancel failed: {error}",
                        pending.binding_name
                    );
                }
            }
        }
        Ok(())
    }

    fn advance_pending_radials(&mut self, now: Duration) -> GameSessionResult<()> {
        let mut index = 0;
        while index < self.pending_radials.len() {
            let pending = &self.pending_radials[index];
            if now < pending.due_at {
                index += 1;
                continue;
            }
            if !self.engine.state().is_active(pending.contact_id) {
                // The contact died (suspend, cancel): drop the schedule.
                self.pending_radials.remove(index);
                continue;
            }
            let contact_id = pending.contact_id;
            let target = pending.target;
            let phase = pending.phase;
            let binding_name = pending.binding_name.clone();
            let failure = match phase {
                RadialPhase::Move => self
                    .engine
                    .move_contact(contact_id, target)
                    .map(|_| Some((RadialPhase::Release, now.saturating_add(RADIAL_MOVE_DWELL))))
                    .err(),
                RadialPhase::Release => self
                    .engine
                    .end_contact(contact_id)
                    .map(|_| Option::<(RadialPhase, Duration)>::None)
                    .err(),
            };
            if let Some(failure) = failure {
                // One broken gesture must not kill the session; abandon it.
                eprintln!(
                    "[radial] {binding_name} {phase:?} phase failed: {failure}; gesture dropped"
                );
                self.pending_radials.remove(index);
                continue;
            }
            if self.trace_input {
                println!(
                    "[trace] runtime radial binding={binding_name} contact={} {}=({},{})",
                    contact_id.get(),
                    if phase == RadialPhase::Move {
                        "move"
                    } else {
                        "up"
                    },
                    target.x,
                    target.y
                );
            }
            match phase {
                RadialPhase::Move => {
                    let pending = &mut self.pending_radials[index];
                    pending.phase = RadialPhase::Release;
                    pending.due_at = now.saturating_add(RADIAL_MOVE_DWELL);
                    index += 1;
                }
                RadialPhase::Release => {
                    self.pending_radials.remove(index);
                }
            }
        }
        Ok(())
    }

    fn suspend(&mut self) -> GameSessionResult<()> {
        let active_before = self.engine.state().active_contact_count();
        let mut cleanup_error: Option<Box<dyn Error + Send + Sync>> = None;
        for controller in self.aim_controllers.iter_mut().flatten() {
            if let Err(error) = controller.cancel(&mut self.engine) {
                cleanup_error.get_or_insert_with(|| Box::new(error));
                // `cancel` removes the controller's active contact before it
                // submits the frame. A retry therefore performs no injection,
                // but does finish clearing ADS, timing, and scale state.
                if let Err(reset_error) = controller.cancel(&mut self.engine) {
                    cleanup_error.get_or_insert_with(|| Box::new(reset_error));
                }
            }
        }
        let cancelled = match self.engine.cancel_all() {
            Ok(cancelled) => cancelled,
            Err(error) => {
                cleanup_error.get_or_insert_with(|| Box::new(error));
                false
            }
        };
        self.active_layers = 0;
        self.held_keys = ModifierMask::EMPTY;
        self.held_modifiers = ModifierMask::EMPTY;
        self.held_mouse_buttons = 0;
        self.directions.fill(DirectionalInput::default());
        self.desired_directions.fill(DirectionalInput::default());
        self.active_bindings.fill(false);
        self.desired_bindings.fill(false);
        self.selected_input_layers.fill(None);
        self.last_joystick_frame.fill(None);
        self.last_walk_scales.fill(1.0);
        self.sustained_taps.fill(None);
        self.walk_toggled.fill(false);
        self.pending_radials.clear();
        if self.trace_input {
            println!(
                "[trace] runtime suspend active_before={} cancel_frame_submitted={cancelled}",
                active_before
            );
        }
        cleanup_error.map_or(Ok(()), Err)
    }

    fn stop(&mut self) -> GameSessionResult<()> {
        self.suspend()
    }

    fn now(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn record_pipeline_latency(
        &mut self,
        reader_latency: Duration,
        kernel_timestamp: Option<SystemTime>,
    ) {
        const MAX_SAMPLES: usize = 100_000;
        if self.pipeline_latencies.len() < MAX_SAMPLES {
            self.pipeline_latencies.push(reader_latency);
        }
        if let Some(timestamp) = kernel_timestamp {
            if let Some(latency) = kernel_event_age(timestamp) {
                if self.kernel_latencies.len() < MAX_SAMPLES {
                    self.kernel_latencies.push(latency);
                }
            } else {
                self.rejected_kernel_timestamps = self.rejected_kernel_timestamps.saturating_add(1);
            }
        }
    }
}

fn tap_binding<I: TouchInjector>(
    engine: &mut TouchEngine<I>,
    contact_id: ContactId,
    binding: &str,
    point: Point,
    trace_input: bool,
) -> GameSessionResult<()> {
    engine.submit(TouchFrame::single(TouchEvent::new(
        contact_id,
        TouchPhase::Down,
        point,
    )))?;
    engine.submit(TouchFrame::single(TouchEvent::new(
        contact_id,
        TouchPhase::Up,
        point,
    )))?;
    if trace_input {
        println!(
            "[trace] runtime tap binding={binding} contact={} point=({},{}) submitted=down+up",
            contact_id.get(),
            point.x,
            point.y
        );
    }
    Ok(())
}

/// How long the wheel-open touch rests at the center before swiping, and how
/// long the swipe holds before release. Games coalesce same-instant
/// down/move/up reports into a plain tap, so each radial phase needs real
/// time between frames for the sector selection to register.
const RADIAL_DOWN_DWELL: Duration = Duration::from_millis(60);
const RADIAL_MOVE_DWELL: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RadialPhase {
    Move,
    Release,
}

#[derive(Debug)]
struct PendingRadial {
    binding_index: usize,
    contact_id: ContactId,
    binding_name: String,
    target: Point,
    phase: RadialPhase,
    due_at: Duration,
}

/// Applies one joystick direction; a scale below 1.0 shrinks the analog
/// deflection (slow walk) while keeping full deflection at 1.0.
fn apply_joystick_input<I: TouchInjector>(
    engine: &mut TouchEngine<I>,
    joystick: &wroid_runtime::VirtualJoystick,
    input: DirectionalInput,
    scale: f64,
) -> Result<bool, TouchEngineError> {
    // A scaled deflection must stay outside the dead zone, otherwise the
    // stick would read as fully released and stop the character instead of
    // walking slowly.
    let floor = f64::from(joystick.dead_zone()) / f64::from(joystick.radius()) + 0.02;
    let scale = scale.max(floor.min(1.0));
    if scale >= 1.0 {
        return joystick.apply(engine, input);
    }
    let Some(mut analog) = AnalogInput::from_directional(input) else {
        // Neutral input keeps the original semantics (release the contact).
        return joystick.apply(engine, input);
    };
    // Scale the vector LENGTH, not the components: a diagonal whose
    // components are scaled keeps sqrt(2) times the magnitude and reads as
    // a fast run. Direction-independent magnitude = uniform walk speed.
    let length = analog.x.hypot(analog.y);
    if length == 0.0 || !length.is_finite() {
        return joystick.apply(engine, input);
    }
    analog.x = analog.x / length * scale;
    analog.y = analog.y / length * scale;
    joystick.apply_analog(engine, analog)
}

/// Sustained-tap half: only the Down phase of a tap.
fn engine_submit_down<I: TouchInjector>(
    engine: &mut TouchEngine<I>,
    contact_id: ContactId,
    point: Point,
) -> GameSessionResult<()> {
    engine.submit(TouchFrame::single(TouchEvent::new(
        contact_id,
        TouchPhase::Down,
        point,
    )))?;
    Ok(())
}

fn radial_down_phase<I: TouchInjector>(
    engine: &mut TouchEngine<I>,
    contact_id: ContactId,
    binding: &str,
    center: Point,
    trace_input: bool,
) -> GameSessionResult<()> {
    engine.submit(TouchFrame::single(TouchEvent::new(
        contact_id,
        TouchPhase::Down,
        center,
    )))?;
    if trace_input {
        println!(
            "[trace] runtime radial binding={binding} contact={} down=({},{}) dwell_ms={}",
            contact_id.get(),
            center.x,
            center.y,
            radial_down_dwell_ms(),
        );
    }
    Ok(())
}

fn radial_down_dwell_ms() -> u64 {
    RADIAL_DOWN_DWELL.as_millis() as u64
}

fn set_hold_binding<I: TouchInjector>(
    engine: &mut TouchEngine<I>,
    contact_id: ContactId,
    binding: &str,
    point: Point,
    pressed: bool,
    trace_input: bool,
) -> GameSessionResult<bool> {
    let active = engine.state().position(contact_id).is_some();
    if active == pressed {
        return Ok(false);
    }
    let phase = if pressed {
        TouchPhase::Down
    } else {
        TouchPhase::Up
    };
    engine.submit(TouchFrame::single(TouchEvent::new(
        contact_id, phase, point,
    )))?;
    if trace_input {
        println!(
            "[trace] runtime hold binding={binding} contact={} point=({},{}) phase={phase:?}",
            contact_id.get(),
            point.x,
            point.y
        );
    }
    Ok(true)
}

fn point_contact(contact_id: Option<ContactId>) -> GameSessionResult<ContactId> {
    contact_id.ok_or_else(|| io::Error::other("point binding is missing its contact id").into())
}

fn direction_intersection(
    current: DirectionalInput,
    desired: DirectionalInput,
) -> DirectionalInput {
    DirectionalInput {
        up: current.up && desired.up,
        left: current.left && desired.left,
        down: current.down && desired.down,
        right: current.right && desired.right,
    }
}

fn binding_available(
    control: &RuntimeControlBinding,
    input: RuntimePhysicalInput,
    active_layers: u64,
    held_modifiers: ModifierMask,
    selected_input_layers: &[Option<LayerId>; PHYSICAL_INPUT_COUNT],
) -> bool {
    if selected_input_layers[physical_input_index(input)]
        .is_some_and(|layer| control.layer == LayerId::BASE || control.layer < layer)
    {
        return false;
    }
    binding_condition_available(control, input, active_layers, held_modifiers)
}

fn binding_condition_available(
    control: &RuntimeControlBinding,
    input: RuntimePhysicalInput,
    active_layers: u64,
    held_modifiers: ModifierMask,
) -> bool {
    let layer_available = control.layer == LayerId::BASE
        || layer_mask_bit(control.layer).is_some_and(|mask| active_layers & mask != 0);
    layer_available
        && control
            .modifier
            .is_none_or(|modifier| held_modifiers.contains(modifier))
        && !control.is_suppressed(input, held_modifiers)
}

fn select_input_layer(
    selected_input_layers: &mut [Option<LayerId>; PHYSICAL_INPUT_COUNT],
    control: &RuntimeControlBinding,
    input: RuntimePhysicalInput,
    active_layers: u64,
) {
    if layer_mask_bit(control.layer).is_none_or(|mask| active_layers & mask == 0) {
        return;
    }
    let selected = &mut selected_input_layers[physical_input_index(input)];
    if selected.is_none_or(|layer| control.layer > layer) {
        *selected = Some(control.layer);
    }
}

fn physical_input_index(input: RuntimePhysicalInput) -> usize {
    match input {
        RuntimePhysicalInput::Key(key) => usize::from(key.index()),
        // Mouse buttons live above every key index (F1–F11 end at 56).
        RuntimePhysicalInput::MouseButton(button) => 57 + button as usize,
    }
}

fn runtime_key(key: HostKey) -> Option<HostKeyName> {
    Some(match key {
        HostKey::Num0 => HostKeyName::Num0,
        HostKey::Num1 => HostKeyName::Num1,
        HostKey::Num2 => HostKeyName::Num2,
        HostKey::Num3 => HostKeyName::Num3,
        HostKey::Num4 => HostKeyName::Num4,
        HostKey::Num5 => HostKeyName::Num5,
        HostKey::Num6 => HostKeyName::Num6,
        HostKey::Num7 => HostKeyName::Num7,
        HostKey::Num8 => HostKeyName::Num8,
        HostKey::Num9 => HostKeyName::Num9,
        HostKey::A => HostKeyName::A,
        HostKey::B => HostKeyName::B,
        HostKey::C => HostKeyName::C,
        HostKey::D => HostKeyName::D,
        HostKey::E => HostKeyName::E,
        HostKey::F => HostKeyName::F,
        HostKey::G => HostKeyName::G,
        HostKey::H => HostKeyName::H,
        HostKey::I => HostKeyName::I,
        HostKey::J => HostKeyName::J,
        HostKey::K => HostKeyName::K,
        HostKey::L => HostKeyName::L,
        HostKey::M => HostKeyName::M,
        HostKey::N => HostKeyName::N,
        HostKey::O => HostKeyName::O,
        HostKey::P => HostKeyName::P,
        HostKey::Q => HostKeyName::Q,
        HostKey::R => HostKeyName::R,
        HostKey::S => HostKeyName::S,
        HostKey::T => HostKeyName::T,
        HostKey::U => HostKeyName::U,
        HostKey::V => HostKeyName::V,
        HostKey::W => HostKeyName::W,
        HostKey::X => HostKeyName::X,
        HostKey::Y => HostKeyName::Y,
        HostKey::Z => HostKeyName::Z,
        HostKey::Space => HostKeyName::Space,
        HostKey::Tab => HostKeyName::Tab,
        HostKey::LeftShift => HostKeyName::Shift,
        HostKey::LeftCtrl => HostKeyName::Ctrl,
        HostKey::LeftAlt => HostKeyName::Alt,
        HostKey::ArrowUp => HostKeyName::Up,
        HostKey::ArrowLeft => HostKeyName::Left,
        HostKey::ArrowDown => HostKeyName::Down,
        HostKey::ArrowRight => HostKeyName::Right,
        HostKey::Esc => HostKeyName::Esc,
        HostKey::F1 => HostKeyName::F1,
        HostKey::F2 => HostKeyName::F2,
        HostKey::F3 => HostKeyName::F3,
        HostKey::F4 => HostKeyName::F4,
        HostKey::F5 => HostKeyName::F5,
        HostKey::F6 => HostKeyName::F6,
        HostKey::F7 => HostKeyName::F7,
        HostKey::F8 => HostKeyName::F8,
        HostKey::F9 => HostKeyName::F9,
        HostKey::F10 => HostKeyName::F10,
        HostKey::F11 => HostKeyName::F11,
        // Reserved for the capture toggle; never reaches profile bindings.
        HostKey::F12 => return None,
    })
}

fn parse_mouse_button(button: &str) -> Option<HostMouseButton> {
    let button = button.trim();
    if button.eq_ignore_ascii_case("left") {
        Some(HostMouseButton::Left)
    } else if button.eq_ignore_ascii_case("right") {
        Some(HostMouseButton::Right)
    } else if button.eq_ignore_ascii_case("middle") {
        Some(HostMouseButton::Middle)
    } else if button.eq_ignore_ascii_case("side") {
        Some(HostMouseButton::Side)
    } else if button.eq_ignore_ascii_case("extra") {
        Some(HostMouseButton::Extra)
    } else {
        None
    }
}

fn runtime_mouse_button(button: wroid_input::mouse::MouseButton) -> HostMouseButton {
    match button {
        wroid_input::mouse::MouseButton::Left => HostMouseButton::Left,
        wroid_input::mouse::MouseButton::Right => HostMouseButton::Right,
        wroid_input::mouse::MouseButton::Middle => HostMouseButton::Middle,
        wroid_input::mouse::MouseButton::Side => HostMouseButton::Side,
        wroid_input::mouse::MouseButton::Extra => HostMouseButton::Extra,
    }
}

fn mouse_button_mask(button: HostMouseButton) -> u8 {
    1_u8 << button as u8
}

fn mouse_button_held(held: u8, button: HostMouseButton) -> bool {
    held & mouse_button_mask(button) != 0
}

fn set_mouse_button_held(held: &mut u8, button: HostMouseButton, pressed: bool) {
    let mask = mouse_button_mask(button);
    if pressed {
        *held |= mask;
    } else {
        *held &= !mask;
    }
}

fn trace_joystick<I: TouchInjector>(
    trace_input: bool,
    binding: &str,
    joystick: &wroid_runtime::VirtualJoystick,
    direction: DirectionalInput,
    submitted: bool,
    engine: &TouchEngine<I>,
) {
    if trace_input {
        println!(
            "[trace] runtime joystick binding={binding} contact={} direction={direction:?} submitted={submitted} position={:?}",
            joystick.contact_id().get(),
            engine.state().position(joystick.contact_id())
        );
    }
}

impl<I: TouchInjector> Drop for UnifiedRuntime<I> {
    fn drop(&mut self) {
        let _ = self.engine.cancel_all();
    }
}

impl UnifiedRuntime<SessionMetricsInjector<UinputTouchInjector>> {
    fn report(&self) -> GameSessionReport {
        let injector = self.engine.injector();
        let recenter_count = self
            .aim_controllers
            .iter()
            .flatten()
            .map(MouseAimController::recenter_count)
            .sum::<u64>();
        let reader_to_inject = latency_metrics(&self.pipeline_latencies);
        let kernel_to_inject = latency_metrics(&self.kernel_latencies);
        GameSessionReport {
            frames_submitted: injector.frames_submitted,
            peak_simultaneous_contacts: injector.peak_contacts as u64,
            mouse_aim_recenters: recenter_count,
            reader_to_inject,
            kernel_to_inject: (kernel_to_inject.samples > 0).then_some(kernel_to_inject),
            rejected_kernel_timestamps: self.rejected_kernel_timestamps,
        }
    }

    fn print_report(report: &GameSessionReport) {
        println!("Session report:");
        println!("  frames submitted: {}", report.frames_submitted);
        println!(
            "  peak simultaneous contacts: {}",
            report.peak_simultaneous_contacts
        );
        println!("  mouse aim recenters: {}", report.mouse_aim_recenters);
        let latency = report.reader_to_inject;
        println!(
            "  reader-to-inject p50/p95/p99/max: {}/{}/{}/{} us ({} batch samples)",
            latency.p50_micros,
            latency.p95_micros,
            latency.p99_micros,
            latency.max_micros,
            latency.samples
        );
        if let Some(kernel_latency) = report.kernel_to_inject {
            println!(
                "  kernel-to-inject p50/p95/p99/max: {}/{}/{}/{} us ({} batch samples)",
                kernel_latency.p50_micros,
                kernel_latency.p95_micros,
                kernel_latency.p99_micros,
                kernel_latency.max_micros,
                kernel_latency.samples
            );
        } else {
            println!("  kernel-to-inject latency: unavailable (no valid evdev timestamps)");
        }
        if report.rejected_kernel_timestamps > 0 {
            println!(
                "  rejected kernel timestamps: {} (clock mismatch or older than 60 s)",
                report.rejected_kernel_timestamps
            );
        }
    }
}

struct SessionMetricsInjector<I> {
    inner: I,
    active_contacts: BTreeSet<ContactId>,
    frames_submitted: u64,
    peak_contacts: usize,
}

impl<I> SessionMetricsInjector<I> {
    fn new(inner: I) -> Self {
        Self {
            inner,
            active_contacts: BTreeSet::new(),
            frames_submitted: 0,
            peak_contacts: 0,
        }
    }
}

impl<I: TouchInjector> TouchInjector for SessionMetricsInjector<I> {
    fn inject(&mut self, frame: &TouchFrame) -> Result<(), TouchInjectionError> {
        let started = Instant::now();
        let result = self.inner.inject(frame);
        // Pinpoint backpressure at the injection sink itself (uinput writes,
        // bridge hand-offs); sustained slowness here stalls the whole loop.
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(50) {
            eprintln!(
                "[stall] touch frame injection took {:.1} ms ({} events)",
                elapsed.as_secs_f64() * 1000.0,
                frame.events().len()
            );
        }
        result?;
        for event in frame.events() {
            match event.phase {
                TouchPhase::Down => {
                    self.active_contacts.insert(event.contact_id);
                }
                TouchPhase::Up | TouchPhase::Cancel => {
                    self.active_contacts.remove(&event.contact_id);
                }
                TouchPhase::Move => {}
            }
        }
        self.frames_submitted = self.frames_submitted.saturating_add(1);
        self.peak_contacts = self.peak_contacts.max(self.active_contacts.len());
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LatencySummary {
    samples: usize,
    p50: u128,
    p95: u128,
    p99: u128,
    max: u128,
}

fn kernel_event_age(timestamp: SystemTime) -> Option<Duration> {
    const MAX_REASONABLE_AGE: Duration = Duration::from_secs(60);
    SystemTime::now()
        .duration_since(timestamp)
        .ok()
        .filter(|latency| *latency <= MAX_REASONABLE_AGE)
}

fn latency_summary_micros(samples: &[Duration]) -> LatencySummary {
    if samples.is_empty() {
        return LatencySummary::default();
    }
    let mut micros = samples.iter().map(Duration::as_micros).collect::<Vec<_>>();
    micros.sort_unstable();
    LatencySummary {
        samples: micros.len(),
        p50: percentile_from_sorted(&micros, 50),
        p95: percentile_from_sorted(&micros, 95),
        p99: percentile_from_sorted(&micros, 99),
        max: *micros.last().expect("non-empty samples have a maximum"),
    }
}

fn latency_metrics(samples: &[Duration]) -> LatencyMetrics {
    let summary = latency_summary_micros(samples);
    LatencyMetrics {
        samples: summary.samples.try_into().unwrap_or(u64::MAX),
        p50_micros: summary.p50.try_into().unwrap_or(u64::MAX),
        p95_micros: summary.p95.try_into().unwrap_or(u64::MAX),
        p99_micros: summary.p99.try_into().unwrap_or(u64::MAX),
        max_micros: summary.max.try_into().unwrap_or(u64::MAX),
    }
}

fn percentile_from_sorted(micros: &[u128], percentile: usize) -> u128 {
    let rank = micros.len().saturating_mul(percentile).saturating_add(99) / 100;
    let index = rank.saturating_sub(1).min(micros.len() - 1);
    micros[index]
}

fn layer_mask_bit(layer: LayerId) -> Option<u64> {
    layer
        .get()
        .checked_sub(1)
        .filter(|index| *index < 64)
        .map(|index| 1_u64 << index)
}

#[derive(Debug)]
pub struct GameSessionOptions {
    pub profile_path: Option<PathBuf>,
    pub keyboard_path: Option<PathBuf>,
    pub mouse_path: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
    pub grab: bool,
    pub show_ui: bool,
    pub launch_package: bool,
    pub trace_input: bool,
    /// Stream `getevent -lt` for the bridged touchscreen from inside Android,
    /// making touch delivery problems visible in the session log.
    pub trace_android_input: bool,
    pub exit_after: Option<Duration>,
    pub focus_socket: Option<PathBuf>,
    pub bridge_broker: Option<BridgeBrokerClient>,
    pub cleanup: bool,
}

impl GameSessionOptions {
    pub fn new(
        profile_path: PathBuf,
        keyboard_path: PathBuf,
        mouse_path: Option<PathBuf>,
        width: u32,
        height: u32,
    ) -> GameSessionResult<Self> {
        if width == 0 || height == 0 {
            return Err(invalid_input("width and height must be greater than zero"));
        }
        Ok(Self {
            profile_path: Some(profile_path),
            keyboard_path: Some(keyboard_path),
            mouse_path,
            width,
            height,
            grab: true,
            show_ui: true,
            launch_package: true,
            trace_input: false,
            trace_android_input: false,
            exit_after: None,
            focus_socket: None,
            bridge_broker: None,
            cleanup: false,
        })
    }

    fn parse(args: impl IntoIterator<Item = String>) -> GameSessionResult<Option<Self>> {
        let mut positional = Vec::new();
        let mut options = Self {
            profile_path: None,
            keyboard_path: None,
            mouse_path: None,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            grab: true,
            show_ui: true,
            launch_package: true,
            trace_input: false,
            trace_android_input: false,
            exit_after: None,
            focus_socket: None,
            bridge_broker: None,
            cleanup: false,
        };
        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--help" | "-h" => return Ok(None),
                "--cleanup" => options.cleanup = true,
                "--no-grab" => options.grab = false,
                "--no-ui" => options.show_ui = false,
                "--no-launch" => options.launch_package = false,
                "--trace-input" => options.trace_input = true,
                "--trace-android-input" => options.trace_android_input = true,
                "--exit-after-ms" => {
                    let milliseconds: u64 = parse_next(&mut args, "--exit-after-ms")?;
                    if milliseconds == 0 {
                        return Err(invalid_input("--exit-after-ms must be greater than zero"));
                    }
                    options.exit_after = Some(Duration::from_millis(milliseconds));
                }
                "--focus-socket" => {
                    options.focus_socket = Some(PathBuf::from(parse_next::<String>(
                        &mut args,
                        "--focus-socket",
                    )?));
                }
                "--width" => options.width = parse_next(&mut args, "--width")?,
                "--height" => options.height = parse_next(&mut args, "--height")?,
                value if value.starts_with("--") => {
                    return Err(invalid_input(format!("unknown option: {value}")))
                }
                value => positional.push(PathBuf::from(value)),
            }
        }
        if options.cleanup {
            return Ok(Some(options));
        }
        if options.width == 0 || options.height == 0 {
            return Err(invalid_input("width and height must be greater than zero"));
        }
        if !(2..=3).contains(&positional.len()) {
            return Err(invalid_input(
                "expected <profile-v2.json> <keyboard-event-node> [mouse-event-node]",
            ));
        }
        options.profile_path = Some(positional.remove(0));
        options.keyboard_path = Some(positional.remove(0));
        options.mouse_path = (!positional.is_empty()).then(|| positional.remove(0));
        Ok(Some(options))
    }
}

fn parse_next<T>(args: &mut impl Iterator<Item = String>, label: &str) -> GameSessionResult<T>
where
    T: std::str::FromStr,
    T::Err: Error + Send + Sync + 'static,
{
    let value = args
        .next()
        .ok_or_else(|| invalid_input(format!("missing {label}")))?;
    value
        .parse::<T>()
        .map_err(|source| invalid_input(format!("invalid {label} '{value}': {source}")))
}

fn invalid_input(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    io::Error::new(io::ErrorKind::InvalidInput, message.into()).into()
}

fn install_interrupt_handler() -> io::Result<()> {
    INTERRUPT_REQUESTED.store(false, Ordering::SeqCst);
    for signal_number in [SIGHUP, SIGINT, SIGTERM] {
        // SAFETY: the handler only performs a lock-free atomic store.
        let previous = unsafe { signal(signal_number, request_interrupt) };
        if previous == SIGNAL_ERROR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

extern "C" fn request_interrupt(_signal: i32) {
    INTERRUPT_REQUESTED.store(true, Ordering::SeqCst);
}

fn interrupt_requested() -> bool {
    INTERRUPT_REQUESTED.load(Ordering::SeqCst)
}

fn print_usage() {
    println!(
        "Usage: wroid-waydroid-game-session <profile-v2.json> <keyboard-event-node> [mouse-event-node] [--width W] [--height H] [--no-grab] [--no-ui] [--no-launch] [--trace-input] [--trace-android-input] [--exit-after-ms N]"
    );
    println!(
        "Example: sudo ./target/release/wroid-waydroid-game-session profiles/examples/shooter-v2.json /dev/input/event7 /dev/input/event9 --width 1920 --height 1080 --trace-input"
    );
    println!("Recovery: sudo ./target/release/wroid-waydroid-game-session --cleanup");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn focus_handshake_tolerates_output_line_before_state() {
        use std::io::Write as _;
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("focus-relay.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // The relay may flush its metadata line before the state line.
            stream.write_all(b"output DP-2\nfocused\n").unwrap();
            // Hold the socket open briefly so the trailing-line probe sees a
            // clean timeout instead of a reset.
            thread::sleep(Duration::from_millis(200));
        });

        let connection = FocusConnection::connect(&socket_path).unwrap();
        assert!(connection.focused);
        assert_eq!(connection.output.as_deref(), Some("DP-2"));
        server.join().unwrap();
    }

    #[test]
    fn focus_reader_ignores_late_output_lines() {
        use std::io::Write as _;
        let (client, mut server) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        spawn_focus_reader(BufReader::new(client), tx);

        for line in ["focused\n", "output DP-2\n", "unfocused\n"] {
            server.write_all(line.as_bytes()).unwrap();
        }

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok(FocusEvent::Changed(true))
        ));
        // The metadata line must not surface as an Unavailable event, and the
        // reader must keep reporting later state transitions.
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_secs(2)),
                Ok(FocusEvent::Changed(false))
            ),
            "reader died on a metadata line"
        );
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn classify_relay_line_rejects_unknown_payload() {
        let error = classify_relay_line("garbage\n").unwrap_err();
        assert!(error.to_string().contains("invalid focus relay state"));
        assert!(matches!(
            classify_relay_line("focused"),
            Ok(RelayLine::State(true))
        ));
        assert!(matches!(
            classify_relay_line("unfocused\n"),
            Ok(RelayLine::State(false))
        ));
        assert!(matches!(
            classify_relay_line("output HDMI-A-4\n"),
            Ok(RelayLine::Output(name)) if name == "HDMI-A-4"
        ));
    }

    #[test]
    fn virtual_cursor_starts_centered_and_clamps_to_the_surface() {
        let mut cursor = VirtualCursor::centered(1000, 500);
        assert_eq!(cursor.point(), Point { x: 500, y: 250 });

        cursor.move_by(-10_000, -10_000, 1000, 500);
        assert_eq!(cursor.point(), Point { x: 0, y: 0 });

        cursor.move_by(90_000, 90_000, 1000, 500);
        assert_eq!(cursor.point(), Point { x: 999, y: 499 });

        cursor.move_by(-150, 120, 1000, 500);
        assert_eq!(cursor.point(), Point { x: 849, y: 499 });
    }

    #[test]
    fn virtual_cursor_accumulates_relative_deltas() {
        let mut cursor = VirtualCursor::centered(1920, 1080);
        cursor.move_by(30, -20, 1920, 1080);
        cursor.move_by(20, 5, 1920, 1080);
        assert_eq!(cursor.point(), Point { x: 1010, y: 525 });
    }

    use crate::{
        serve_bridge_broker, BridgeBrokerClient, BridgeHelperFactory, BridgeHelperSession,
    };
    use wroid_core::profile_v2::{
        ActionV2, BindingV2, JoystickMode, JoystickWalkMode, LayerActivation, LayerV2,
        NormalizedPoint, NormalizedRect, ProfileV2,
    };
    use wroid_runtime::{TouchInjectionError, TouchInjector};

    #[test]
    fn package_launch_bypasses_full_android_desktop() {
        assert_eq!(android_open_action(true, true), AndroidOpenAction::Package);
        assert_eq!(android_open_action(false, true), AndroidOpenAction::Package);
        assert_eq!(android_open_action(true, false), AndroidOpenAction::FullUi);
        assert_eq!(android_open_action(false, false), AndroidOpenAction::None);
    }

    #[derive(Default)]
    struct RecordingInjector {
        frames: Vec<TouchFrame>,
        fail_next: bool,
    }

    struct RecordingBridgeFactory {
        calls: Arc<Mutex<Vec<String>>>,
    }

    struct RecordingBridgeSession {
        calls: Arc<Mutex<Vec<String>>>,
        finished: bool,
    }

    impl BridgeHelperFactory for RecordingBridgeFactory {
        fn start(&self, event_node: &Path) -> io::Result<Box<dyn BridgeHelperSession>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("open:{}", event_node.display()));
            Ok(Box::new(RecordingBridgeSession {
                calls: self.calls.clone(),
                finished: false,
            }))
        }
    }

    impl BridgeHelperSession for RecordingBridgeSession {
        fn verify_android_input(&mut self) -> io::Result<()> {
            self.calls.lock().unwrap().push("verify".to_owned());
            Ok(())
        }

        fn finish(mut self: Box<Self>, waydroid_stopped: bool) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("finish:{waydroid_stopped}"));
            self.finished = true;
            Ok(())
        }
    }

    impl Drop for RecordingBridgeSession {
        fn drop(&mut self) {
            if !self.finished {
                self.calls.lock().unwrap().push("drop".to_owned());
            }
        }
    }

    impl TouchInjector for RecordingInjector {
        fn inject(&mut self, frame: &TouchFrame) -> Result<(), TouchInjectionError> {
            if self.fail_next {
                self.fail_next = false;
                return Err(TouchInjectionError::new("injected cleanup failure"));
            }
            self.frames.push(frame.clone());
            Ok(())
        }
    }

    #[test]
    fn parses_trace_input_flag() {
        let options = GameSessionOptions::parse([
            "profile.json".to_owned(),
            "/dev/input/event7".to_owned(),
            "/dev/input/event3".to_owned(),
            "--trace-input".to_owned(),
            "--no-grab".to_owned(),
        ])
        .unwrap()
        .unwrap();

        assert!(options.trace_input);
        assert!(!options.grab);
        assert_eq!(options.profile_path, Some(PathBuf::from("profile.json")));
    }

    #[test]
    fn rootless_session_requires_daemon_bridge_before_runtime_setup() {
        let result = select_bridge_broker(false, None);
        let error = match result {
            Ok(_) => panic!("rootless session unexpectedly accepted no broker"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("daemon-owned bridge channel"));
        assert!(select_bridge_broker(true, None).unwrap().is_none());
    }

    #[test]
    fn session_bridge_delegates_verify_and_forced_cleanup_to_broker() {
        let (client, server) = UnixStream::pair().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(RecordingBridgeFactory {
            calls: calls.clone(),
        });
        let broker = thread::spawn(move || serve_bridge_broker(server, factory));
        let owned: OwnedFd = client.into();
        let mut client = BridgeBrokerClient::from_owned_fd(owned).unwrap();
        client.open(Path::new("/dev/input/event17")).unwrap();
        let mut bridge = SessionBridge::Broker(client);

        bridge.verify_android_input().unwrap();
        bridge.cleanup(false).unwrap();
        broker.join().unwrap().unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            ["open:/dev/input/event17", "verify", "finish:false"]
        );
    }

    #[test]
    fn parses_internal_focus_socket() {
        let options = GameSessionOptions::parse([
            "profile.json".to_owned(),
            "/dev/input/event7".to_owned(),
            "--focus-socket".to_owned(),
            "/run/user/1000/wroid/focus-test.sock".to_owned(),
        ])
        .unwrap()
        .unwrap();

        assert_eq!(
            options.focus_socket,
            Some(PathBuf::from("/run/user/1000/wroid/focus-test.sock"))
        );
        assert!(matches!(
            classify_relay_line("focused\n"),
            Ok(RelayLine::State(true))
        ));
        assert!(matches!(
            classify_relay_line("unfocused\n"),
            Ok(RelayLine::State(false))
        ));
        assert!(classify_relay_line("unknown\n").is_err());
    }

    #[test]
    fn released_keyboard_forwards_only_the_capture_hotkey() {
        let events = vec![
            HostKeyEvent::new(wroid_input::HostKey::W, KeyTransition::Pressed),
            HostKeyEvent::new(wroid_input::HostKey::F12, KeyTransition::Pressed),
            HostKeyEvent::new(wroid_input::HostKey::F12, KeyTransition::Released),
        ];
        assert_eq!(
            keyboard_events_for_capture(events.clone(), false),
            events[1..]
        );
        assert_eq!(keyboard_events_for_capture(events.clone(), true), events);
    }

    #[test]
    fn cleanup_does_not_require_positional_paths() {
        let options = GameSessionOptions::parse(["--cleanup".to_owned()])
            .unwrap()
            .unwrap();

        assert!(options.cleanup);
        assert!(options.profile_path.is_none());
        assert!(options.keyboard_path.is_none());
        assert!(options.mouse_path.is_none());
    }

    #[test]
    fn reports_game_and_all_cleanup_failures_together() {
        let result = combine_session_results::<()>(
            Err(io::Error::other("runtime injection failed").into()),
            Err(io::Error::other("container did not stop")),
            Err(io::Error::other("managed include could not be removed")),
        );

        let error = result.unwrap_err().to_string();
        assert!(error.contains("game session failed: runtime injection failed"));
        assert!(error.contains("Waydroid shutdown failed: container did not stop"));
        assert!(error.contains("input bridge cleanup failed: managed include could not be removed"));
    }

    #[test]
    fn keyboard_only_profile_does_not_require_mouse_path() {
        let options =
            GameSessionOptions::parse(["profile.json".to_owned(), "/dev/input/event7".to_owned()])
                .unwrap()
                .unwrap();

        assert_eq!(
            options.keyboard_path,
            Some(PathBuf::from("/dev/input/event7"))
        );
        assert!(options.mouse_path.is_none());
    }

    #[test]
    fn two_joysticks_and_tap_coexist_as_three_contacts() {
        let profile = ProfileV2 {
            schema_version: 2,
            name: "Brawl test".to_owned(),
            package_name: "com.supercell.brawlstars".to_owned(),
            orientation: Default::default(),
            layers: Vec::new(),
            bindings: vec![
                joystick_binding("movement", "w", "a", "s", "d", 0.2),
                joystick_binding("attack", "up", "left", "down", "right", 0.8),
                BindingV2 {
                    name: "super".to_owned(),
                    layer: None,
                    modifier: None,
                    input: InputV2::Key {
                        key: "space".to_owned(),
                    },
                    action: ActionV2::Tap {
                        hold: false,
                        point: NormalizedPoint { x: 0.7, y: 0.8 },
                    },
                },
            ],
        };
        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1000,
                height: 1000,
            },
        )
        .unwrap();
        let mut runtime = UnifiedRuntime::new(plan, RecordingInjector::default(), false).unwrap();
        runtime.start().unwrap();

        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::W,
                KeyTransition::Pressed,
            ))
            .unwrap();
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::ArrowRight,
                KeyTransition::Pressed,
            ))
            .unwrap();
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::Space,
                KeyTransition::Pressed,
            ))
            .unwrap();

        assert_eq!(peak_contacts(&runtime.engine.injector().frames), 3);
        assert_eq!(runtime.engine.state().active_contact_count(), 2);
        runtime.stop().unwrap();
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        assert_eq!(
            runtime
                .engine
                .injector()
                .frames
                .last()
                .unwrap()
                .events()
                .len(),
            2
        );
    }

    #[test]
    fn mouse_hold_keeps_contact_down_until_release() {
        let profile = ProfileV2 {
            schema_version: 2,
            name: "Automatic fire test".to_owned(),
            package_name: "com.example.shooter".to_owned(),
            orientation: Default::default(),
            layers: Vec::new(),
            bindings: vec![BindingV2 {
                name: "fire".to_owned(),
                layer: None,
                modifier: None,
                input: InputV2::MouseButton {
                    button: "left".to_owned(),
                },
                action: ActionV2::Hold {
                    point: NormalizedPoint { x: 0.9, y: 0.5 },
                },
            }],
        };
        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1000,
                height: 1000,
            },
        )
        .unwrap();
        let mut runtime = UnifiedRuntime::new(plan, RecordingInjector::default(), false).unwrap();
        runtime.start().unwrap();
        let press = MouseEvent::Button(wroid_input::mouse::MouseButtonEvent::new(
            wroid_input::mouse::MouseButton::Left,
            MouseButtonTransition::Pressed,
        ));
        let release = MouseEvent::Button(wroid_input::mouse::MouseButtonEvent::new(
            wroid_input::mouse::MouseButton::Left,
            MouseButtonTransition::Released,
        ));

        assert!(runtime.handle_mouse(press).unwrap());
        assert!(!runtime.handle_mouse(press).unwrap());
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(runtime.engine.injector().frames.len(), 1);
        assert_eq!(
            runtime.engine.injector().frames[0].events()[0].phase,
            TouchPhase::Down
        );

        assert!(runtime.handle_mouse(release).unwrap());
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        assert_eq!(runtime.engine.injector().frames.len(), 2);
        assert_eq!(
            runtime.engine.injector().frames[1].events()[0].phase,
            TouchPhase::Up
        );
    }

    #[test]
    fn ui_lmb_stays_cursor_click_after_overlay_loss() {
        let profile = ProfileV2 {
            schema_version: 2,
            name: "UI mouse routing test".to_owned(),
            package_name: "com.example.shooter".to_owned(),
            orientation: Default::default(),
            layers: Vec::new(),
            bindings: vec![
                BindingV2 {
                    name: "camera".to_owned(),
                    layer: None,
                    modifier: None,
                    input: InputV2::MouseMove,
                    action: ActionV2::MouseAim {
                        region: NormalizedRect {
                            x: 0.2,
                            y: 0.1,
                            w: 0.6,
                            h: 0.7,
                        },
                        sensitivity: 1.0,
                        toggle_key: Some("tab".to_owned()),
                        recenter_threshold: 0.8,
                        recenter_gap_ms: 0,
                        ads_multiplier: None,
                        reaffirm_ms: None,
                    },
                },
                mouse_hold_binding("fire", "left", None, None, 0.9),
            ],
        };
        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1000,
                height: 1000,
            },
        )
        .unwrap();
        let mut runtime = UnifiedRuntime::new(plan, RecordingInjector::default(), false).unwrap();
        runtime.start().unwrap();

        let (host_sender, host_receiver) = mpsc::channel();
        host_sender
            .send(HostEvent::Keyboard {
                events: vec![HostKeyEvent::new(
                    HostKey::LeftShift,
                    KeyTransition::Pressed,
                )],
                received_at: Instant::now(),
                kernel_timestamp: None,
            })
            .unwrap();
        host_sender
            .send(HostEvent::Mouse {
                events: vec![
                    MouseEvent::Button(wroid_input::mouse::MouseButtonEvent::new(
                        MouseButton::Left,
                        MouseButtonTransition::Pressed,
                    )),
                    MouseEvent::Button(wroid_input::mouse::MouseButtonEvent::new(
                        MouseButton::Left,
                        MouseButtonTransition::Released,
                    )),
                ],
                received_at: Instant::now(),
                kernel_timestamp: None,
            })
            .unwrap();
        for event in [
            HostKeyEvent::new(HostKey::LeftCtrl, KeyTransition::Pressed),
            HostKeyEvent::new(HostKey::Esc, KeyTransition::Pressed),
        ] {
            host_sender
                .send(HostEvent::Keyboard {
                    events: vec![event],
                    received_at: Instant::now(),
                    kernel_timestamp: None,
                })
                .unwrap();
        }
        let (keyboard_control, _keyboard_commands) = mpsc::channel();
        let controls = InputReaderControls {
            keyboard: keyboard_control,
            mouse: None,
        };

        let exit = run_event_loop(
            &host_receiver,
            &controls,
            &mut runtime,
            EventLoopOptions {
                trace_input: false,
                exit_after: None,
                focus_protected: true,
                focused: true,
                focus_receiver: None,
                grab_allowed: true,
                initial_mode: InputMode::Ui,
                // This models a renderer failure after UI mode was selected.
                // Input routing must remain usable without the cursor sprite.
                overlay: None,
                profile_reload: None,
            },
        )
        .unwrap();

        assert_eq!(exit, EventLoopExit::ExitHotkeyRequested);
        let events = runtime
            .engine
            .injector()
            .frames
            .iter()
            .flat_map(|frame| frame.events())
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].phase, TouchPhase::Down);
        assert_eq!(events[1].phase, TouchPhase::Up);
        assert!(events
            .iter()
            .all(|event| event.position == Point { x: 500, y: 500 }));
    }

    #[test]
    fn focus_loss_suspends_runtime_and_releases_capture() {
        let profile = ProfileV2 {
            schema_version: 2,
            name: "Focus test".to_owned(),
            package_name: "com.example.focus".to_owned(),
            orientation: Default::default(),
            layers: Vec::new(),
            bindings: vec![joystick_binding("movement", "w", "a", "s", "d", 0.2)],
        };
        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1000,
                height: 1000,
            },
        )
        .unwrap();
        let mut runtime = UnifiedRuntime::new(plan, RecordingInjector::default(), false).unwrap();
        runtime.start().unwrap();
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::W,
                KeyTransition::Pressed,
            ))
            .unwrap();
        assert_eq!(runtime.engine.state().active_contact_count(), 1);

        let (host_sender, host_receiver) = mpsc::channel();
        let (focus_sender, focus_receiver) = mpsc::channel();
        focus_sender.send(FocusEvent::Changed(false)).unwrap();
        let (reader_control, reader_commands) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            if let Ok(InputReaderCommand::SetCapture { enabled, reply }) =
                reader_commands.recv_timeout(Duration::from_secs(1))
            {
                assert!(!enabled);
                let _ = reply.send(Ok(()));
            }
        });
        let controls = InputReaderControls {
            keyboard: reader_control,
            mouse: None,
        };

        let exit = run_event_loop(
            &host_receiver,
            &controls,
            &mut runtime,
            EventLoopOptions {
                trace_input: false,
                exit_after: Some(Duration::from_millis(1)),
                focus_protected: true,
                focused: true,
                focus_receiver: Some(&focus_receiver),
                grab_allowed: true,
                initial_mode: InputMode::Aim,
                overlay: None,
                profile_reload: None,
            },
        )
        .unwrap();
        drop(host_sender);
        reader_thread.join().unwrap();

        assert_eq!(exit, EventLoopExit::TimeLimitReached);
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        assert!(runtime
            .directions
            .iter()
            .all(|direction| *direction == DirectionalInput::default()));
    }

    #[test]
    fn f12_releases_and_recaptures_before_gameplay_resumes() {
        let profile = ProfileV2 {
            schema_version: 2,
            name: "Capture toggle test".to_owned(),
            package_name: "com.example.capture".to_owned(),
            orientation: Default::default(),
            layers: Vec::new(),
            bindings: vec![joystick_binding("movement", "w", "a", "s", "d", 0.2)],
        };
        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1000,
                height: 1000,
            },
        )
        .unwrap();
        let mut runtime = UnifiedRuntime::new(plan, RecordingInjector::default(), false).unwrap();
        runtime.start().unwrap();

        let (host_sender, host_receiver) = mpsc::channel();
        for event in [
            HostKeyEvent::new(wroid_input::HostKey::F12, KeyTransition::Pressed),
            HostKeyEvent::new(wroid_input::HostKey::F12, KeyTransition::Pressed),
            HostKeyEvent::new(wroid_input::HostKey::W, KeyTransition::Pressed),
            HostKeyEvent::new(wroid_input::HostKey::LeftCtrl, KeyTransition::Pressed),
            HostKeyEvent::new(wroid_input::HostKey::Esc, KeyTransition::Pressed),
        ] {
            host_sender
                .send(HostEvent::Keyboard {
                    events: vec![event],
                    received_at: Instant::now(),
                    kernel_timestamp: None,
                })
                .unwrap();
        }
        let (reader_control, reader_commands) = mpsc::channel();
        let (capture_state_sender, capture_states) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            for _ in 0..2 {
                match reader_commands
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                {
                    InputReaderCommand::SetCapture { enabled, reply } => {
                        capture_state_sender.send(enabled).unwrap();
                        let _ = reply.send(Ok(()));
                    }
                    InputReaderCommand::SetMouseMode { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    }
                }
            }
        });
        let controls = InputReaderControls {
            keyboard: reader_control,
            mouse: None,
        };

        let exit = run_event_loop(
            &host_receiver,
            &controls,
            &mut runtime,
            EventLoopOptions {
                trace_input: false,
                exit_after: None,
                focus_protected: true,
                focused: true,
                focus_receiver: None,
                grab_allowed: true,
                initial_mode: InputMode::Aim,
                overlay: None,
                profile_reload: None,
            },
        )
        .unwrap();
        reader_thread.join().unwrap();

        assert_eq!(exit, EventLoopExit::ExitHotkeyRequested);
        assert_eq!(capture_states.try_iter().collect::<Vec<_>>(), [false, true]);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(runtime.pipeline_latencies.len(), 1);
        assert!(runtime.kernel_latencies.is_empty());
        runtime.stop().unwrap();
    }

    #[test]
    fn plain_escape_reaches_profile_before_ctrl_escape_stops_session() {
        let profile = ProfileV2 {
            schema_version: 2,
            name: "Escape binding test".to_owned(),
            package_name: "com.example.escape".to_owned(),
            orientation: Default::default(),
            layers: Vec::new(),
            bindings: vec![BindingV2 {
                name: "menu".to_owned(),
                layer: None,
                modifier: None,
                input: InputV2::Key {
                    key: "esc".to_owned(),
                },
                action: ActionV2::Tap {
                    hold: false,
                    point: NormalizedPoint { x: 0.5, y: 0.5 },
                },
            }],
        };
        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1000,
                height: 1000,
            },
        )
        .unwrap();
        let mut runtime = UnifiedRuntime::new(plan, RecordingInjector::default(), false).unwrap();
        runtime.start().unwrap();

        let (host_sender, host_receiver) = mpsc::channel();
        for event in [
            HostKeyEvent::new(wroid_input::HostKey::Esc, KeyTransition::Pressed),
            HostKeyEvent::new(wroid_input::HostKey::LeftCtrl, KeyTransition::Pressed),
            HostKeyEvent::new(wroid_input::HostKey::Esc, KeyTransition::Pressed),
        ] {
            host_sender
                .send(HostEvent::Keyboard {
                    events: vec![event],
                    received_at: Instant::now(),
                    kernel_timestamp: None,
                })
                .unwrap();
        }
        let (reader_control, _reader_commands) = mpsc::channel();
        let controls = InputReaderControls {
            keyboard: reader_control,
            mouse: None,
        };

        let exit = run_event_loop(
            &host_receiver,
            &controls,
            &mut runtime,
            EventLoopOptions {
                trace_input: false,
                exit_after: None,
                focus_protected: true,
                focused: true,
                focus_receiver: None,
                grab_allowed: true,
                initial_mode: InputMode::Aim,
                overlay: None,
                profile_reload: None,
            },
        )
        .unwrap();

        assert_eq!(exit, EventLoopExit::ExitHotkeyRequested);
        assert_eq!(runtime.engine.injector().frames.len(), 2);
        assert_eq!(runtime.pipeline_latencies.len(), 1);
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
    }

    #[test]
    fn base_binding_fires_without_a_declared_layer() {
        let mut runtime = test_runtime(
            Vec::new(),
            vec![key_hold_binding("base", "r", None, None, 0.2)],
        );

        assert!(send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down]);

        assert!(send_key(&mut runtime, HostKey::R, KeyTransition::Released));
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down, TouchPhase::Up]);
    }

    #[test]
    fn inactive_layer_blocks_binding() {
        let mut runtime = test_runtime(
            vec![hold_layer("combat", "tab")],
            vec![key_hold_binding(
                "layer-fire",
                "r",
                Some("combat"),
                None,
                0.2,
            )],
        );

        assert!(!send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert!(!send_key(&mut runtime, HostKey::R, KeyTransition::Released));
        assert!(runtime.engine.injector().frames.is_empty());
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
    }

    #[test]
    fn hold_layer_enables_binding_only_while_held() {
        let mut runtime = test_runtime(
            vec![hold_layer("combat", "tab")],
            vec![key_hold_binding(
                "layer-fire",
                "r",
                Some("combat"),
                None,
                0.2,
            )],
        );

        assert!(!send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Pressed
        ));
        assert!(send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);

        assert!(send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Released
        ));
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down, TouchPhase::Up]);
        assert!(!send_key(&mut runtime, HostKey::R, KeyTransition::Released));
    }

    #[test]
    fn toggle_layer_persists_after_activation_release_and_turns_off_on_second_press() {
        let mut runtime = test_runtime(
            vec![toggle_layer("combat", "tab")],
            vec![key_hold_binding(
                "layer-fire",
                "r",
                Some("combat"),
                None,
                0.2,
            )],
        );

        assert!(!send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Pressed
        ));
        assert!(!send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Released
        ));
        assert!(send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);

        assert!(send_key(&mut runtime, HostKey::Tab, KeyTransition::Pressed));
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down, TouchPhase::Up]);
    }

    #[test]
    fn activation_key_is_consumed_instead_of_dispatching_a_modified_base_binding() {
        let mut runtime = test_runtime(
            vec![hold_layer("combat", "tab")],
            vec![key_hold_binding(
                "base-tab",
                "tab",
                None,
                Some("shift"),
                0.2,
            )],
        );

        assert!(!send_key(
            &mut runtime,
            HostKey::LeftShift,
            KeyTransition::Pressed
        ));
        assert!(!send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Pressed
        ));
        assert!(!send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Released
        ));
        assert!(runtime.engine.injector().frames.is_empty());
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
    }

    #[test]
    fn same_key_selects_active_layer_action_instead_of_base_action() {
        let mut runtime = test_runtime(
            vec![hold_layer("combat", "tab")],
            vec![
                key_hold_binding("base", "r", None, None, 0.2),
                key_hold_binding("layer", "r", Some("combat"), None, 0.8),
            ],
        );

        assert!(send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(last_touch_event(&runtime).position.x, 200);

        assert!(send_key(&mut runtime, HostKey::Tab, KeyTransition::Pressed));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(last_touch_event(&runtime).position.x, 799);
        assert_eq!(
            emitted_phases(&runtime),
            [TouchPhase::Down, TouchPhase::Up, TouchPhase::Down]
        );

        assert!(send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Released
        ));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(last_touch_event(&runtime).position.x, 200);
    }

    #[test]
    fn active_layer_owns_input_even_when_its_modifier_is_not_held() {
        let mut runtime = test_runtime(
            vec![hold_layer("combat", "tab")],
            vec![
                key_hold_binding("base", "r", None, None, 0.2),
                key_hold_binding("layer-shift", "r", Some("combat"), Some("shift"), 0.8),
            ],
        );

        assert!(!send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Pressed
        ));
        assert!(!send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert!(runtime.engine.injector().frames.is_empty());

        assert!(send_key(
            &mut runtime,
            HostKey::LeftShift,
            KeyTransition::Pressed
        ));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(last_touch_event(&runtime).position.x, 799);

        assert!(send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Released
        ));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(last_touch_event(&runtime).position.x, 200);
        assert_eq!(
            emitted_phases(&runtime),
            [TouchPhase::Down, TouchPhase::Up, TouchPhase::Down]
        );
    }

    #[test]
    fn highest_active_layer_id_wins_when_named_layers_share_an_input() {
        let mut runtime = test_runtime(
            vec![hold_layer("lower", "tab"), hold_layer("higher", "alt")],
            vec![
                key_hold_binding("base", "r", None, None, 0.2),
                key_hold_binding("lower", "r", Some("lower"), None, 0.5),
                key_hold_binding("higher", "r", Some("higher"), None, 0.8),
            ],
        );

        send_key(&mut runtime, HostKey::R, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::Tab, KeyTransition::Pressed);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(last_touch_event(&runtime).position.x, 500);

        send_key(&mut runtime, HostKey::LeftAlt, KeyTransition::Pressed);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(last_touch_event(&runtime).position.x, 799);

        send_key(&mut runtime, HostKey::LeftAlt, KeyTransition::Released);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(last_touch_event(&runtime).position.x, 500);

        send_key(&mut runtime, HostKey::Tab, KeyTransition::Released);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(last_touch_event(&runtime).position.x, 200);
    }

    #[test]
    fn modifier_binding_requires_held_modifier() {
        let mut runtime = test_runtime(
            Vec::new(),
            vec![key_hold_binding(
                "shift-fire",
                "r",
                None,
                Some("shift"),
                0.2,
            )],
        );

        assert!(!send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert!(!send_key(&mut runtime, HostKey::R, KeyTransition::Released));
        assert!(!send_key(
            &mut runtime,
            HostKey::LeftShift,
            KeyTransition::Pressed
        ));
        assert!(send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down]);
    }

    #[test]
    fn unmodified_sibling_is_suppressed_while_modifier_is_held() {
        let mut runtime = test_runtime(
            Vec::new(),
            vec![
                key_hold_binding("base", "r", None, None, 0.2),
                key_hold_binding("specific", "r", None, Some("shift"), 0.8),
            ],
        );

        send_key(&mut runtime, HostKey::LeftShift, KeyTransition::Pressed);
        assert!(send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));

        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(runtime.engine.injector().frames.len(), 1);
        let event = runtime.engine.injector().frames[0].events()[0];
        assert_eq!(event.phase, TouchPhase::Down);
        assert_eq!(event.position.x, 799);
    }

    #[test]
    fn releasing_modifier_while_action_key_is_held_releases_specific_contact() {
        let mut runtime = test_runtime(
            Vec::new(),
            vec![key_hold_binding("specific", "r", None, Some("shift"), 0.8)],
        );

        send_key(&mut runtime, HostKey::LeftShift, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::R, KeyTransition::Pressed);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);

        assert!(send_key(
            &mut runtime,
            HostKey::LeftShift,
            KeyTransition::Released
        ));
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down, TouchPhase::Up]);
    }

    #[test]
    fn deactivating_layer_releases_its_held_contacts() {
        let mut runtime = test_runtime(
            vec![hold_layer("combat", "tab")],
            vec![key_hold_binding(
                "layer-fire",
                "r",
                Some("combat"),
                None,
                0.2,
            )],
        );

        send_key(&mut runtime, HostKey::Tab, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::R, KeyTransition::Pressed);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);

        assert!(send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Released
        ));
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down, TouchPhase::Up]);
    }

    #[test]
    fn action_key_release_after_modifier_release_leaks_nothing() {
        let mut runtime = test_runtime(
            Vec::new(),
            vec![key_hold_binding("specific", "r", None, Some("shift"), 0.8)],
        );

        send_key(&mut runtime, HostKey::LeftShift, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::R, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::LeftShift, KeyTransition::Released);
        let frames_after_modifier_release = runtime.engine.injector().frames.len();

        assert!(!send_key(&mut runtime, HostKey::R, KeyTransition::Released));
        assert_eq!(
            runtime.engine.injector().frames.len(),
            frames_after_modifier_release
        );
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down, TouchPhase::Up]);
    }

    #[test]
    fn suspend_resets_layer_modifier_and_held_key_state() {
        let mut runtime = test_runtime(
            vec![toggle_layer("combat", "tab")],
            vec![key_hold_binding(
                "specific",
                "r",
                Some("combat"),
                Some("shift"),
                0.8,
            )],
        );

        send_key(&mut runtime, HostKey::Tab, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::Tab, KeyTransition::Released);
        send_key(&mut runtime, HostKey::LeftShift, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::R, KeyTransition::Pressed);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);

        runtime.suspend().unwrap();
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        let frames_after_suspend = runtime.engine.injector().frames.len();
        assert!(!send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert!(!send_key(&mut runtime, HostKey::R, KeyTransition::Released));
        assert_eq!(runtime.engine.injector().frames.len(), frames_after_suspend);
        send_key(&mut runtime, HostKey::LeftShift, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::Tab, KeyTransition::Pressed);
        assert_eq!(runtime.engine.injector().frames.len(), frames_after_suspend);
        assert_eq!(runtime.engine.state().active_contact_count(), 0);

        assert!(send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
    }

    #[test]
    fn suspend_resets_logical_state_even_when_contact_cleanup_must_be_retried() {
        let mut runtime = test_runtime(
            vec![toggle_layer("combat", "tab")],
            vec![key_hold_binding(
                "specific",
                "r",
                Some("combat"),
                Some("shift"),
                0.8,
            )],
        );
        send_key(&mut runtime, HostKey::Tab, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::LeftShift, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::R, KeyTransition::Pressed);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        runtime.engine.injector_mut().fail_next = true;

        assert!(runtime.suspend().is_err());
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(runtime.active_layers, 0);
        assert_eq!(runtime.held_keys, ModifierMask::EMPTY);
        assert_eq!(runtime.held_modifiers, ModifierMask::EMPTY);
        assert_eq!(runtime.held_mouse_buttons, 0);
        assert!(runtime.active_bindings.iter().all(|active| !active));

        runtime.suspend().unwrap();
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
    }

    #[test]
    fn failed_mouse_aim_cancel_does_not_leave_phantom_ads_after_recapture() {
        let mut runtime = test_runtime(
            Vec::new(),
            vec![BindingV2 {
                name: "aim".to_owned(),
                layer: None,
                modifier: None,
                input: InputV2::MouseMove,
                action: ActionV2::MouseAim {
                    region: NormalizedRect {
                        x: 0.4,
                        y: 0.1,
                        w: 0.5,
                        h: 0.7,
                    },
                    sensitivity: 1.0,
                    toggle_key: None,
                    recenter_threshold: 0.8,
                    recenter_gap_ms: 0,
                    ads_multiplier: Some(0.5),
                    reaffirm_ms: None,
                },
            }],
        );
        let controller = runtime.aim_controllers[0].as_ref().unwrap();
        let contact_id = controller.aim().contact_id();
        let origin = controller.aim().origin();
        send_mouse_button(
            &mut runtime,
            wroid_input::mouse::MouseButton::Right,
            MouseButtonTransition::Pressed,
        );
        runtime.engine.injector_mut().fail_next = true;

        assert!(runtime.suspend().is_err());
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        runtime.start().unwrap();
        assert!(runtime
            .handle_mouse(MouseEvent::Motion(RelativeMouseMotion::new(2, 0)))
            .unwrap());

        assert_eq!(
            runtime.engine.state().position(contact_id),
            Some(Point {
                x: origin.x + 2,
                y: origin.y,
            })
        );
    }

    #[test]
    fn layer_joystick_becomes_neutral_when_layer_deactivates() {
        let mut binding = joystick_binding("movement", "w", "a", "s", "d", 0.2);
        binding.layer = Some("combat".to_owned());
        let ActionV2::VirtualJoystick { mode, .. } = &mut binding.action else {
            unreachable!("joystick helper returns a joystick action")
        };
        *mode = JoystickMode::Toggle;
        let mut runtime = test_runtime(vec![hold_layer("combat", "tab")], vec![binding]);

        send_key(&mut runtime, HostKey::Tab, KeyTransition::Pressed);
        assert!(send_key(&mut runtime, HostKey::W, KeyTransition::Pressed));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);

        assert!(send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Released
        ));
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down, TouchPhase::Up]);
    }

    #[test]
    fn modifier_arriving_after_held_key_transitions_base_to_specific_safely() {
        let mut runtime = test_runtime(
            Vec::new(),
            vec![
                key_hold_binding("base", "r", None, None, 0.2),
                key_hold_binding("specific", "r", None, Some("shift"), 0.8),
            ],
        );

        assert!(send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert!(send_key(
            &mut runtime,
            HostKey::LeftShift,
            KeyTransition::Pressed
        ));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(
            emitted_phases(&runtime),
            [TouchPhase::Down, TouchPhase::Up, TouchPhase::Down]
        );
        assert_eq!(last_touch_event(&runtime).position.x, 799);

        assert!(send_key(
            &mut runtime,
            HostKey::LeftShift,
            KeyTransition::Released
        ));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(
            emitted_phases(&runtime),
            [
                TouchPhase::Down,
                TouchPhase::Up,
                TouchPhase::Down,
                TouchPhase::Up,
                TouchPhase::Down,
            ]
        );
        assert_eq!(last_touch_event(&runtime).position.x, 200);

        assert!(send_key(&mut runtime, HostKey::R, KeyTransition::Released));
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
    }

    #[test]
    fn key_cluster_suppression_is_per_key_and_keeps_unrelated_direction_usable() {
        let mut specific = joystick_binding("shift-movement", "w", "up", "down", "right", 0.8);
        specific.modifier = Some("shift".to_owned());
        let mut runtime = test_runtime(
            Vec::new(),
            vec![
                joystick_binding("movement", "w", "a", "s", "d", 0.2),
                specific,
            ],
        );

        send_key(&mut runtime, HostKey::A, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::LeftShift, KeyTransition::Pressed);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        let base_contact = joystick_contact(&runtime, "movement");
        assert_eq!(
            runtime.engine.state().position(base_contact),
            Some(Point { x: 100, y: 799 })
        );

        assert!(send_key(&mut runtime, HostKey::W, KeyTransition::Pressed));
        assert_eq!(runtime.engine.state().active_contact_count(), 2);
        assert_eq!(
            runtime.engine.state().position(base_contact),
            Some(Point { x: 100, y: 799 })
        );
        let specific_contact = joystick_contact(&runtime, "shift-movement");
        assert_eq!(
            runtime.engine.state().position(specific_contact),
            Some(Point { x: 799, y: 699 })
        );
    }

    #[test]
    fn modifier_release_reconciles_joystick_owners_before_base_addition() {
        assert_modifier_joystick_handoff_order(false);
    }

    #[test]
    fn modifier_release_reconciles_joystick_owners_before_base_addition_when_reversed() {
        assert_modifier_joystick_handoff_order(true);
    }

    #[test]
    fn mouse_button_obeys_layer_modifier_press_gating_and_release_cleanup() {
        let mut runtime = test_runtime(
            vec![hold_layer("combat", "tab")],
            vec![mouse_hold_binding(
                "fire",
                "left",
                Some("combat"),
                Some("shift"),
                0.8,
            )],
        );

        assert!(!send_mouse_button(
            &mut runtime,
            wroid_input::mouse::MouseButton::Left,
            MouseButtonTransition::Pressed
        ));
        assert!(!send_mouse_button(
            &mut runtime,
            wroid_input::mouse::MouseButton::Left,
            MouseButtonTransition::Released
        ));
        send_key(&mut runtime, HostKey::Tab, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::LeftShift, KeyTransition::Pressed);

        assert!(send_mouse_button(
            &mut runtime,
            wroid_input::mouse::MouseButton::Left,
            MouseButtonTransition::Pressed
        ));
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert!(send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Released
        ));
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        let frames_after_cleanup = runtime.engine.injector().frames.len();
        assert!(!send_mouse_button(
            &mut runtime,
            wroid_input::mouse::MouseButton::Left,
            MouseButtonTransition::Released
        ));
        assert_eq!(runtime.engine.injector().frames.len(), frames_after_cleanup);
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down, TouchPhase::Up]);
    }

    #[test]
    fn tap_does_not_refire_when_modifier_or_layer_state_changes() {
        let mut runtime = test_runtime(
            vec![toggle_layer("combat", "tab")],
            vec![
                key_tap_binding("base", "r", None, None, 0.2),
                key_tap_binding("specific", "r", None, Some("shift"), 0.5),
                key_tap_binding("layer", "r", Some("combat"), None, 0.8),
            ],
        );

        assert!(send_key(&mut runtime, HostKey::R, KeyTransition::Pressed));
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down, TouchPhase::Up]);
        assert!(!send_key(
            &mut runtime,
            HostKey::LeftShift,
            KeyTransition::Pressed
        ));
        assert!(!send_key(
            &mut runtime,
            HostKey::Tab,
            KeyTransition::Pressed
        ));
        assert!(!send_key(
            &mut runtime,
            HostKey::LeftShift,
            KeyTransition::Released
        ));
        assert_eq!(emitted_phases(&runtime), [TouchPhase::Down, TouchPhase::Up]);
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
    }

    #[test]
    fn mouse_aim_stays_live_when_its_declared_layer_is_inactive() {
        let mut runtime = test_runtime(
            vec![hold_layer("combat", "tab")],
            vec![BindingV2 {
                name: "aim".to_owned(),
                layer: Some("combat".to_owned()),
                modifier: None,
                input: InputV2::MouseMove,
                action: ActionV2::MouseAim {
                    region: NormalizedRect {
                        x: 0.4,
                        y: 0.1,
                        w: 0.5,
                        h: 0.7,
                    },
                    sensitivity: 1.0,
                    toggle_key: None,
                    recenter_threshold: 0.8,
                    recenter_gap_ms: 0,
                    ads_multiplier: None,
                    reaffirm_ms: None,
                },
            }],
        );
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        let frames_before_motion = runtime.engine.injector().frames.len();

        assert!(runtime
            .handle_mouse(MouseEvent::Motion(RelativeMouseMotion::new(5, -3)))
            .unwrap());
        assert_eq!(
            runtime.engine.injector().frames.len(),
            frames_before_motion + 1
        );
        assert_eq!(last_touch_event(&runtime).phase, TouchPhase::Move);
    }

    #[test]
    fn declared_layer_64_uses_the_high_mask_bit_without_colliding_with_base() {
        assert_eq!(
            layer_mask_bit(wroid_runtime::LayerId::new(64)),
            Some(1_u64 << 63)
        );
        assert_eq!(layer_mask_bit(wroid_runtime::LayerId::BASE), None);
    }

    fn test_runtime(
        layers: Vec<LayerV2>,
        bindings: Vec<BindingV2>,
    ) -> UnifiedRuntime<RecordingInjector> {
        let profile = ProfileV2 {
            schema_version: 2,
            name: "dispatch test".to_owned(),
            package_name: "com.example.dispatch".to_owned(),
            orientation: Default::default(),
            layers,
            bindings,
        };
        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1000,
                height: 1000,
            },
        )
        .unwrap();
        let mut runtime = UnifiedRuntime::new(plan, RecordingInjector::default(), false).unwrap();
        runtime.start().unwrap();
        runtime
    }

    fn hold_layer(name: &str, key: &str) -> LayerV2 {
        LayerV2 {
            name: name.to_owned(),
            activation: LayerActivation::Hold {
                key: key.to_owned(),
            },
        }
    }

    fn toggle_layer(name: &str, key: &str) -> LayerV2 {
        LayerV2 {
            name: name.to_owned(),
            activation: LayerActivation::Toggle {
                key: key.to_owned(),
            },
        }
    }

    fn key_hold_binding(
        name: &str,
        key: &str,
        layer: Option<&str>,
        modifier: Option<&str>,
        x: f64,
    ) -> BindingV2 {
        point_binding(
            name,
            InputV2::Key {
                key: key.to_owned(),
            },
            layer,
            modifier,
            x,
            false,
        )
    }

    fn mouse_hold_binding(
        name: &str,
        button: &str,
        layer: Option<&str>,
        modifier: Option<&str>,
        x: f64,
    ) -> BindingV2 {
        point_binding(
            name,
            InputV2::MouseButton {
                button: button.to_owned(),
            },
            layer,
            modifier,
            x,
            false,
        )
    }

    fn key_tap_binding(
        name: &str,
        key: &str,
        layer: Option<&str>,
        modifier: Option<&str>,
        x: f64,
    ) -> BindingV2 {
        point_binding(
            name,
            InputV2::Key {
                key: key.to_owned(),
            },
            layer,
            modifier,
            x,
            true,
        )
    }

    fn point_binding(
        name: &str,
        input: InputV2,
        layer: Option<&str>,
        modifier: Option<&str>,
        x: f64,
        tap: bool,
    ) -> BindingV2 {
        let point = NormalizedPoint { x, y: 0.5 };
        BindingV2 {
            name: name.to_owned(),
            layer: layer.map(str::to_owned),
            modifier: modifier.map(str::to_owned),
            input,
            action: if tap {
                ActionV2::Tap { point, hold: false }
            } else {
                ActionV2::Hold { point }
            },
        }
    }

    fn send_key(
        runtime: &mut UnifiedRuntime<RecordingInjector>,
        key: HostKey,
        transition: KeyTransition,
    ) -> bool {
        runtime
            .handle_keyboard(HostKeyEvent::new(key, transition))
            .unwrap()
    }

    fn send_mouse_button(
        runtime: &mut UnifiedRuntime<RecordingInjector>,
        button: wroid_input::mouse::MouseButton,
        transition: MouseButtonTransition,
    ) -> bool {
        runtime
            .handle_mouse(MouseEvent::Button(
                wroid_input::mouse::MouseButtonEvent::new(button, transition),
            ))
            .unwrap()
    }

    fn emitted_phases(runtime: &UnifiedRuntime<RecordingInjector>) -> Vec<TouchPhase> {
        runtime
            .engine
            .injector()
            .frames
            .iter()
            .flat_map(|frame| frame.events().iter().map(|event| event.phase))
            .collect()
    }

    fn last_touch_event(runtime: &UnifiedRuntime<RecordingInjector>) -> TouchEvent {
        *runtime
            .engine
            .injector()
            .frames
            .last()
            .expect("runtime emitted a touch frame")
            .events()
            .last()
            .expect("touch frame contains an event")
    }

    fn joystick_contact(runtime: &UnifiedRuntime<RecordingInjector>, name: &str) -> ContactId {
        let control = runtime.plan.control(name).unwrap();
        let RuntimeControlAction::VirtualJoystick { joystick, .. } = &control.action else {
            panic!("binding must be a joystick")
        };
        joystick.contact_id()
    }

    fn joystick_binding(
        name: &str,
        up: &str,
        left: &str,
        down: &str,
        right: &str,
        center_x: f64,
    ) -> BindingV2 {
        BindingV2 {
            name: name.to_owned(),
            layer: None,
            modifier: None,
            input: InputV2::KeyCluster {
                up: up.to_owned(),
                left: left.to_owned(),
                down: down.to_owned(),
                right: right.to_owned(),
            },
            action: ActionV2::VirtualJoystick {
                center: NormalizedPoint {
                    x: center_x,
                    y: 0.8,
                },
                radius: 0.1,
                dead_zone: 0.02,
                mode: JoystickMode::Hold,
                reaffirm_ms: Some(50),
                slow_key: None,
                slow_scale: None,
                slow_mode: JoystickWalkMode::default(),
            },
        }
    }

    fn assert_modifier_joystick_handoff_order(specific_first: bool) {
        let base = joystick_binding("movement", "w", "a", "s", "d", 0.2);
        let mut specific = joystick_binding("shift-movement", "w", "up", "down", "right", 0.8);
        specific.modifier = Some("shift".to_owned());
        let bindings = if specific_first {
            vec![specific, base]
        } else {
            vec![base, specific]
        };
        let mut runtime = test_runtime(Vec::new(), bindings);

        send_key(&mut runtime, HostKey::A, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::LeftShift, KeyTransition::Pressed);
        send_key(&mut runtime, HostKey::W, KeyTransition::Pressed);
        let base_contact = joystick_contact(&runtime, "movement");
        let specific_contact = joystick_contact(&runtime, "shift-movement");
        assert_eq!(
            runtime.engine.state().position(base_contact),
            Some(Point { x: 100, y: 799 })
        );
        assert_eq!(
            runtime.engine.state().position(specific_contact),
            Some(Point { x: 799, y: 699 })
        );
        let frames_before_release = runtime.engine.injector().frames.len();

        assert!(send_key(
            &mut runtime,
            HostKey::LeftShift,
            KeyTransition::Released
        ));

        let events: Vec<_> = runtime.engine.injector().frames[frames_before_release..]
            .iter()
            .flat_map(|frame| frame.events().iter().copied())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].contact_id, specific_contact);
        assert_eq!(events[0].phase, TouchPhase::Up);
        assert_eq!(events[1].contact_id, base_contact);
        assert_eq!(events[1].phase, TouchPhase::Move);
        assert_eq!(events[1].position, Point { x: 129, y: 728 });
        assert_eq!(runtime.engine.state().active_contact_count(), 1);
        assert_eq!(
            runtime.engine.state().position(base_contact),
            Some(Point { x: 129, y: 728 })
        );
        assert_eq!(runtime.engine.state().position(specific_contact), None);
    }

    fn peak_contacts(frames: &[TouchFrame]) -> usize {
        let mut active = BTreeSet::new();
        let mut peak = 0;
        for frame in frames {
            for event in frame.events() {
                match event.phase {
                    TouchPhase::Down => {
                        active.insert(event.contact_id);
                    }
                    TouchPhase::Up | TouchPhase::Cancel => {
                        active.remove(&event.contact_id);
                    }
                    TouchPhase::Move => {}
                }
            }
            peak = peak.max(active.len());
        }
        peak
    }

    #[test]
    fn session_metrics_track_frames_peak_contacts_and_percentiles() {
        let mut injector = SessionMetricsInjector::new(RecordingInjector::default());
        injector
            .inject(&TouchFrame::new([
                TouchEvent::new(ContactId::new(1), TouchPhase::Down, Point { x: 10, y: 10 }),
                TouchEvent::new(ContactId::new(2), TouchPhase::Down, Point { x: 20, y: 20 }),
            ]))
            .unwrap();
        injector
            .inject(&TouchFrame::new([
                TouchEvent::new(
                    ContactId::new(1),
                    TouchPhase::Cancel,
                    Point { x: 10, y: 10 },
                ),
                TouchEvent::new(
                    ContactId::new(2),
                    TouchPhase::Cancel,
                    Point { x: 20, y: 20 },
                ),
            ]))
            .unwrap();

        assert_eq!(injector.frames_submitted, 2);
        assert_eq!(injector.peak_contacts, 2);
        assert_eq!(
            latency_summary_micros(&[
                Duration::from_micros(10),
                Duration::from_micros(20),
                Duration::from_micros(30),
            ]),
            LatencySummary {
                samples: 3,
                p50: 20,
                p95: 30,
                p99: 30,
                max: 30,
            }
        );
        assert_eq!(
            latency_metrics(&[
                Duration::from_micros(10),
                Duration::from_micros(20),
                Duration::from_micros(30),
            ]),
            LatencyMetrics {
                samples: 3,
                p50_micros: 20,
                p95_micros: 30,
                p99_micros: 30,
                max_micros: 30,
            }
        );

        let recent = SystemTime::now() - Duration::from_millis(5);
        assert!(kernel_event_age(recent).is_some_and(|age| age >= Duration::from_millis(5)));
        assert!(kernel_event_age(SystemTime::UNIX_EPOCH).is_none());
        assert!(kernel_event_age(SystemTime::now() + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn radial_select_sweeps_through_timed_phases() {
        let mut runtime = test_runtime(
            vec![],
            vec![BindingV2 {
                name: "wheel".to_owned(),
                layer: None,
                modifier: None,
                input: InputV2::Key {
                    key: "1".to_owned(),
                },
                action: ActionV2::RadialSelect {
                    center: NormalizedPoint { x: 0.5, y: 0.5 },
                    distance: 0.07,
                    direction: wroid_core::profile_v2::RadialDirection::Down,
                },
            }],
        );

        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::Num1,
                KeyTransition::Pressed,
            ))
            .unwrap();

        // Only the wheel-open touch so far; the swipe is scheduled.
        assert_eq!(runtime.engine.injector().frames.len(), 1);
        assert_eq!(runtime.pending_radials.len(), 1);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);

        // Before the dwell elapses nothing advances.
        runtime
            .advance_pending_radials(Duration::from_millis(0))
            .unwrap();
        assert_eq!(runtime.engine.injector().frames.len(), 1);

        // After the down dwell the contact swipes to the sector.
        let move_at = RADIAL_DOWN_DWELL + Duration::from_millis(1);
        runtime.advance_pending_radials(move_at).unwrap();
        assert_eq!(runtime.engine.injector().frames.len(), 2);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);

        // Still holding before the move dwell.
        runtime
            .advance_pending_radials(move_at + Duration::from_millis(1))
            .unwrap();
        assert_eq!(runtime.engine.injector().frames.len(), 2);

        // Then the release completes the gesture.
        let release_at = move_at + RADIAL_MOVE_DWELL + Duration::from_millis(1);
        runtime.advance_pending_radials(release_at).unwrap();
        assert_eq!(runtime.engine.injector().frames.len(), 3);
        assert!(runtime.pending_radials.is_empty());
        assert_eq!(runtime.engine.state().active_contact_count(), 0);

        let frames = &runtime.engine.injector().frames;
        assert_eq!(frames[0].events()[0].phase, TouchPhase::Down);
        assert_eq!(frames[0].events()[0].position, Point { x: 500, y: 500 });
        assert_eq!(frames[1].events()[0].phase, TouchPhase::Move);
        // distance 0.07 * min(1000,1000) = 70px straight down.
        assert_eq!(frames[1].events()[0].position, Point { x: 500, y: 570 });
        assert_eq!(frames[2].events()[0].phase, TouchPhase::Up);
        runtime.stop().unwrap();
    }

    #[test]
    fn radial_repress_while_in_flight_finishes_previous_gesture() {
        let mut runtime = test_runtime(
            vec![],
            vec![BindingV2 {
                name: "wheel".to_owned(),
                layer: None,
                modifier: None,
                input: InputV2::Key {
                    key: "1".to_owned(),
                },
                action: ActionV2::RadialSelect {
                    center: NormalizedPoint { x: 0.5, y: 0.5 },
                    distance: 0.07,
                    direction: wroid_core::profile_v2::RadialDirection::Down,
                },
            }],
        );

        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::Num1,
                KeyTransition::Pressed,
            ))
            .unwrap();
        assert_eq!(runtime.engine.injector().frames.len(), 1);

        // Re-press during the down dwell must not begin the contact twice.
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::Num1,
                KeyTransition::Pressed,
            ))
            .unwrap();
        // The old gesture released its contact; the new one is down again.
        let frames = &runtime.engine.injector().frames;
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[1].events()[0].phase, TouchPhase::Up);
        assert_eq!(frames[2].events()[0].phase, TouchPhase::Down);
        assert_eq!(runtime.pending_radials.len(), 1);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);

        // The replacement gesture completes normally.
        runtime
            .advance_pending_radials(RADIAL_DOWN_DWELL + Duration::from_millis(1))
            .unwrap();
        runtime
            .advance_pending_radials(
                2 * RADIAL_DOWN_DWELL + RADIAL_MOVE_DWELL + Duration::from_millis(2),
            )
            .unwrap();
        assert!(runtime.pending_radials.is_empty());
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        runtime.stop().unwrap();
    }

    #[test]
    fn mouse_side_button_triggers_radial_and_middle_tap() {
        let mut runtime = test_runtime(
            vec![],
            vec![
                BindingV2 {
                    name: "wheel_side".to_owned(),
                    layer: None,
                    modifier: None,
                    input: InputV2::MouseButton {
                        button: "side".to_owned(),
                    },
                    action: ActionV2::RadialSelect {
                        center: NormalizedPoint { x: 0.5, y: 0.5 },
                        distance: 0.07,
                        direction: wroid_core::profile_v2::RadialDirection::Up,
                    },
                },
                BindingV2 {
                    name: "middle_tap".to_owned(),
                    layer: None,
                    modifier: None,
                    input: InputV2::MouseButton {
                        button: "middle".to_owned(),
                    },
                    action: ActionV2::Tap {
                        point: NormalizedPoint { x: 0.9, y: 0.3 },
                        hold: false,
                    },
                },
            ],
        );

        // Middle click taps immediately.
        assert!(send_mouse_button(
            &mut runtime,
            wroid_input::mouse::MouseButton::Middle,
            MouseButtonTransition::Pressed
        ));
        assert_eq!(
            emitted_phases(&runtime),
            vec![TouchPhase::Down, TouchPhase::Up]
        );

        // Side button opens the wheel and completes the timed gesture.
        assert!(send_mouse_button(
            &mut runtime,
            wroid_input::mouse::MouseButton::Side,
            MouseButtonTransition::Pressed
        ));
        assert_eq!(runtime.pending_radials.len(), 1);
        runtime
            .advance_pending_radials(RADIAL_DOWN_DWELL + Duration::from_millis(1))
            .unwrap();
        runtime
            .advance_pending_radials(
                RADIAL_DOWN_DWELL + RADIAL_MOVE_DWELL + Duration::from_millis(2),
            )
            .unwrap();
        assert!(runtime.pending_radials.is_empty());
        assert_eq!(runtime.engine.state().active_contact_count(), 0);
        runtime.stop().unwrap();
    }

    #[test]
    fn walk_key_shrinks_joystick_deflection() {
        let mut runtime = test_runtime(
            vec![],
            vec![BindingV2 {
                name: "movement".to_owned(),
                layer: None,
                modifier: None,
                input: InputV2::KeyCluster {
                    up: "w".to_owned(),
                    left: "a".to_owned(),
                    down: "s".to_owned(),
                    right: "d".to_owned(),
                },
                action: ActionV2::VirtualJoystick {
                    center: NormalizedPoint { x: 0.5, y: 0.8 },
                    radius: 0.1,
                    dead_zone: 0.0,
                    mode: JoystickMode::Hold,
                    reaffirm_ms: Some(50),
                    slow_key: Some("shift".to_owned()),
                    slow_scale: Some(0.5),
                    slow_mode: JoystickWalkMode::Hold,
                },
            }],
        );

        // Full deflection without the walk key.
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::W,
                KeyTransition::Pressed,
            ))
            .unwrap();
        let full = runtime.engine.injector().frames.last().unwrap().events()[0]
            .position
            .y;
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::W,
                KeyTransition::Released,
            ))
            .unwrap();

        // Hold the walk key: same direction, half the distance from center.
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::LeftShift,
                KeyTransition::Pressed,
            ))
            .unwrap();
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::W,
                KeyTransition::Pressed,
            ))
            .unwrap();
        let walked = runtime.engine.injector().frames.last().unwrap().events()[0]
            .position
            .y;

        let center_y = 800; // 0.8 * 1000
        let full_reach = center_y - i64::from(full);
        let walked_reach = center_y - i64::from(walked);
        assert!(full_reach > 0);
        assert!((walked_reach * 2 - full_reach).abs() <= 2);

        // Releasing the walk key while W is still held must restore full
        // deflection immediately (no stale slow position).
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::LeftShift,
                KeyTransition::Released,
            ))
            .unwrap();
        let restored = runtime.engine.injector().frames.last().unwrap().events()[0]
            .position
            .y;
        assert_eq!(restored, full);

        // And re-engaging walk mid-stride scales back down again.
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::LeftShift,
                KeyTransition::Pressed,
            ))
            .unwrap();
        let walked_again = runtime.engine.injector().frames.last().unwrap().events()[0]
            .position
            .y;
        assert_eq!(walked_again, walked);

        // Diagonal while walking: W+D under Shift must also stay scaled.
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::LeftShift,
                KeyTransition::Released,
            ))
            .unwrap();
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::D,
                KeyTransition::Pressed,
            ))
            .unwrap();
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::LeftShift,
                KeyTransition::Pressed,
            ))
            .unwrap();
        let diag_walked = runtime.engine.injector().frames.last().unwrap().events()[0].position;
        // Full-speed diagonal reference for comparison.
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::LeftShift,
                KeyTransition::Released,
            ))
            .unwrap();
        let diag_full = runtime.engine.injector().frames.last().unwrap().events()[0].position;
        let center_x = 500_i64; // 0.5 * 1000
        let walked_dx = i64::from(diag_walked.x) - center_x;
        let full_dx = i64::from(diag_full.x) - center_x;
        assert!(full_dx > 0);
        for (i, f) in runtime
            .engine
            .injector()
            .frames
            .iter()
            .enumerate()
            .skip(runtime.engine.injector().frames.len() - 6)
        {
            eprintln!("frame {i}: {:?}", f.events());
        }
        eprintln!("diag_walked={diag_walked:?} diag_full={diag_full:?}");
        assert!(
            (walked_dx * 2 - full_dx).abs() <= 4,
            "walked dx {walked_dx} should be ~half of full dx {full_dx}"
        );
        runtime.stop().unwrap();
    }

    #[test]
    fn user_profile_dispatches_editor_added_keys() {
        let path =
            std::path::PathBuf::from("/home/supergut/.config/wroid/profiles-v2/standoff-2.json");
        if !path.exists() {
            return;
        }
        let profile = wroid_core::profile_v2::ProfileV2::load_from_path(&path).unwrap();
        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap();
        let mut runtime = UnifiedRuntime::new(plan, RecordingInjector::default(), false).unwrap();
        runtime.start().unwrap();

        for key in [
            wroid_input::HostKey::E,
            wroid_input::HostKey::G,
            wroid_input::HostKey::F,
        ] {
            runtime.engine.injector_mut().frames.clear();
            let submitted = send_key(&mut runtime, key, KeyTransition::Pressed);
            let frames = runtime.engine.injector().frames.len();
            // Plain taps submit down+up at once; sustained taps (F is
            // hold-enabled in this profile) submit only Down until release.
            assert!(
                submitted && frames >= 1,
                "key {key:?} produced submitted={submitted} frames={frames}"
            );
            send_key(&mut runtime, key, KeyTransition::Released);
            let total = runtime.engine.injector().frames.len();
            assert!(total >= 2, "release completed gesture: {total} frames");
        }
        runtime.stop().unwrap();
    }

    #[test]
    fn replace_plan_hot_swaps_bindings_without_restart() {
        let mut runtime = test_runtime(
            vec![],
            vec![point_binding(
                "reload",
                InputV2::Key {
                    key: "r".to_owned(),
                },
                None,
                None,
                0.9,
                true,
            )],
        );

        // Old plan works.
        assert!(send_key(
            &mut runtime,
            wroid_input::HostKey::R,
            KeyTransition::Pressed
        ));
        let old_frames = runtime.engine.injector().frames.len();
        assert!(old_frames >= 2);

        // New plan drops "r", adds "g" at a different point.
        let new_plan = RuntimeControlPlan::from_profile_v2(
            &ProfileV2 {
                schema_version: 2,
                name: "hot reload".to_owned(),
                package_name: "com.example.hot".to_owned(),
                orientation: Default::default(),
                layers: Vec::new(),
                bindings: vec![BindingV2 {
                    name: "grenade".to_owned(),
                    layer: None,
                    modifier: None,
                    input: InputV2::Key {
                        key: "g".to_owned(),
                    },
                    action: ActionV2::Tap {
                        point: NormalizedPoint { x: 0.1, y: 0.1 },
                        hold: false,
                    },
                }],
            },
            Resolution {
                width: 1000,
                height: 1000,
            },
        )
        .unwrap();
        runtime.replace_plan(new_plan).unwrap();

        // The dropped key no longer dispatches...
        runtime.engine.injector_mut().frames.clear();
        assert!(!send_key(
            &mut runtime,
            wroid_input::HostKey::R,
            KeyTransition::Pressed
        ));
        assert!(runtime.engine.injector().frames.is_empty());

        // ...and the added key fires immediately after the swap.
        assert!(send_key(
            &mut runtime,
            wroid_input::HostKey::G,
            KeyTransition::Pressed
        ));
        let phases = emitted_phases(&runtime);
        assert_eq!(phases, vec![TouchPhase::Down, TouchPhase::Up]);
        runtime.stop().unwrap();
    }

    #[test]
    fn sustained_tap_down_on_press_up_on_release() {
        let mut runtime = test_runtime(
            vec![],
            vec![BindingV2 {
                name: "defuse".to_owned(),
                layer: None,
                modifier: None,
                input: InputV2::Key {
                    key: "f".to_owned(),
                },
                action: ActionV2::Tap {
                    point: NormalizedPoint { x: 0.5, y: 0.5 },
                    hold: true,
                },
            }],
        );

        // Press: only the Down phase; the contact stays.
        assert!(send_key(
            &mut runtime,
            wroid_input::HostKey::F,
            KeyTransition::Pressed
        ));
        assert_eq!(emitted_phases(&runtime), vec![TouchPhase::Down]);
        assert_eq!(runtime.engine.state().active_contact_count(), 1);

        // Release: the Up completes the gesture.
        assert!(send_key(
            &mut runtime,
            wroid_input::HostKey::F,
            KeyTransition::Released
        ));
        assert_eq!(
            emitted_phases(&runtime),
            vec![TouchPhase::Down, TouchPhase::Up]
        );
        assert_eq!(runtime.engine.state().active_contact_count(), 0);

        // A second cycle works identically (no stuck state).
        assert!(send_key(
            &mut runtime,
            wroid_input::HostKey::F,
            KeyTransition::Pressed
        ));
        assert!(send_key(
            &mut runtime,
            wroid_input::HostKey::F,
            KeyTransition::Released
        ));
        assert_eq!(
            emitted_phases(&runtime),
            vec![
                TouchPhase::Down,
                TouchPhase::Up,
                TouchPhase::Down,
                TouchPhase::Up
            ]
        );
        runtime.stop().unwrap();
    }

    #[test]
    fn plain_taps_still_fire_immediately() {
        let mut runtime = test_runtime(
            vec![],
            vec![BindingV2 {
                name: "plain".to_owned(),
                layer: None,
                modifier: None,
                input: InputV2::Key {
                    key: "r".to_owned(),
                },
                action: ActionV2::Tap {
                    point: NormalizedPoint { x: 0.5, y: 0.5 },
                    hold: false,
                },
            }],
        );

        assert!(send_key(
            &mut runtime,
            wroid_input::HostKey::R,
            KeyTransition::Pressed
        ));
        assert_eq!(
            emitted_phases(&runtime),
            vec![TouchPhase::Down, TouchPhase::Up]
        );

        // Releasing the key must not duplicate anything for a plain tap.
        send_key(
            &mut runtime,
            wroid_input::HostKey::R,
            KeyTransition::Released,
        );
        assert_eq!(
            emitted_phases(&runtime),
            vec![TouchPhase::Down, TouchPhase::Up]
        );
        runtime.stop().unwrap();
    }

    #[test]
    fn walk_toggle_mode_persists_after_key_release() {
        let mut runtime = test_runtime(
            vec![],
            vec![BindingV2 {
                name: "movement".to_owned(),
                layer: None,
                modifier: None,
                input: InputV2::KeyCluster {
                    up: "w".to_owned(),
                    left: "a".to_owned(),
                    down: "s".to_owned(),
                    right: "d".to_owned(),
                },
                action: ActionV2::VirtualJoystick {
                    center: NormalizedPoint { x: 0.5, y: 0.8 },
                    radius: 0.1,
                    dead_zone: 0.0,
                    mode: JoystickMode::Hold,
                    reaffirm_ms: Some(50),
                    slow_key: Some("shift".to_owned()),
                    slow_scale: Some(0.5),
                    slow_mode: JoystickWalkMode::Toggle,
                },
            }],
        );

        // Full deflection baseline.
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::W,
                KeyTransition::Pressed,
            ))
            .unwrap();
        let full = runtime.engine.injector().frames.last().unwrap().events()[0]
            .position
            .y;

        // Tap shift ON: walk engages even though shift is released.
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::LeftShift,
                KeyTransition::Pressed,
            ))
            .unwrap();
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::LeftShift,
                KeyTransition::Released,
            ))
            .unwrap();
        let walked = runtime.engine.injector().frames.last().unwrap().events()[0]
            .position
            .y;
        let center_y = 800_i64;
        assert!((center_y - i64::from(walked)) * 2 - (center_y - i64::from(full)) <= 2);

        // Tap shift OFF: back to full speed.
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::LeftShift,
                KeyTransition::Pressed,
            ))
            .unwrap();
        runtime
            .handle_keyboard(HostKeyEvent::new(
                wroid_input::HostKey::LeftShift,
                KeyTransition::Released,
            ))
            .unwrap();
        let restored = runtime.engine.injector().frames.last().unwrap().events()[0]
            .position
            .y;
        assert_eq!(restored, full);
        runtime.stop().unwrap();
    }
}
