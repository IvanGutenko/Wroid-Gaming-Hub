use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use wroid_core::profile_v2::{ActionV2, LayerActivation, ProfileV2};
use wroid_input::{discover_keyboard_devices, discover_mouse_devices, InputDeviceInfo};

use crate::backend::InputExecutor;

use super::compatibility::{self, CompatibilityReport};
use super::desktop_webview::{self, HUB_WINDOW};
use super::game_catalog::{family_for_package, GAME_FAMILIES};
use super::graphics::GraphicsReport;
use super::local_web_app::{LocalWebApp, WebUiMode};
use super::preferences::{self, UserPreferences};
use super::terminal::spawn_terminal;

const INDEX_HTML: &str = include_str!("../../assets/hub/index.html");
const STYLES_CSS: &str = include_str!("../../assets/hub/styles.css");
const CONTROL_CHIPS_JS: &str = include_str!("../../assets/hub/control-chips.js");
const COMPATIBILITY_STATE_JS: &str = include_str!("../../assets/hub/compatibility-state.js");
const APP_JS: &str = include_str!("../../assets/hub/app.js");
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_APK_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const APK_STATUS_BYTES: u64 = 16 * 1024;
const APK_STATUS_DETAIL_BYTES: usize = 4 * 1024;
const SIDELOAD_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const DESKTOP_READY_ATTEMPTS: usize = 240;
const DESKTOP_READY_INTERVAL: Duration = Duration::from_millis(250);
const PACKAGE_DISCOVERY_ATTEMPTS: usize = 40;
const INPUT_SELF_TEST_SECONDS: u64 = 20;

const STARTER_PROFILES: [StarterProfile; 4] = [
    StarterProfile {
        id: "pubg-mobile",
        source: include_str!("../../../../profiles/examples/pubg-v2.json"),
    },
    StarterProfile {
        id: "free-fire",
        source: include_str!("../../../../profiles/examples/freefire-v2.json"),
    },
    StarterProfile {
        id: "brawl-stars",
        source: include_str!("../../../../profiles/examples/brawlstars-v2.json"),
    },
    StarterProfile {
        id: "standoff-2",
        source: include_str!("../../../../profiles/examples/standoff2-v2.json"),
    },
];

struct StarterProfile {
    id: &'static str,
    source: &'static str,
}

#[derive(Debug)]
struct LibraryProfile {
    id: String,
    path: PathBuf,
    profile: ProfileV2,
}

pub(crate) fn run_hub(port: u16, mode: WebUiMode, profiles_dir: Option<PathBuf>) -> Result<()> {
    if mode == WebUiMode::Native {
        return desktop_webview::run_native_app(HUB_WINDOW, move || start_hub(port, profiles_dir));
    }
    let app = start_hub(port, profiles_dir)?;

    println!("Wroid Gaming Hub");
    println!("The server listens on localhost only. Ctrl+C stops it.");
    match mode {
        WebUiMode::Browser => open_url(&app.authenticated_url()),
        WebUiMode::Headless => println!("Hub: {}", app.authenticated_url()),
        WebUiMode::Native => unreachable!("handled before starting the native application"),
    }
    app.wait()
}

pub(crate) fn start_hub(port: u16, profiles_dir: Option<PathBuf>) -> Result<LocalWebApp> {
    start_hub_in(port, profiles_dir, default_sideload_directory()?)
}

fn start_hub_in(
    port: u16,
    profiles_dir: Option<PathBuf>,
    sideload_directory: PathBuf,
) -> Result<LocalWebApp> {
    if effective_uid_from_proc().unwrap_or(u32::MAX) == 0 {
        bail!("the gaming hub must run as the desktop user, without sudo");
    }

    let profiles_dir = match profiles_dir {
        Some(path) => absolute_directory(path)?,
        None => default_profiles_dir()?,
    };
    bootstrap_library(&profiles_dir)?;
    let sideload_directory = secure_sideload_directory(&sideload_directory)?;
    cleanup_stale_sideload(&sideload_directory, SystemTime::now())?;

    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
        .context("failed to bind the local gaming hub")?;
    let address = listener.local_addr()?;
    let token = local_token()?;
    listener
        .set_nonblocking(true)
        .context("failed to configure the local gaming hub")?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = Arc::clone(&shutdown);
    LocalWebApp::spawn(address, token.clone(), Arc::clone(&shutdown), move || {
        while !server_shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let profiles_dir = profiles_dir.clone();
                    let sideload_directory = sideload_directory.clone();
                    let token = token.clone();
                    let shutdown = Arc::clone(&server_shutdown);
                    thread::spawn(move || {
                        serve_connection(
                            stream,
                            &profiles_dir,
                            &sideload_directory,
                            &token,
                            &shutdown,
                        );
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error).context("gaming hub connection failed"),
            }
        }
        Ok(())
    })
}

fn serve_connection(
    mut stream: TcpStream,
    directory: &Path,
    sideload_directory: &Path,
    token: &str,
    shutdown: &AtomicBool,
) {
    if let Err(error) = stream.set_nonblocking(false) {
        eprintln!("Warning: could not configure hub client: {error}");
        return;
    }
    if let Err(error) = stream.set_read_timeout(Some(Duration::from_secs(5))) {
        eprintln!("Warning: could not configure hub client timeout: {error}");
        return;
    }
    match read_request_head(&mut stream) {
        Ok(head) if head.method == "POST" && request_route(&head.target) == "/api/apk/upload" => {
            let response = if !target_is_authorized(&head.target, token) {
                Response::json(403, r#"{"ok":false,"error":"Invalid hub token"}"#)
            } else {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                match local_token() {
                    Ok(ticket) => handle_apk_upload_with_ticket(
                        head,
                        &mut stream,
                        sideload_directory,
                        &ticket,
                    ),
                    Err(error) => Response::json(500, &json_error(&error.to_string())),
                }
            };
            if let Err(error) = write_response(&mut stream, response) {
                eprintln!("Warning: hub client disconnected: {error}");
            }
        }
        Ok(head) => match read_bounded_body(head, &mut stream, MAX_BODY_BYTES) {
            Ok(request) => {
                let (response, close) =
                    handle_request(&request, directory, sideload_directory, token);
                if let Err(error) = write_response(&mut stream, response) {
                    eprintln!("Warning: hub client disconnected: {error}");
                }
                if close {
                    shutdown.store(true, Ordering::Release);
                }
            }
            Err(error) => {
                let _ = write_response(
                    &mut stream,
                    Response::json(400, &json_error(&error.to_string())),
                );
            }
        },
        Err(error) => {
            let _ = write_response(
                &mut stream,
                Response::json(400, &json_error(&error.to_string())),
            );
        }
    }
}

fn default_sideload_directory() -> Result<PathBuf> {
    let state_home = env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|home| home.join(".local").join("state"))
        })
        .context("HOME and XDG_STATE_HOME are unavailable for APK sideload state")?;
    Ok(state_home.join("wroid").join("hub-sideload"))
}

fn default_profiles_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path).join("wroid").join("profiles-v2"));
    }
    if let Some(path) = env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path)
            .join(".config")
            .join("wroid")
            .join("profiles-v2"));
    }
    bail!("could not locate the user config directory; set XDG_CONFIG_HOME or HOME")
}

fn absolute_directory(path: PathBuf) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .context("failed to read the current directory")?
            .join(path)
    };
    fs::create_dir_all(&path)
        .with_context(|| format!("failed to create profile library {}", path.display()))?;
    path.canonicalize()
        .with_context(|| format!("failed to resolve profile library {}", path.display()))
}

fn bootstrap_library(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)
        .with_context(|| format!("failed to create profile library {}", directory.display()))?;
    for starter in STARTER_PROFILES {
        let path = directory.join(format!("{}.json", starter.id));
        let profile: ProfileV2 = serde_json::from_str(starter.source)
            .with_context(|| format!("embedded profile {} is invalid JSON", starter.id))?;
        if path.exists() {
            upgrade_untouched_starter(&path, &profile)?;
            continue;
        }
        profile
            .save_to_path(&path)
            .with_context(|| format!("failed to install starter profile {}", path.display()))?;
    }
    Ok(())
}

fn upgrade_untouched_starter(path: &Path, current: &ProfileV2) -> Result<()> {
    let Ok(installed) = ProfileV2::load_from_path(path) else {
        return Ok(());
    };
    if starter_predecessors(current).contains(&installed) {
        current
            .save_to_path(path)
            .with_context(|| format!("failed to upgrade starter profile {}", path.display()))?;
    }
    Ok(())
}

fn legacy_tap_starter(current: &ProfileV2) -> Option<ProfileV2> {
    let mut legacy = current.clone();
    let mut changed = false;
    for binding in &mut legacy.bindings {
        if let ActionV2::Hold { point } = &binding.action {
            binding.action = ActionV2::Tap { point: *point };
            changed = true;
        }
    }
    changed.then_some(legacy)
}

fn starter_predecessors(current: &ProfileV2) -> Vec<ProfileV2> {
    let added_bindings: &[&str] = match current.package_name.as_str() {
        "com.tencent.ig" => &["reload"],
        "com.axlebolt.standoff2" => &["aim_down_sights"],
        _ => &[],
    };
    let mut predecessors = Vec::new();
    if !added_bindings.is_empty() {
        let mut previous = current.clone();
        previous
            .bindings
            .retain(|binding| !added_bindings.contains(&binding.name.as_str()));
        if previous != *current {
            if let Some(legacy) = legacy_tap_starter(&previous) {
                predecessors.push(legacy);
            }
            predecessors.push(previous);
        }
    }
    if let Some(legacy) = legacy_tap_starter(current) {
        predecessors.push(legacy);
    }
    predecessors
}

fn library_profiles(directory: &Path) -> Result<(Vec<LibraryProfile>, Vec<String>)> {
    let mut paths = fs::read_dir(directory)
        .with_context(|| format!("failed to read profile library {}", directory.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .collect::<Vec<_>>();
    paths.sort();

    let mut profiles = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        let Some(id) = path.file_stem().and_then(OsStr::to_str) else {
            errors.push(format!("Skipped non-UTF-8 profile path {}", path.display()));
            continue;
        };
        match ProfileV2::load_from_path(&path) {
            Ok(profile) => match profile.validate() {
                Ok(()) => profiles.push(LibraryProfile {
                    id: id.to_owned(),
                    path,
                    profile,
                }),
                Err(error) => {
                    errors.push(format!("{}: {}", path.display(), error.errors.join("; ")))
                }
            },
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }

    profiles.sort_by_key(|profile| starter_order(&profile.profile.package_name));
    Ok((profiles, errors))
}

fn starter_order(package: &str) -> usize {
    family_for_package(package).map_or(4, |family| family.order)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct VariantSyncReport {
    created: Vec<String>,
    warnings: Vec<String>,
}

fn reconcile_installed_game_variants(
    directory: &Path,
    profiles: &[LibraryProfile],
    installed_packages: &[String],
) -> VariantSyncReport {
    let mut report = VariantSyncReport::default();
    let mut represented_packages = profiles
        .iter()
        .map(|profile| profile.profile.package_name.clone())
        .collect::<HashSet<_>>();

    for family in GAME_FAMILIES {
        let Some(canonical) = profiles
            .iter()
            .find(|profile| profile.profile.package_name == family.canonical_package)
        else {
            if family.variants.iter().any(|variant| {
                variant.package != family.canonical_package
                    && installed_packages
                        .iter()
                        .any(|package| package == variant.package)
            }) {
                report.warnings.push(bounded_variant_warning(&format!(
                    "{}: canonical controls profile {} is unavailable",
                    family.name, family.canonical_package
                )));
            }
            continue;
        };

        for variant in family.variants {
            if variant.package == family.canonical_package
                || represented_packages.contains(variant.package)
                || !installed_packages
                    .iter()
                    .any(|package| package == variant.package)
            {
                continue;
            }
            let mut derived = canonical.profile.clone();
            derived.name = variant.name.to_owned();
            derived.package_name = variant.package.to_owned();
            let destination = directory.join(format!("{}.json", variant.profile_id));
            match publish_profile_no_replace(&derived, &destination) {
                Ok(true) => {
                    report.created.push(variant.profile_id.to_owned());
                    represented_packages.insert(variant.package.to_owned());
                }
                Ok(false) => report.warnings.push(bounded_variant_warning(&format!(
                    "{}: kept existing profile id {}; choose another id or fix that file to adopt {}",
                    variant.name, variant.profile_id, variant.package
                ))),
                Err(error) => report.warnings.push(bounded_variant_warning(&format!(
                    "{}: could not create controls profile: {error:#}",
                    variant.name
                ))),
            }
        }
    }
    report
}

fn publish_profile_no_replace(profile: &ProfileV2, destination: &Path) -> Result<bool> {
    let directory = destination
        .parent()
        .context("derived profile destination has no parent")?;
    let file_name = destination
        .file_name()
        .and_then(OsStr::to_str)
        .context("derived profile destination is not valid UTF-8")?;
    let temporary = directory.join(format!(".{file_name}.{}.variant.tmp", local_token()?));
    profile
        .save_to_path(&temporary)
        .with_context(|| format!("failed to stage {}", destination.display()))?;
    let published = (|| {
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure {}", temporary.display()))?;
        match fs::hard_link(&temporary, destination) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("failed to publish {}", destination.display()))
            }
        }
    })();
    let _ = fs::remove_file(&temporary);
    published
}

fn bounded_variant_warning(message: &str) -> String {
    message.chars().take(512).collect()
}

fn build_state(directory: &Path) -> Result<Value> {
    let (mut profiles, mut library_errors) = library_profiles(directory)?;
    let waydroid_available = wroid_waydroid::is_available();
    let waydroid_status = waydroid_available
        .then(wroid_waydroid::status)
        .transpose()
        .map(|status| status.unwrap_or_default());
    let (status_text, running, status_error) = match waydroid_status {
        Ok(status) => {
            let running = status.contains("Session:") && status.contains("RUNNING");
            (status, running, None)
        }
        Err(error) => (String::new(), false, Some(error.to_string())),
    };
    let installed_packages = if running {
        wroid_waydroid::app_list_packages().ok()
    } else {
        None
    };
    if let Some(packages) = &installed_packages {
        let sync = reconcile_installed_game_variants(directory, &profiles, packages);
        library_errors.extend(sync.warnings);
        if !sync.created.is_empty() {
            let (reloaded, reload_errors) = library_profiles(directory)?;
            profiles = reloaded;
            library_errors.extend(reload_errors);
        }
    }

    let games = profiles
        .iter()
        .map(|entry| library_game_json(entry, installed_packages.as_deref()))
        .collect::<Vec<_>>();

    let keyboard =
        input_devices_json(discover_keyboard_devices().map_err(|error| error.to_string()));
    let mouse = input_devices_json(discover_mouse_devices().map_err(|error| error.to_string()));
    let graphics = GraphicsReport::probe();
    let compatibility = CompatibilityReport::probe_with(
        running,
        installed_packages.as_deref(),
        graphics.android.abi.clone(),
    );
    let input_bridge = input_bridge_json(
        super::launch_v2::active_game_session_state()
            .map_err(|error| io::Error::other(error.to_string())),
    );
    let last_game_session = last_game_session_json(
        super::launch_v2::last_game_session_state()
            .map_err(|error| io::Error::other(error.to_string())),
    );
    let bridge_helper = super::system_helper::readiness();
    let (preferences, preferences_error) = match preferences::load_default() {
        Ok(preferences) => (preferences, None),
        Err(error) => (UserPreferences::default(), Some(error.to_string())),
    };

    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "libraryPath": directory,
        "preferences": preferences,
        "preferencesError": preferences_error,
        "games": games,
        "libraryErrors": library_errors,
        "system": {
            "waydroid": {
                "available": waydroid_available,
                "running": running,
                "status": status_text,
                "error": status_error,
            },
            "keyboard": keyboard,
            "mouse": mouse,
            "inputBridge": input_bridge,
            "lastGameSession": last_game_session,
            "bridgeHelper": {
                "state": bridge_helper.state,
                "ready": bridge_helper.ready,
                "detail": bridge_helper.detail,
            },
            "focusProtection": focus_protection_json(),
            "graphics": graphics.as_json(),
            "compatibility": compatibility.as_json(),
            "storage": super::storage::storage_json(),
        },
    }))
}

fn calibration_json(profile_path: &Path) -> Value {
    match super::editor::calibration_background_state(profile_path) {
        super::editor::CalibrationBackgroundState::Ready => json!({
            "state": "ready",
            "ready": true,
            "detail": "Saved calibration reference available",
        }),
        super::editor::CalibrationBackgroundState::Missing => json!({
            "state": "needed",
            "ready": false,
            "detail": "Open the game and align this profile with its current HUD",
        }),
        super::editor::CalibrationBackgroundState::Invalid(error) => json!({
            "state": "invalid",
            "ready": false,
            "detail": error,
        }),
    }
}

fn input_bridge_json(result: io::Result<super::launch_v2::ActiveGameSessionState>) -> Value {
    match result {
        Ok(state) => json!({
            "busy": state.owner.is_some(),
            "owner": state.owner,
            "canStop": state.can_stop,
            "error": null,
        }),
        Err(error) => json!({
            "busy": false,
            "owner": null,
            "canStop": false,
            "error": error.to_string(),
        }),
    }
}

fn last_game_session_json(
    result: io::Result<Option<super::launch_v2::LastGameSessionState>>,
) -> Value {
    match result {
        Ok(Some(state)) => json!(state),
        Ok(None) => Value::Null,
        Err(error) => json!({
            "state": "unavailable",
            "detail": error.to_string(),
        }),
    }
}

fn focus_protection_json() -> Value {
    let desktop = env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let plasma_version = env::var("KDE_SESSION_VERSION").ok();
    let supported =
        desktop.to_ascii_lowercase().contains("kde") && plasma_version.as_deref() == Some("6");
    json!({
        "supported": supported,
        "mode": if supported { "automatic" } else { "fallback" },
        "detail": if supported {
            "KWin releases input when Waydroid loses focus. F12 also releases or reacquires keyboard and mouse manually."
        } else {
            "F12 releases or reacquires keyboard and mouse manually; Ctrl+Esc stops the session and cancels all touches."
        },
    })
}

fn display_name(name: &str) -> &str {
    name.split('—').next().unwrap_or(name).trim()
}

fn input_devices_json(result: std::result::Result<Vec<InputDeviceInfo>, String>) -> Value {
    match result {
        Ok(devices) => {
            let preferred = devices
                .first()
                .map(|device| device.path.display().to_string());
            json!({
                "ready": preferred.is_some(),
                "value": preferred,
                "devices": devices.iter().enumerate().map(|(index, device)| json!({
                    "path": device.path,
                    "name": device.name,
                    "preferred": index == 0,
                })).collect::<Vec<_>>(),
            })
        }
        Err(error) => json!({
            "ready": false,
            "error": error,
            "devices": [],
        }),
    }
}

fn library_game_json(entry: &LibraryProfile, installed_packages: Option<&[String]>) -> Value {
    let (taps, holds, joysticks, mouse_aim, layers) = control_counts(&entry.profile);
    let layer_metadata = entry
        .profile
        .layers
        .iter()
        .map(|layer| {
            let (mode, key) = match &layer.activation {
                LayerActivation::Hold { key } => ("hold", key),
                LayerActivation::Toggle { key } => ("toggle", key),
            };
            json!({
                "name": layer.name,
                "mode": mode,
                "key": key,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "id": entry.id,
        "name": display_name(&entry.profile.name),
        "package": entry.profile.package_name,
        "path": entry.path,
        "kind": game_kind(&entry.profile.package_name),
        "description": game_description(&entry.profile.package_name),
        "bindings": entry.profile.bindings.len(),
        "layers": layer_metadata,
        "controls": {
            "layers": layers,
            "taps": taps,
            "holds": holds,
            "joysticks": joysticks,
            "mouseAim": mouse_aim,
        },
        "calibration": calibration_json(&entry.path),
        "installed": installed_packages.map(|packages| {
            packages.iter().any(|package| package == &entry.profile.package_name)
        }),
    })
}

fn control_counts(profile: &ProfileV2) -> (usize, usize, usize, usize, usize) {
    let action_counts = profile
        .bindings
        .iter()
        .fold((0, 0, 0, 0), |mut counts, binding| {
            match binding.action {
                ActionV2::Tap { .. } => counts.0 += 1,
                ActionV2::Hold { .. } => counts.1 += 1,
                ActionV2::VirtualJoystick { .. } => counts.2 += 1,
                ActionV2::MouseAim { .. } => counts.3 += 1,
                ActionV2::Macro { .. } => {}
            }
            counts
        });
    (
        action_counts.0,
        action_counts.1,
        action_counts.2,
        action_counts.3,
        profile.layers.len(),
    )
}

fn game_kind(package: &str) -> &'static str {
    family_for_package(package).map_or("custom", |family| family.kind)
}

fn game_description(package: &str) -> &'static str {
    family_for_package(package).map_or("Custom Android game profile", |family| family.description)
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    method: String,
    target: String,
    body: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
struct RequestHead {
    method: String,
    target: String,
    content_length: Option<u64>,
    initial_body: Vec<u8>,
}

fn read_request_head(reader: &mut impl Read) -> Result<RequestHead> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end = loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            bail!("connection closed before request headers");
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(index) = find_bytes(&data, b"\r\n\r\n") {
            let header_end = index + 4;
            if header_end > MAX_HEADER_BYTES {
                bail!("request headers are too large");
            }
            break header_end;
        }
        if data.len() > MAX_HEADER_BYTES {
            bail!("request headers are too large");
        }
    };

    let header =
        std::str::from_utf8(&data[..header_end]).context("request headers are not valid UTF-8")?;
    let mut lines = header.split("\r\n");
    let request_line = lines.next().context("missing HTTP request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("missing HTTP method")?.to_owned();
    let target = parts.next().context("missing HTTP target")?.to_owned();
    let version = parts.next().context("missing HTTP version")?;
    if parts.next().is_some() {
        bail!("invalid HTTP request line");
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        bail!("unsupported HTTP version");
    }

    let mut content_length = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .context("malformed HTTP request header")?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                bail!("duplicate Content-Length is not supported");
            }
            content_length = Some(
                value
                    .trim()
                    .parse::<u64>()
                    .context("invalid Content-Length")?,
            );
        }
        if name.trim().eq_ignore_ascii_case("transfer-encoding") {
            bail!("Transfer-Encoding is not supported");
        }
    }

    Ok(RequestHead {
        method,
        target,
        content_length,
        initial_body: data[header_end..].to_vec(),
    })
}

fn read_bounded_body(
    head: RequestHead,
    reader: &mut impl Read,
    max_body_bytes: usize,
) -> Result<Request> {
    let content_length = head.content_length.unwrap_or(0);
    if content_length > max_body_bytes as u64 {
        bail!("request body is too large");
    }
    let content_length = content_length as usize;
    let mut body = head.initial_body;

    let mut buffer = [0_u8; 8192];
    while body.len() < content_length {
        let remaining = content_length - body.len();
        let read_limit = remaining.min(buffer.len());
        let read = reader.read(&mut buffer[..read_limit])?;
        if read == 0 {
            bail!("connection closed before request body");
        }
        body.extend_from_slice(&buffer[..read]);
    }
    body.truncate(content_length);

    Ok(Request {
        method: head.method,
        target: head.target,
        body,
    })
}

fn validate_apk_upload_length(content_length: Option<u64>) -> Result<u64> {
    let length = content_length.context("APK upload requires Content-Length")?;
    if length == 0 {
        bail!("APK upload is empty");
    }
    if length > MAX_APK_BYTES {
        bail!("APK upload is too large (maximum 4 GiB)");
    }
    Ok(length)
}

fn is_valid_sideload_ticket(ticket: &str) -> bool {
    ticket.len() == 48
        && ticket
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn secure_sideload_directory(directory: &Path) -> Result<PathBuf> {
    fs::create_dir_all(directory).with_context(|| {
        format!(
            "failed to create sideload directory {}",
            directory.display()
        )
    })?;
    let metadata = fs::symlink_metadata(directory).with_context(|| {
        format!(
            "failed to inspect sideload directory {}",
            directory.display()
        )
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "sideload path is not a private directory: {}",
            directory.display()
        );
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "failed to secure sideload directory {}",
            directory.display()
        )
    })?;
    directory.canonicalize().with_context(|| {
        format!(
            "failed to resolve sideload directory {}",
            directory.display()
        )
    })
}

fn cleanup_stale_sideload(directory: &Path, now: SystemTime) -> Result<()> {
    let directory = secure_sideload_directory(directory)?;
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        if !is_known_sideload_filename(&entry.file_name()) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        if now.duration_since(modified).unwrap_or_default() < SIDELOAD_RETENTION {
            continue;
        }
        fs::remove_file(entry.path()).with_context(|| {
            format!(
                "failed to remove stale sideload artifact {}",
                entry.path().display()
            )
        })?;
    }
    Ok(())
}

fn is_known_sideload_filename(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if [".apk", ".json", ".lock", ".upload"].iter().any(|suffix| {
        name.strip_suffix(suffix)
            .is_some_and(is_valid_sideload_ticket)
    }) {
        return true;
    }
    let Some(temporary) = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((ticket, random)) = temporary.split_once('.') else {
        return false;
    };
    is_valid_sideload_ticket(ticket) && is_valid_sideload_ticket(random)
}

fn sideload_artifact_path(directory: &Path, ticket: &str) -> Result<PathBuf> {
    if !is_valid_sideload_ticket(ticket) {
        bail!("invalid sideload ticket");
    }
    Ok(directory.join(format!("{ticket}.apk")))
}

fn sideload_status_path(directory: &Path, ticket: &str) -> Result<PathBuf> {
    if !is_valid_sideload_ticket(ticket) {
        bail!("invalid sideload ticket");
    }
    Ok(directory.join(format!("{ticket}.json")))
}

fn sideload_lock_path(directory: &Path, ticket: &str) -> Result<PathBuf> {
    if !is_valid_sideload_ticket(ticket) {
        bail!("invalid sideload ticket");
    }
    Ok(directory.join(format!("{ticket}.lock")))
}

fn stage_apk_upload(
    head: RequestHead,
    reader: &mut impl Read,
    directory: &Path,
    ticket: &str,
) -> Result<PathBuf> {
    let length = validate_apk_upload_length(head.content_length)?;
    if !is_valid_sideload_ticket(ticket) {
        bail!("invalid sideload ticket");
    }
    let directory = secure_sideload_directory(directory)?;
    let temporary = directory.join(format!("{ticket}.upload"));
    let artifact = sideload_artifact_path(&directory, ticket)?;
    let result: Result<PathBuf> = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)
            .with_context(|| format!("failed to create APK upload {}", temporary.display()))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;

        let initial_bytes = (head.initial_body.len() as u64).min(length) as usize;
        file.write_all(&head.initial_body[..initial_bytes])?;
        let mut written = initial_bytes as u64;
        let mut buffer = [0_u8; 64 * 1024];
        while written < length {
            let remaining = (length - written).min(buffer.len() as u64) as usize;
            let read = reader
                .read(&mut buffer[..remaining])
                .context("failed while reading APK upload")?;
            if read == 0 {
                bail!("connection closed before APK upload completed");
            }
            file.write_all(&buffer[..read])?;
            written += read as u64;
        }
        file.flush()?;
        file.sync_all()?;
        fs::hard_link(&temporary, &artifact)
            .with_context(|| format!("failed to publish APK upload {}", artifact.display()))?;
        fs::remove_file(&temporary)?;
        Ok(artifact.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn package_intake_json(ticket: &str, preflight: &super::app::PackagePreflight) -> Value {
    json!({
        "ok": true,
        "ticket": ticket,
        "state": "inspected",
        "installable": true,
        "artifact": {
            "format": preflight.artifact.format,
            "formatLabel": preflight.artifact.format.label(),
            "fileSize": preflight.artifact.file_size,
            "archiveEntries": preflight.artifact.archive_entries,
            "hasAndroidManifest": preflight.artifact.has_android_manifest,
            "hasDex": preflight.artifact.has_dex,
            "hasResources": preflight.artifact.has_resources,
            "nativeAbis": preflight.artifact.native_abis,
            "embeddedApks": preflight.artifact.embedded_apks,
            "obbFiles": preflight.artifact.obb_files,
        },
        "compatibility": {
            "state": preflight.abi_compatibility,
            "androidAbis": preflight.android_abis,
            "armTranslation": preflight.arm_translation,
        },
    })
}

fn handle_apk_upload_with_ticket(
    head: RequestHead,
    reader: &mut impl Read,
    directory: &Path,
    ticket: &str,
) -> Response {
    let artifact = match stage_apk_upload(head, reader, directory, ticket) {
        Ok(path) => path,
        Err(error) => return Response::json(400, &json_error(&error.to_string())),
    };
    let result = super::app::package_preflight(&artifact).and_then(|preflight| {
        super::app::validate_install_preflight(&preflight, false)?;
        Ok(package_intake_json(ticket, &preflight))
    });
    match result {
        Ok(intake) => {
            if let Err(error) = write_apk_install_status(
                directory,
                ticket,
                "inspected",
                "Package inspected and ready to install",
            ) {
                let _ = fs::remove_file(&artifact);
                return Response::json(500, &json_error(&error.to_string()));
            }
            Response::json(201, &intake.to_string())
        }
        Err(error) => {
            let _ = fs::remove_file(&artifact);
            Response::json(422, &json_error(&error.to_string()))
        }
    }
}

fn ticket_from_json(body: &[u8]) -> Result<String> {
    let payload: Value = serde_json::from_slice(body).context("invalid JSON")?;
    let ticket = payload
        .get("ticket")
        .and_then(Value::as_str)
        .context("missing sideload ticket")?;
    if !is_valid_sideload_ticket(ticket) {
        bail!("invalid sideload ticket");
    }
    Ok(ticket.to_owned())
}

fn handle_apk_discard(body: &[u8], directory: &Path) -> Response {
    let result: Result<String> = (|| {
        let ticket = ticket_from_json(body)?;
        let directory = secure_sideload_directory(directory)?;
        let lock_path = sideload_lock_path(&directory, &ticket)?;
        let lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)
            .with_context(|| format!("APK sideload ticket {ticket} is already installing"))?;
        lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        let discard_result: Result<()> = (|| {
            let status = read_apk_install_status(&directory, &ticket)?;
            let state = status
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(state, "queued" | "installing") {
                bail!("APK installation is already in progress");
            }
            if state != "inspected" {
                bail!("APK sideload ticket is no longer discardable");
            }
            let artifact = sideload_artifact_path(&directory, &ticket)?;
            let status_path = sideload_status_path(&directory, &ticket)?;
            fs::remove_file(&artifact)
                .with_context(|| format!("APK sideload ticket {ticket} was not found"))?;
            fs::remove_file(status_path)?;
            Ok(())
        })();
        let unlock_result = fs::remove_file(lock_path).context("failed to release discard lock");
        discard_result?;
        unlock_result?;
        Ok(ticket)
    })();
    match result {
        Ok(ticket) => Response::json(
            200,
            &json!({ "ok": true, "ticket": ticket, "message": "Staged APK discarded" }).to_string(),
        ),
        Err(error) => Response::json(422, &json_error(&error.to_string())),
    }
}

fn handle_apk_status(target: &str, directory: &Path) -> Response {
    let result = (|| {
        let ticket = query_parameter(target, "ticket").context("missing sideload ticket")?;
        if !is_valid_sideload_ticket(ticket) {
            bail!("invalid sideload ticket");
        }
        read_apk_install_status(directory, ticket)
    })();
    match result {
        Ok(status) => Response::json(200, &status.to_string()),
        Err(error) => Response::json(404, &json_error(&error.to_string())),
    }
}

fn claim_apk_install(directory: &Path, ticket: &str) -> Result<()> {
    let directory = secure_sideload_directory(directory)?;
    let lock_path = sideload_lock_path(&directory, ticket)?;
    let lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .with_context(|| format!("APK sideload ticket {ticket} is already installing"))?;
    lock.set_permissions(fs::Permissions::from_mode(0o600))?;

    let result = (|| {
        let artifact = sideload_artifact_path(&directory, ticket)?;
        let metadata = fs::symlink_metadata(&artifact)
            .with_context(|| format!("APK sideload ticket {ticket} was not found"))?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            bail!("staged APK is not a regular private file");
        }
        let status = read_apk_install_status(&directory, ticket)?;
        if status.get("state").and_then(Value::as_str) != Some("inspected") {
            bail!("APK sideload ticket is not ready to install");
        }
        write_apk_install_status(&directory, ticket, "queued", "Install worker queued")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(lock_path);
    }
    result
}

fn queue_apk_install_with<F>(directory: &Path, ticket: &str, launcher: F) -> Result<()>
where
    F: FnOnce(&[OsString]) -> Result<()>,
{
    claim_apk_install(directory, ticket)?;
    let arguments = apk_install_worker_arguments(ticket);
    if let Err(error) = launcher(&arguments) {
        let directory = secure_sideload_directory(directory)?;
        let lock = sideload_lock_path(&directory, ticket)?;
        let _ = fs::remove_file(lock);
        write_apk_install_status(
            &directory,
            ticket,
            "inspected",
            "Install worker could not start; package remains ready",
        )?;
        return Err(error);
    }
    Ok(())
}

fn start_apk_install(directory: &Path, ticket: &str) -> Result<String> {
    let executable = env::current_exe().context("failed to locate the wroid executable")?;
    queue_apk_install_with(directory, ticket, |arguments| {
        let mut command = Command::new(&executable);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        let mut child = command
            .spawn()
            .context("failed to start detached APK install worker")?;
        thread::spawn(move || {
            if let Err(error) = child.wait() {
                eprintln!("Warning: could not reap APK install worker: {error}");
            }
        });
        Ok(())
    })?;
    Ok("APK install queued; Waydroid will start automatically".to_owned())
}

fn handle_apk_install(body: &[u8], directory: &Path) -> Response {
    let result = ticket_from_json(body)
        .and_then(|ticket| start_apk_install(directory, &ticket).map(|message| (ticket, message)));
    match result {
        Ok((ticket, message)) => Response::json(
            202,
            &json!({ "ok": true, "ticket": ticket, "state": "queued", "message": message })
                .to_string(),
        ),
        Err(error) => Response::json(422, &json_error(&error.to_string())),
    }
}

fn bounded_apk_status_detail(detail: &str) -> String {
    if detail.len() <= APK_STATUS_DETAIL_BYTES {
        return detail.to_owned();
    }
    let mut end = APK_STATUS_DETAIL_BYTES;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail[..end].to_owned()
}

fn write_apk_install_status(
    directory: &Path,
    ticket: &str,
    state: &str,
    detail: &str,
) -> Result<Value> {
    if !matches!(
        state,
        "inspected" | "queued" | "installing" | "installed" | "failed"
    ) {
        bail!("invalid APK install state");
    }
    let directory = secure_sideload_directory(directory)?;
    let path = sideload_status_path(&directory, ticket)?;
    let status = json!({
        "ok": state != "failed",
        "ticket": ticket,
        "state": state,
        "detail": bounded_apk_status_detail(detail),
        "updatedAtUnixMs": unix_time_millis(),
    });
    let encoded = serde_json::to_vec(&status)?;
    if encoded.len() as u64 > APK_STATUS_BYTES {
        bail!("APK install status is too large");
    }
    let random = local_token()?;
    let temporary = directory.join(format!(".{ticket}.{random}.tmp"));
    let result: Result<Value> = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(&encoded)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(status.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("failed to write APK install status {}", path.display()))
}

fn read_apk_install_status(directory: &Path, ticket: &str) -> Result<Value> {
    let directory = secure_sideload_directory(directory)?;
    let path = sideload_status_path(&directory, ticket)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("APK sideload ticket {ticket} was not found"))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > APK_STATUS_BYTES {
        bail!("APK install status is invalid");
    }
    let mut encoded = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut encoded)?;
    let status: Value = serde_json::from_slice(&encoded).context("invalid APK install status")?;
    if status.get("ticket").and_then(Value::as_str) != Some(ticket) {
        bail!("APK install status ticket does not match");
    }
    Ok(status)
}

fn run_apk_install_worker_with<F>(directory: &Path, ticket: &str, installer: F) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let directory = secure_sideload_directory(directory)?;
    let artifact = sideload_artifact_path(&directory, ticket)?;
    let metadata = fs::symlink_metadata(&artifact)
        .with_context(|| format!("APK sideload ticket {ticket} was not found"))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("staged APK is not a regular private file");
    }
    write_apk_install_status(&directory, ticket, "installing", "Installing into Waydroid")?;

    let result = (|| {
        let preflight = super::app::package_preflight(&artifact)?;
        super::app::validate_install_preflight(&preflight, false)?;
        installer(&artifact)
    })();
    let _ = fs::remove_file(&artifact);
    if let Ok(lock) = sideload_lock_path(&directory, ticket) {
        let _ = fs::remove_file(lock);
    }

    match result {
        Ok(()) => {
            write_apk_install_status(
                &directory,
                ticket,
                "installed",
                "Installed APK into Waydroid",
            )?;
            Ok(())
        }
        Err(error) => {
            let detail = format!("{error:#}");
            write_apk_install_status(&directory, ticket, "failed", &detail)?;
            Err(error)
        }
    }
}

fn apk_install_worker_arguments(ticket: &str) -> Vec<OsString> {
    vec![
        OsString::from("install-apk-worker"),
        OsString::from("--ticket"),
        OsString::from(ticket),
    ]
}

pub(crate) fn install_apk_worker(input_executor: &impl InputExecutor, ticket: &str) -> Result<()> {
    let directory = default_sideload_directory()?;
    run_apk_install_worker_with(&directory, ticket, |artifact| {
        let _lease = acquire_desktop_action_guard("installing a local APK from Wroid Hub")?;
        let mut control = SystemDesktopWaydroidControl::default();
        ensure_desktop_waydroid_ready(
            &mut control,
            DESKTOP_READY_ATTEMPTS,
            DESKTOP_READY_INTERVAL,
        )?;
        input_executor
            .waydroid_app_install(artifact)
            .with_context(|| format!("failed to install APK {} into Waydroid", artifact.display()))
    })
}

fn handle_request(
    request: &Request,
    directory: &Path,
    sideload_directory: &Path,
    token: &str,
) -> (Response, bool) {
    let route = request_route(&request.target);
    let authorized = target_is_authorized(&request.target, token);

    match (request.method.as_str(), route) {
        ("GET", "/styles.css") => (Response::css(STYLES_CSS), false),
        ("GET", "/control-chips.js") => (Response::javascript(CONTROL_CHIPS_JS), false),
        ("GET", "/compatibility-state.js") => (Response::javascript(COMPATIBILITY_STATE_JS), false),
        ("GET", "/app.js") => (Response::javascript(APP_JS), false),
        ("GET", "/") if authorized => (Response::html(INDEX_HTML), false),
        ("GET", "/api/state") if authorized => match build_state(directory) {
            Ok(state) => (Response::json(200, &state.to_string()), false),
            Err(error) => (Response::json(500, &json_error(&error.to_string())), false),
        },
        ("GET", "/api/apk/status") if authorized => (
            handle_apk_status(&request.target, sideload_directory),
            false,
        ),
        ("POST", "/api/apk/install") if authorized => {
            (handle_apk_install(&request.body, sideload_directory), false)
        }
        ("POST", "/api/apk/discard") if authorized => {
            (handle_apk_discard(&request.body, sideload_directory), false)
        }
        ("PUT", "/api/preferences") if authorized => {
            match preferences::update_default(&request.body) {
                Ok(preferences) => (
                    Response::json(
                        200,
                        &json!({
                            "ok": true,
                            "preferences": preferences,
                            "message": "Preferences saved",
                        })
                        .to_string(),
                    ),
                    false,
                ),
                Err(error) => (Response::json(422, &json_error(&error.to_string())), false),
            }
        }
        ("POST", "/api/action") if authorized => (handle_action(&request.body, directory), false),
        ("POST", "/api/import") if authorized => {
            (handle_profile_import(&request.body, directory), false)
        }
        ("POST", "/api/close") if authorized => (
            Response::json(200, r#"{"ok":true,"message":"Gaming hub closed"}"#),
            true,
        ),
        (_, _) if !authorized => (
            Response::json(403, r#"{"ok":false,"error":"Invalid hub token"}"#),
            false,
        ),
        _ => (
            Response::json(404, r#"{"ok":false,"error":"Not found"}"#),
            false,
        ),
    }
}

fn request_route(target: &str) -> &str {
    target.split_once('?').map_or(target, |(route, _)| route)
}

fn target_is_authorized(target: &str, token: &str) -> bool {
    query_parameter(target, "token") == Some(token)
}

fn query_parameter<'a>(target: &'a str, expected: &str) -> Option<&'a str> {
    target
        .split_once('?')
        .map_or("", |(_, query)| query)
        .split('&')
        .filter_map(|part| part.split_once('='))
        .find_map(|(name, value)| (name == expected).then_some(value))
}

fn handle_action(body: &[u8], directory: &Path) -> Response {
    let payload: Value = match serde_json::from_slice(body) {
        Ok(payload) => payload,
        Err(error) => return Response::json(400, &json_error(&format!("invalid JSON: {error}"))),
    };
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let result = match action {
        "edit" => profile_from_payload(&payload, directory).and_then(open_profile_editor),
        "calibrate" => open_game_for_calibration(&payload, directory),
        "launch" => profile_from_payload(&payload, directory).and_then(|profile| {
            ensure_input_bridge_available()?;
            super::system_helper::ensure_ready()?;
            let (width, height) = launch_resolution(&payload)?;
            let game_mode = launch_game_mode(&payload)?;
            GraphicsReport::probe().ensure_launch_ready()?;
            CompatibilityReport::probe()
                .ensure_known_game_launch_ready(&profile.profile.package_name)?;
            let (keyboard, mouse) = selected_profile_input_devices(&payload, &profile.profile)?;
            open_game_background(
                &profile.path,
                &profile.profile,
                width,
                height,
                keyboard.as_deref(),
                mouse.as_deref(),
                game_mode,
            )
        }),
        "input-test" => profile_from_payload(&payload, directory).and_then(|profile| {
            ensure_input_bridge_available()?;
            super::system_helper::ensure_ready()?;
            let (width, height) = launch_resolution(&payload)?;
            GraphicsReport::probe().ensure_launch_ready()?;
            let (keyboard, mouse) = selected_profile_input_devices(&payload, &profile.profile)?;
            open_input_self_test_terminal(
                &profile.path,
                width,
                height,
                keyboard.as_deref(),
                mouse.as_deref(),
            )
        }),
        "stop" => stop_game_background(),
        "store" => open_store(&payload, directory),
        "graphics-setup" => super::graphics::open_gpu_setup(),
        "compatibility-setup" => compatibility::open_setup(),
        "helper-setup" => open_system_helper_setup(),
        "show-waydroid" => open_desktop_waydroid(),
        _ => bail_result("unsupported hub action"),
    };

    match result {
        Ok(message) => Response::json(202, &json!({ "ok": true, "message": message }).to_string()),
        Err(error) => Response::json(422, &json_error(&error.to_string())),
    }
}

fn open_system_helper_setup() -> Result<String> {
    if super::system_helper::graphical_install_supported() {
        return super::system_helper::start_graphical_install();
    }
    let executable = env::current_exe().context("failed to locate the wroid executable")?;
    let command = system_helper_setup_command(&executable);
    let terminal = spawn_terminal(&command)?;
    Ok(format!(
        "Graphical authorization is unavailable; helper setup opened in {terminal}"
    ))
}

fn system_helper_setup_command(executable: &Path) -> Vec<OsString> {
    vec![
        executable.as_os_str().to_owned(),
        OsString::from("helper"),
        OsString::from("install"),
    ]
}

fn open_game_for_calibration(payload: &Value, directory: &Path) -> Result<String> {
    let profile = profile_from_payload(payload, directory)?;
    let title = display_name(&profile.profile.name).to_owned();
    let package = profile.profile.package_name.clone();
    let _lease = acquire_desktop_action_guard(&format!("calibrating controls for {title}"))?;
    let mut control = SystemDesktopWaydroidControl::default();
    let started = ensure_desktop_waydroid_ready(
        &mut control,
        DESKTOP_READY_ATTEMPTS,
        DESKTOP_READY_INTERVAL,
    )?;
    wait_for_package_available_for_calibration(
        &package,
        PACKAGE_DISCOVERY_ATTEMPTS,
        DESKTOP_READY_INTERVAL,
        || {
            wroid_waydroid::app_list_packages()
                .context("failed to verify the game package before calibration")
        },
    )?;
    wroid_waydroid::app_launch_package(&package)
        .with_context(|| format!("failed to open {title} for calibration"))?;
    open_profile_editor(profile)?;
    Ok(desktop_action_message(
        &format!("Opened {title} and Controls Studio for live alignment"),
        started,
    ))
}

fn ensure_package_available_for_calibration(package: &str, packages: &[String]) -> Result<()> {
    if packages.iter().any(|installed| installed == package) {
        return Ok(());
    }
    bail!("game package {package} is not installed; install it from Google Play first")
}

fn wait_for_package_available_for_calibration<F>(
    package: &str,
    attempts: usize,
    interval: Duration,
    mut list_packages: F,
) -> Result<()>
where
    F: FnMut() -> Result<Vec<String>>,
{
    let mut last_error = None;
    for attempt in 0..attempts {
        match list_packages() {
            Ok(packages) => {
                if ensure_package_available_for_calibration(package, &packages).is_ok() {
                    return Ok(());
                }
                last_error = None;
            }
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < attempts {
            thread::sleep(interval);
        }
    }
    if let Some(error) = last_error {
        return Err(error);
    }
    bail!(
        "game package {package} did not become visible after Android startup; refresh the Hub and try calibration again"
    )
}

fn ensure_input_bridge_available() -> Result<()> {
    if let Some(owner) = super::launch_v2::active_game_session_owner()? {
        bail!(
            "another Wroid game session is already active ({owner}); stop it with Ctrl+Esc before launching another game"
        );
    }
    Ok(())
}

fn open_store(payload: &Value, directory: &Path) -> Result<String> {
    let _lease = acquire_desktop_action_guard("opening Google Play from the Hub")?;
    let mut control = SystemDesktopWaydroidControl::default();
    let started = ensure_desktop_waydroid_ready(
        &mut control,
        DESKTOP_READY_ATTEMPTS,
        DESKTOP_READY_INTERVAL,
    )?;
    let Some(id) = payload.get("id").and_then(Value::as_str) else {
        wroid_waydroid::app_launch_package("com.android.vending")?;
        return Ok(desktop_action_message("Opened Google Play Store", started));
    };
    let profile = profile_from_payload(&json!({ "id": id }), directory)?;
    let uri = format!("market://details?id={}", profile.profile.package_name);
    wroid_waydroid::app_open_uri("android.intent.action.VIEW", &uri)?;
    Ok(desktop_action_message(
        &format!(
            "Opened {} in Google Play",
            display_name(&profile.profile.name)
        ),
        started,
    ))
}

fn open_desktop_waydroid() -> Result<String> {
    let _lease = acquire_desktop_action_guard("opening desktop Waydroid from the Hub")?;
    let mut control = SystemDesktopWaydroidControl::default();
    let started = ensure_desktop_waydroid_ready(
        &mut control,
        DESKTOP_READY_ATTEMPTS,
        DESKTOP_READY_INTERVAL,
    )?;
    wroid_waydroid::show_full_ui()?;
    Ok(desktop_action_message("Opened Waydroid UI", started))
}

fn acquire_desktop_action_guard(owner: &str) -> Result<wroid_inject::WaydroidBridgeLease> {
    let lease = super::launch_v2::acquire_desktop_action_lease(owner)?;
    if let Some(owner) = wroid_inject::active_default_bridge_lease_owner()
        .context("failed to inspect the privileged Wroid input bridge lease")?
    {
        bail!(
            "another Wroid game session is already active ({owner}); stop it with Ctrl+Esc before opening desktop Waydroid"
        );
    }
    Ok(lease)
}

fn desktop_action_message(action: &str, started: bool) -> String {
    if started {
        format!("Started desktop Waydroid. {action}.")
    } else {
        action.to_owned()
    }
}

trait DesktopWaydroidControl {
    fn status(&mut self) -> Result<String>;
    fn start(&mut self) -> Result<()>;
    fn package_manager_ready(&mut self) -> Result<()>;
    fn start_exit(&mut self) -> Result<Option<String>>;
}

#[derive(Default)]
struct SystemDesktopWaydroidControl {
    child: Option<std::process::Child>,
}

impl DesktopWaydroidControl for SystemDesktopWaydroidControl {
    fn status(&mut self) -> Result<String> {
        wroid_waydroid::status()
    }

    fn start(&mut self) -> Result<()> {
        let mut command = Command::new("waydroid");
        command
            .args(["session", "start"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        self.child = Some(
            command
                .spawn()
                .context("failed to start the desktop Waydroid session")?,
        );
        Ok(())
    }

    fn package_manager_ready(&mut self) -> Result<()> {
        wroid_waydroid::app_list_packages().map(|_| ())
    }

    fn start_exit(&mut self) -> Result<Option<String>> {
        self.child
            .as_mut()
            .map(|child| {
                child
                    .try_wait()
                    .context("failed to monitor desktop Waydroid startup")
                    .map(|status| status.map(|status| status.to_string()))
            })
            .transpose()
            .map(Option::flatten)
    }
}

fn ensure_desktop_waydroid_ready<C: DesktopWaydroidControl>(
    control: &mut C,
    attempts: usize,
    interval: Duration,
) -> Result<bool> {
    let initial_status = control
        .status()
        .context("failed to inspect Waydroid status")?;
    let started = !session_is_running(&initial_status);
    if started {
        control.start()?;
    }

    let mut last_status = initial_status;
    let mut last_readiness_error = None;
    for _ in 0..attempts {
        last_status = control
            .status()
            .context("failed to inspect Waydroid startup")?;
        if session_is_running(&last_status) {
            match control.package_manager_ready() {
                Ok(()) => return Ok(started),
                Err(error) => last_readiness_error = Some(error.to_string()),
            }
        } else if let Some(status) = control.start_exit()? {
            bail!("waydroid session start exited with {status}\n{last_status}");
        }
        thread::sleep(interval);
    }

    let readiness = last_readiness_error
        .map(|error| format!("\nAndroid package manager: {error}"))
        .unwrap_or_default();
    bail!(
        "desktop Waydroid did not become ready after {} seconds\n{last_status}{readiness}",
        interval.as_secs_f64() * attempts as f64
    )
}

fn session_is_running(status: &str) -> bool {
    status.lines().any(|line| {
        line.split_once(':')
            .is_some_and(|(name, value)| name.trim() == "Session" && value.trim() == "RUNNING")
    })
}

fn profile_from_payload(payload: &Value, directory: &Path) -> Result<LibraryProfile> {
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .context("missing game profile id")?;
    if id.is_empty()
        || !id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
    {
        bail!("invalid game profile id");
    }
    let (profiles, _) = library_profiles(directory)?;
    profiles
        .into_iter()
        .find(|profile| profile.id == id)
        .with_context(|| format!("game profile {id} was not found"))
}

fn launch_resolution(payload: &Value) -> Result<(u32, u32)> {
    let width = payload.get("width").and_then(Value::as_u64).unwrap_or(1600) as u32;
    let height = payload.get("height").and_then(Value::as_u64).unwrap_or(900) as u32;
    if !matches!((width, height), (1280, 720) | (1600, 900) | (1920, 1080)) {
        bail!("unsupported performance preset");
    }
    Ok((width, height))
}

fn launch_game_mode(payload: &Value) -> Result<bool> {
    match payload.get("gameMode") {
        Some(value) => value.as_bool().context("gameMode must be a boolean"),
        None => Ok(true),
    }
}

fn selected_input_device(
    payload: &Value,
    field: &str,
    devices: Result<Vec<InputDeviceInfo>>,
) -> Result<Option<PathBuf>> {
    let Some(selected) = payload.get(field).and_then(Value::as_str) else {
        return Ok(None);
    };
    let devices = devices.with_context(|| format!("failed to discover {field} devices"))?;
    devices
        .into_iter()
        .find(|device| device.path == Path::new(selected))
        .map(|device| Some(device.path))
        .with_context(|| format!("selected {field} is not an available input device"))
}

fn selected_profile_input_devices(
    payload: &Value,
    profile: &ProfileV2,
) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    let keyboard = selected_input_device(
        payload,
        "keyboard",
        discover_keyboard_devices().map_err(anyhow::Error::from),
    )?;
    let mouse = if profile_needs_mouse(profile) {
        selected_input_device(
            payload,
            "mouse",
            discover_mouse_devices().map_err(anyhow::Error::from),
        )?
    } else {
        None
    };
    Ok((keyboard, mouse))
}

fn profile_needs_mouse(profile: &ProfileV2) -> bool {
    profile.bindings.iter().any(|binding| {
        matches!(
            binding.input,
            wroid_core::profile_v2::InputV2::MouseButton { .. }
                | wroid_core::profile_v2::InputV2::MouseMove
        )
    })
}

fn open_profile_editor(profile: LibraryProfile) -> Result<String> {
    let executable = env::current_exe().context("failed to locate the wroid executable")?;
    let mut command = Command::new(executable);
    command
        .args(["profile", "edit-v2"])
        .arg(&profile.path)
        .stdin(Stdio::null());
    // Keep the standard streams attached: the editor's WebKit console
    // (window-capture failures, portal errors) is its only diagnostic
    // surface in release builds, and nulling it made capture issues
    // impossible to investigate.
    command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    command
        .spawn()
        .with_context(|| format!("failed to open editor for {}", profile.profile.name))?;
    Ok(format!("Opening controls for {}", profile.profile.name))
}

fn open_game_background(
    profile_path: &Path,
    profile: &ProfileV2,
    width: u32,
    height: u32,
    keyboard: Option<&Path>,
    mouse: Option<&Path>,
    game_mode: bool,
) -> Result<String> {
    super::runtime_daemon::launch_game(
        profile_path,
        profile,
        width,
        height,
        keyboard,
        mouse,
        game_mode,
    )
}

fn stop_game_background() -> Result<String> {
    match super::runtime_daemon::stop_game()? {
        Some(message) => Ok(message),
        None => super::launch_v2::stop_active_game_session(),
    }
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn open_input_self_test_terminal(
    profile_path: &Path,
    width: u32,
    height: u32,
    keyboard: Option<&Path>,
    mouse: Option<&Path>,
) -> Result<String> {
    let executable = env::current_exe().context("failed to locate the wroid executable")?;
    let command =
        input_self_test_command(&executable, profile_path, width, height, keyboard, mouse);
    let terminal = spawn_terminal(&command)?;
    Ok(format!(
        "{INPUT_SELF_TEST_SECONDS}s input self-test opened in {terminal}; focus Waydroid and exercise the selected map"
    ))
}

fn input_self_test_command(
    executable: &Path,
    profile_path: &Path,
    width: u32,
    height: u32,
    keyboard: Option<&Path>,
    mouse: Option<&Path>,
) -> Vec<OsString> {
    let mut command = vec![
        executable.as_os_str().to_owned(),
        OsString::from("launch-v2"),
        profile_path.as_os_str().to_owned(),
        OsString::from("--width"),
        OsString::from(width.to_string()),
        OsString::from("--height"),
        OsString::from(height.to_string()),
        OsString::from("--no-launch"),
        OsString::from("--trace-input"),
        OsString::from("--exit-after-seconds"),
        OsString::from(INPUT_SELF_TEST_SECONDS.to_string()),
    ];
    if let Some(keyboard) = keyboard {
        command.push(OsString::from("--keyboard"));
        command.push(keyboard.as_os_str().to_owned());
    }
    if let Some(mouse) = mouse {
        command.push(OsString::from("--mouse"));
        command.push(mouse.as_os_str().to_owned());
    }
    command
}

fn handle_profile_import(body: &[u8], directory: &Path) -> Response {
    let profile: ProfileV2 = match serde_json::from_slice(body) {
        Ok(profile) => profile,
        Err(error) => return Response::json(422, &json_error(&format!("invalid JSON: {error}"))),
    };
    if let Err(error) = profile.validate() {
        return Response::json(
            422,
            &json!({ "ok": false, "errors": error.errors }).to_string(),
        );
    }

    let id = profile_id(&profile.package_name);
    let path = directory.join(format!("{id}.json"));
    if path.exists() {
        return Response::json(
            409,
            &json_error(&format!("profile {id} already exists in the library")),
        );
    }
    match profile.save_to_path(&path) {
        Ok(()) => Response::json(
            201,
            &json!({ "ok": true, "id": id, "message": "Profile imported" }).to_string(),
        ),
        Err(error) => Response::json(500, &json_error(&error.to_string())),
    }
}

fn profile_id(package: &str) -> String {
    let mut result = String::new();
    let mut previous_dash = false;
    for character in package.trim().to_ascii_lowercase().chars() {
        let mapped = if character.is_ascii_alphanumeric() || matches!(character, '.' | '_') {
            previous_dash = false;
            Some(character)
        } else if !previous_dash {
            previous_dash = true;
            Some('-')
        } else {
            None
        };
        if let Some(character) = mapped {
            result.push(character);
        }
    }
    let result = result.trim_matches('-');
    if result.is_empty() {
        "custom-game".to_owned()
    } else {
        result.to_owned()
    }
}

fn bail_result<T>(message: &str) -> Result<T> {
    bail!("{message}")
}

fn open_url(url: &str) {
    if let Err(error) = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        eprintln!("Warning: could not open a browser: {error}");
    }
}

fn local_token() -> Result<String> {
    let mut bytes = [0_u8; 24];
    fs::File::open("/dev/urandom")
        .context("failed to open system random source")?
        .read_exact(&mut bytes)
        .context("failed to generate hub access token")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn effective_uid_from_proc() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let uid_line = status.lines().find(|line| line.starts_with("Uid:"))?;
    uid_line.split_whitespace().nth(2)?.parse().ok()
}

fn json_error(message: &str) -> String {
    json!({ "ok": false, "error": message }).to_string()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn html(body: &str) -> Self {
        Self::new(200, "text/html; charset=utf-8", body)
    }

    fn css(body: &str) -> Self {
        Self::new(200, "text/css; charset=utf-8", body)
    }

    fn javascript(body: &str) -> Self {
        Self::new(200, "text/javascript; charset=utf-8", body)
    }

    fn json(status: u16, body: &str) -> Self {
        Self::new(status, "application/json; charset=utf-8", body)
    }

    fn new(status: u16, content_type: &'static str, body: &str) -> Self {
        Self {
            status,
            content_type,
            body: body.as_bytes().to_vec(),
        }
    }
}

fn write_response(stream: &mut TcpStream, response: Response) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Content",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; connect-src 'self'\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len(),
    )?;
    stream.write_all(&response.body)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::net::Shutdown;

    use super::*;
    use wroid_android::{AbiCompatibility, PackageFormat, PackageInspection};
    use wroid_core::profile_v2::{LayerActivation, LayerV2};

    #[test]
    fn started_hub_serves_until_handle_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let app = start_hub_in(
            0,
            Some(directory.path().join("profiles")),
            directory.path().join("sideload"),
        )
        .unwrap();
        let address: std::net::SocketAddr = app
            .origin()
            .as_str()
            .strip_prefix("http://")
            .unwrap()
            .parse()
            .unwrap();
        let target = app
            .authenticated_url()
            .strip_prefix(app.origin().as_str())
            .unwrap()
            .to_owned();
        let mut client = TcpStream::connect(address).unwrap();
        write!(client, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        assert!(response.starts_with("HTTP/1.1 200"));
        app.shutdown_and_join().unwrap();
    }

    fn write_minimal_apk(path: &Path) {
        const LOCAL: u32 = 0x0403_4b50;
        const CENTRAL: u32 = 0x0201_4b50;
        const END: u32 = 0x0605_4b50;
        let names = ["AndroidManifest.xml", "classes.dex"];
        let mut bytes = Vec::new();
        let mut central = Vec::new();
        for name in names {
            let offset = bytes.len() as u32;
            bytes.extend_from_slice(&LOCAL.to_le_bytes());
            bytes.extend_from_slice(&20_u16.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(&[0; 16]);
            bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());

            central.extend_from_slice(&CENTRAL.to_le_bytes());
            central.extend_from_slice(&20_u16.to_le_bytes());
            central.extend_from_slice(&20_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&[0; 16]);
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_offset = bytes.len() as u32;
        let central_size = central.len() as u32;
        bytes.extend_from_slice(&central);
        bytes.extend_from_slice(&END.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&(names.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(names.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&central_size.to_le_bytes());
        bytes.extend_from_slice(&central_offset.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn hub_can_reuse_android_package_preflight() {
        let inspect: fn(&Path) -> Result<super::super::app::PackagePreflight> =
            super::super::app::package_preflight;
        let validate: fn(&super::super::app::PackagePreflight, bool) -> Result<()> =
            super::super::app::validate_install_preflight;
        let _ = (inspect, validate);
    }

    #[test]
    fn request_head_keeps_initial_body_for_streaming_routes() {
        let raw = b"POST /api/apk/upload?token=secret HTTP/1.1\r\nContent-Length: 6\r\n\r\nabcdef";
        let mut input = Cursor::new(raw.as_slice());

        let head = read_request_head(&mut input).unwrap();

        assert_eq!(head.method, "POST");
        assert_eq!(head.target, "/api/apk/upload?token=secret");
        assert_eq!(head.content_length, Some(6));
        assert_eq!(head.initial_body, b"abcdef");
    }

    #[test]
    fn request_head_rejects_ambiguous_or_chunked_bodies() {
        let duplicate = b"POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx";
        let chunked = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";

        assert!(read_request_head(&mut Cursor::new(duplicate)).is_err());
        assert!(read_request_head(&mut Cursor::new(chunked)).is_err());
    }

    #[test]
    fn bounded_body_reader_preserves_json_limit() {
        let head = RequestHead {
            method: "POST".to_owned(),
            target: "/api/action".to_owned(),
            content_length: Some((MAX_BODY_BYTES + 1) as u64),
            initial_body: Vec::new(),
        };

        let error = read_bounded_body(head, &mut io::empty(), MAX_BODY_BYTES).unwrap_err();

        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn apk_upload_length_is_nonzero_and_capped_at_four_gib() {
        assert!(validate_apk_upload_length(None).is_err());
        assert!(validate_apk_upload_length(Some(0)).is_err());
        assert_eq!(
            validate_apk_upload_length(Some(MAX_APK_BYTES)).unwrap(),
            MAX_APK_BYTES
        );
        assert!(validate_apk_upload_length(Some(MAX_APK_BYTES + 1)).is_err());
    }

    #[test]
    fn sideload_ticket_is_exact_lowercase_hex() {
        let valid = "0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(is_valid_sideload_ticket(valid));
        assert!(!is_valid_sideload_ticket(&valid[..47]));
        assert!(!is_valid_sideload_ticket(
            "0123456789abcdef0123456789abcdef0123456789abcdeF"
        ));
        assert!(!is_valid_sideload_ticket(
            "../../456789abcdef0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn apk_upload_streams_exactly_into_private_ticket_file() {
        let directory = tempfile::tempdir().unwrap();
        let ticket = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let head = RequestHead {
            method: "POST".to_owned(),
            target: "/api/apk/upload?token=secret".to_owned(),
            content_length: Some(6),
            initial_body: b"ab".to_vec(),
        };

        let path =
            stage_apk_upload(head, &mut Cursor::new(b"cdef"), directory.path(), ticket).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"abcdef");
        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!directory.path().join(format!("{ticket}.upload")).exists());
    }

    #[test]
    fn short_apk_upload_leaves_no_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let ticket = "abcdef0123456789abcdef0123456789abcdef0123456789";
        let head = RequestHead {
            method: "POST".to_owned(),
            target: "/api/apk/upload?token=secret".to_owned(),
            content_length: Some(6),
            initial_body: b"ab".to_vec(),
        };

        let error =
            stage_apk_upload(head, &mut Cursor::new(b"cd"), directory.path(), ticket).unwrap_err();

        assert!(error.to_string().contains("before APK upload completed"));
        assert!(fs::read_dir(directory.path()).unwrap().next().is_none());
    }

    #[test]
    fn package_intake_json_never_exposes_private_artifact_path() {
        let ticket = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let preflight = super::super::app::PackagePreflight {
            artifact: PackageInspection {
                path: PathBuf::from("/home/private/wroid/ticket.apk"),
                format: PackageFormat::Apk,
                file_size: 12_345,
                archive_entries: 2,
                has_android_manifest: true,
                has_dex: true,
                has_resources: false,
                native_abis: vec!["x86_64".to_owned()],
                embedded_apks: Vec::new(),
                obb_files: Vec::new(),
                encrypted_entries: 0,
            },
            abi_compatibility: AbiCompatibility::Native,
            android_abis: vec!["x86_64".to_owned()],
            arm_translation: Some(false),
        };

        let intake = package_intake_json(ticket, &preflight);
        let serialized = intake.to_string();

        assert_eq!(intake["ticket"], ticket);
        assert_eq!(intake["artifact"]["format"], "apk");
        assert_eq!(intake["compatibility"]["state"], "native");
        assert!(!serialized.contains("/home/private"));
        assert!(intake.get("path").is_none());
        assert!(intake["artifact"].get("path").is_none());
    }

    #[test]
    fn malformed_uploaded_package_is_rejected_and_deleted() {
        let directory = tempfile::tempdir().unwrap();
        let ticket = "abcdef0123456789abcdef0123456789abcdef0123456789";
        let bytes = b"not an APK";
        let head = RequestHead {
            method: "POST".to_owned(),
            target: "/api/apk/upload?token=secret".to_owned(),
            content_length: Some(bytes.len() as u64),
            initial_body: Vec::new(),
        };

        let response =
            handle_apk_upload_with_ticket(head, &mut Cursor::new(bytes), directory.path(), ticket);

        assert_eq!(response.status, 422);
        assert!(fs::read_dir(directory.path()).unwrap().next().is_none());
    }

    #[test]
    fn unauthorized_apk_upload_is_rejected_before_body_read() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        let (server, _) = listener.accept().unwrap();
        client
            .write_all(
                b"POST /api/apk/upload?token=wrong HTTP/1.1\r\nContent-Length: 1048576\r\n\r\n",
            )
            .unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let profiles = tempfile::tempdir().unwrap();
        let sideload = tempfile::tempdir().unwrap();
        let shutdown = AtomicBool::new(false);

        serve_connection(
            server,
            profiles.path(),
            sideload.path(),
            "secret",
            &shutdown,
        );

        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    }

    #[test]
    fn apk_install_worker_records_success_and_deletes_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let ticket = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let artifact = directory.path().join(format!("{ticket}.apk"));
        write_minimal_apk(&artifact);
        let called = Cell::new(false);

        run_apk_install_worker_with(directory.path(), ticket, |path| {
            called.set(true);
            assert_eq!(path, artifact);
            let status = read_apk_install_status(directory.path(), ticket)?;
            assert_eq!(status["state"], "installing");
            Ok(())
        })
        .unwrap();

        assert!(called.get());
        assert!(!artifact.exists());
        let status = read_apk_install_status(directory.path(), ticket).unwrap();
        assert_eq!(status["state"], "installed");
        assert!(status["detail"].as_str().unwrap().contains("Installed"));
    }

    #[test]
    fn apk_install_worker_bounds_failure_and_deletes_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let ticket = "abcdef0123456789abcdef0123456789abcdef0123456789";
        let artifact = directory.path().join(format!("{ticket}.apk"));
        write_minimal_apk(&artifact);

        let error = run_apk_install_worker_with(directory.path(), ticket, |_| {
            bail!("{}", "x".repeat(APK_STATUS_DETAIL_BYTES * 2))
        })
        .unwrap_err();

        assert!(error.to_string().contains('x'));
        assert!(!artifact.exists());
        let status = read_apk_install_status(directory.path(), ticket).unwrap();
        assert_eq!(status["state"], "failed");
        assert!(status["detail"].as_str().unwrap().len() <= APK_STATUS_DETAIL_BYTES);
    }

    #[test]
    fn apk_install_worker_uses_only_typed_ticket_argument() {
        let ticket = "0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            apk_install_worker_arguments(ticket),
            vec![
                OsString::from("install-apk-worker"),
                OsString::from("--ticket"),
                OsString::from(ticket),
            ]
        );
    }

    #[test]
    fn valid_uploaded_apk_records_inspected_status() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.apk");
        write_minimal_apk(&source);
        let bytes = fs::read(&source).unwrap();
        fs::remove_file(source).unwrap();
        let ticket = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let head = RequestHead {
            method: "POST".to_owned(),
            target: "/api/apk/upload?token=secret".to_owned(),
            content_length: Some(bytes.len() as u64),
            initial_body: Vec::new(),
        };

        let response =
            handle_apk_upload_with_ticket(head, &mut Cursor::new(bytes), directory.path(), ticket);

        assert_eq!(response.status, 201);
        assert_eq!(
            read_apk_install_status(directory.path(), ticket).unwrap()["state"],
            "inspected"
        );
    }

    #[test]
    fn discard_removes_only_an_inspected_ticket() {
        let directory = tempfile::tempdir().unwrap();
        let ticket = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let artifact = directory.path().join(format!("{ticket}.apk"));
        write_minimal_apk(&artifact);
        write_apk_install_status(directory.path(), ticket, "inspected", "Ready").unwrap();

        let response = handle_apk_discard(
            json!({ "ticket": ticket }).to_string().as_bytes(),
            directory.path(),
        );

        assert_eq!(response.status, 200);
        assert!(!artifact.exists());
        assert!(!directory.path().join(format!("{ticket}.json")).exists());
    }

    #[test]
    fn discard_cannot_race_an_existing_install_claim() {
        let directory = tempfile::tempdir().unwrap();
        let ticket = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let artifact = directory.path().join(format!("{ticket}.apk"));
        write_minimal_apk(&artifact);
        write_apk_install_status(directory.path(), ticket, "inspected", "Ready").unwrap();
        fs::write(
            directory.path().join(format!("{ticket}.lock")),
            b"installing",
        )
        .unwrap();

        let response = handle_apk_discard(
            json!({ "ticket": ticket }).to_string().as_bytes(),
            directory.path(),
        );

        assert_eq!(response.status, 422);
        assert!(artifact.exists());
        assert!(directory.path().join(format!("{ticket}.json")).exists());
    }

    #[test]
    fn install_claim_is_atomic_and_reports_queued_state() {
        let directory = tempfile::tempdir().unwrap();
        let ticket = "abcdef0123456789abcdef0123456789abcdef0123456789";
        let artifact = directory.path().join(format!("{ticket}.apk"));
        write_minimal_apk(&artifact);
        write_apk_install_status(directory.path(), ticket, "inspected", "Ready").unwrap();

        claim_apk_install(directory.path(), ticket).unwrap();
        let duplicate = claim_apk_install(directory.path(), ticket).unwrap_err();

        assert!(duplicate.to_string().contains("already"));
        assert_eq!(
            read_apk_install_status(directory.path(), ticket).unwrap()["state"],
            "queued"
        );
        assert_eq!(
            fs::metadata(directory.path().join(format!("{ticket}.lock")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn apk_status_handler_resolves_only_query_ticket() {
        let directory = tempfile::tempdir().unwrap();
        let ticket = "0123456789abcdef0123456789abcdef0123456789abcdef";
        write_apk_install_status(directory.path(), ticket, "queued", "Waiting").unwrap();

        let response = handle_apk_status(
            &format!("/api/apk/status?ticket={ticket}&token=secret"),
            directory.path(),
        );
        let status: Value = serde_json::from_slice(&response.body).unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(status["ticket"], ticket);
        assert_eq!(status["state"], "queued");
        assert_eq!(
            handle_apk_status(
                "/api/apk/status?ticket=../../etc/passwd&token=secret",
                directory.path(),
            )
            .status,
            404
        );
    }

    #[test]
    fn authenticated_apk_status_route_uses_private_sideload_directory() {
        let profiles = tempfile::tempdir().unwrap();
        let sideload = tempfile::tempdir().unwrap();
        let ticket = "0123456789abcdef0123456789abcdef0123456789abcdef";
        write_apk_install_status(sideload.path(), ticket, "queued", "Waiting").unwrap();
        let request = Request {
            method: "GET".to_owned(),
            target: format!("/api/apk/status?ticket={ticket}&token=secret"),
            body: Vec::new(),
        };

        let (response, close) =
            handle_request(&request, profiles.path(), sideload.path(), "secret");

        assert_eq!(response.status, 200);
        assert!(!close);
    }

    #[test]
    fn install_queue_rolls_back_claim_when_worker_cannot_start() {
        let directory = tempfile::tempdir().unwrap();
        let ticket = "abcdef0123456789abcdef0123456789abcdef0123456789";
        let artifact = directory.path().join(format!("{ticket}.apk"));
        write_minimal_apk(&artifact);
        write_apk_install_status(directory.path(), ticket, "inspected", "Ready").unwrap();

        let error = queue_apk_install_with(directory.path(), ticket, |_| {
            bail!("synthetic spawn failure")
        })
        .unwrap_err();

        assert!(error.to_string().contains("synthetic spawn failure"));
        assert!(artifact.exists());
        assert!(!directory.path().join(format!("{ticket}.lock")).exists());
        assert_eq!(
            read_apk_install_status(directory.path(), ticket).unwrap()["state"],
            "inspected"
        );
    }

    #[test]
    fn stale_cleanup_removes_only_known_ticket_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let ticket = "0123456789abcdef0123456789abcdef0123456789abcdef";
        for suffix in ["apk", "json", "lock", "upload"] {
            fs::write(
                directory.path().join(format!("{ticket}.{suffix}")),
                b"stale",
            )
            .unwrap();
        }
        let temporary = directory.path().join(format!(
            ".{ticket}.abcdef0123456789abcdef0123456789abcdef0123456789.tmp"
        ));
        fs::write(&temporary, b"stale").unwrap();
        let unrelated = directory.path().join("keep-me.txt");
        fs::write(&unrelated, b"user data").unwrap();

        cleanup_stale_sideload(
            directory.path(),
            SystemTime::now() + SIDELOAD_RETENTION + Duration::from_secs(1),
        )
        .unwrap();

        assert!(unrelated.exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn hub_assets_include_accessible_streaming_apk_intake() {
        for id in [
            "sideload-button",
            "sideload-input",
            "package-intake",
            "package-progress",
            "package-install-button",
            "package-discard-button",
        ] {
            assert!(INDEX_HTML.contains(&format!("id=\"{id}\"")), "missing {id}");
        }
        assert!(INDEX_HTML.contains("aria-live=\"polite\""));
        assert!(INDEX_HTML.contains("role=\"progressbar\""));
        assert!(APP_JS.contains("new XMLHttpRequest()"));
        assert!(APP_JS.contains("xhr.upload.addEventListener(\"progress\""));
        for endpoint in [
            "/api/apk/upload",
            "/api/apk/install",
            "/api/apk/status",
            "/api/apk/discard",
        ] {
            assert!(APP_JS.contains(endpoint), "missing {endpoint}");
        }
        assert!(STYLES_CSS.contains(".package-intake"));
        assert!(STYLES_CSS.contains(".package-scanner"));
    }

    #[test]
    fn hub_assets_include_persisted_game_mode_toggle_and_launch_payload() {
        assert!(INDEX_HTML.contains("id=\"game-mode-toggle\""));
        assert!(INDEX_HTML.contains("role=\"switch\""));
        assert!(INDEX_HTML.contains("Auto when installed"));
        assert!(APP_JS.contains("hubState.preferences?.gameMode"));
        assert!(APP_JS.contains("savePreferences({ gameMode:"));
        assert!(APP_JS.contains("gameMode: gameModeEnabled"));
        assert!(STYLES_CSS.contains(".game-mode-toggle"));
    }

    #[derive(Default)]
    struct FakeDesktopWaydroid {
        statuses: VecDeque<String>,
        readiness: VecDeque<std::result::Result<(), String>>,
        exits: VecDeque<Option<String>>,
        starts: usize,
    }

    impl DesktopWaydroidControl for FakeDesktopWaydroid {
        fn status(&mut self) -> Result<String> {
            self.statuses
                .pop_front()
                .context("missing synthetic Waydroid status")
        }

        fn start(&mut self) -> Result<()> {
            self.starts += 1;
            Ok(())
        }

        fn package_manager_ready(&mut self) -> Result<()> {
            self.readiness
                .pop_front()
                .context("missing synthetic package-manager result")?
                .map_err(anyhow::Error::msg)
        }

        fn start_exit(&mut self) -> Result<Option<String>> {
            Ok(self.exits.pop_front().flatten())
        }
    }

    #[test]
    fn installs_four_valid_starters_without_overwriting_changes() {
        let directory = tempfile::tempdir().unwrap();
        bootstrap_library(directory.path()).unwrap();
        let (profiles, errors) = library_profiles(directory.path()).unwrap();
        assert_eq!(profiles.len(), 4);
        assert!(errors.is_empty());

        let path = directory.path().join("pubg-mobile.json");
        let mut profile = ProfileV2::load_from_path(&path).unwrap();
        profile.name = "My PUBG layout".to_owned();
        profile.save_to_path(&path).unwrap();
        bootstrap_library(directory.path()).unwrap();

        assert_eq!(
            ProfileV2::load_from_path(path).unwrap().name,
            "My PUBG layout"
        );
    }

    #[test]
    fn layered_profile_state_exposes_readiness_metadata_and_counts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("layered.json");
        let mut profile: ProfileV2 = serde_json::from_str(STARTER_PROFILES[0].source).unwrap();
        profile.layers.push(LayerV2 {
            name: "grenades".to_owned(),
            activation: LayerActivation::Hold {
                key: "x".to_owned(),
            },
        });
        let entry = LibraryProfile {
            id: "layered".to_owned(),
            path,
            profile,
        };

        let state = library_game_json(&entry, None);

        assert_eq!(state["bindings"], 12);
        assert_eq!(state["controls"]["layers"], 1);
        assert_eq!(state["controls"]["taps"], 9);
        assert_eq!(state["controls"]["holds"], 1);
        assert_eq!(state["controls"]["joysticks"], 1);
        assert_eq!(state["controls"]["mouseAim"], 1);
        assert_eq!(
            state["layers"],
            json!([{"name": "grenades", "mode": "hold", "key": "x"}])
        );
    }

    #[test]
    fn untouched_starter_stays_equal_after_profile_v2_load_save_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pubg-v2.json");
        let original: ProfileV2 = serde_json::from_str(STARTER_PROFILES[0].source).unwrap();

        assert!(original.layers.is_empty());
        assert!(original
            .bindings
            .iter()
            .all(|binding| binding.layer.is_none() && binding.modifier.is_none()));

        original.save_to_path(&path).unwrap();
        let reloaded = ProfileV2::load_from_path(&path).unwrap();

        assert_eq!(reloaded, original);
        assert!(starter_predecessors(&original)
            .iter()
            .all(|predecessor| predecessor.layers.is_empty()));
    }

    #[test]
    fn desktop_runtime_start_waits_for_android_package_manager() {
        let mut desktop = FakeDesktopWaydroid {
            statuses: VecDeque::from([
                "Session:\tSTOPPED".to_owned(),
                "Session:\tRUNNING".to_owned(),
                "Session:\tRUNNING".to_owned(),
            ]),
            readiness: VecDeque::from([Err("package service warming up".to_owned()), Ok(())]),
            ..FakeDesktopWaydroid::default()
        };

        assert!(ensure_desktop_waydroid_ready(&mut desktop, 2, Duration::ZERO).unwrap());
        assert_eq!(desktop.starts, 1);
    }

    #[test]
    fn ready_desktop_runtime_is_reused_without_restart() {
        let mut desktop = FakeDesktopWaydroid {
            statuses: VecDeque::from([
                "Session: RUNNING".to_owned(),
                "Session: RUNNING".to_owned(),
            ]),
            readiness: VecDeque::from([Ok(())]),
            ..FakeDesktopWaydroid::default()
        };

        assert!(!ensure_desktop_waydroid_ready(&mut desktop, 1, Duration::ZERO).unwrap());
        assert_eq!(desktop.starts, 0);
    }

    #[test]
    fn desktop_runtime_reports_early_session_exit() {
        let mut desktop = FakeDesktopWaydroid {
            statuses: VecDeque::from([
                "Session:\tSTOPPED".to_owned(),
                "Session:\tSTOPPED".to_owned(),
            ]),
            exits: VecDeque::from([Some("exit status: 1".to_owned())]),
            ..FakeDesktopWaydroid::default()
        };

        let error = ensure_desktop_waydroid_ready(&mut desktop, 1, Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("exit status: 1"));
        assert_eq!(desktop.starts, 1);
    }

    #[test]
    fn waydroid_running_status_parser_ignores_container_state() {
        assert!(session_is_running(
            "Session:\tRUNNING\nContainer:\tRUNNING\n"
        ));
        assert!(!session_is_running(
            "Session:\tSTOPPED\nContainer:\tRUNNING\n"
        ));
    }

    #[test]
    fn upgrades_only_untouched_legacy_starter_controls() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pubg-mobile.json");
        let current: ProfileV2 = serde_json::from_str(STARTER_PROFILES[0].source).unwrap();
        let legacy = legacy_tap_starter(&current).unwrap();
        legacy.save_to_path(&path).unwrap();

        bootstrap_library(directory.path()).unwrap();

        assert_eq!(ProfileV2::load_from_path(&path).unwrap(), current);

        let mut customized = legacy;
        customized.name = "My legacy controls".to_owned();
        customized.save_to_path(&path).unwrap();
        bootstrap_library(directory.path()).unwrap();
        assert_eq!(
            ProfileV2::load_from_path(path).unwrap().name,
            "My legacy controls"
        );
    }

    #[test]
    fn upgrades_only_untouched_predecessors_missing_new_essential_controls() {
        for (starter_index, missing_binding) in [(0, "reload"), (3, "aim_down_sights")] {
            let directory = tempfile::tempdir().unwrap();
            let starter = &STARTER_PROFILES[starter_index];
            let path = directory.path().join(format!("{}.json", starter.id));
            let current: ProfileV2 = serde_json::from_str(starter.source).unwrap();
            let mut predecessor = current.clone();
            predecessor
                .bindings
                .retain(|binding| binding.name != missing_binding);
            predecessor.save_to_path(&path).unwrap();

            bootstrap_library(directory.path()).unwrap();
            assert_eq!(ProfileV2::load_from_path(&path).unwrap(), current);

            predecessor.name.push_str(" custom");
            predecessor.save_to_path(&path).unwrap();
            bootstrap_library(directory.path()).unwrap();
            assert_eq!(
                ProfileV2::load_from_path(&path).unwrap().name,
                predecessor.name
            );
        }
    }

    #[test]
    fn starter_profiles_include_essential_gameplay_bindings() {
        let profiles = STARTER_PROFILES
            .iter()
            .map(|starter| serde_json::from_str::<ProfileV2>(starter.source).unwrap())
            .collect::<Vec<_>>();

        assert!(profiles.iter().all(|profile| profile.validate().is_ok()));
        assert!(has_key_binding(&profiles[0], "reload", "r"));
        assert!(has_key_binding(&profiles[1], "reload", "r"));
        assert!(profiles[2]
            .bindings
            .iter()
            .any(|binding| binding.name == "attack"));
        assert!(has_mouse_binding(&profiles[3], "aim_down_sights", "right"));
        assert_eq!(
            profiles.iter().map(profile_needs_mouse).collect::<Vec<_>>(),
            [true, true, false, true]
        );
    }

    fn has_key_binding(profile: &ProfileV2, name: &str, key: &str) -> bool {
        profile.bindings.iter().any(|binding| {
            binding.name == name
                && matches!(
                    &binding.input,
                    wroid_core::profile_v2::InputV2::Key { key: binding_key }
                        if binding_key == key
                )
                && matches!(binding.action, ActionV2::Tap { .. })
        })
    }

    fn has_mouse_binding(profile: &ProfileV2, name: &str, button: &str) -> bool {
        profile.bindings.iter().any(|binding| {
            binding.name == name
                && matches!(
                    &binding.input,
                    wroid_core::profile_v2::InputV2::MouseButton {
                        button: binding_button
                    } if binding_button == button
                )
                && matches!(binding.action, ActionV2::Tap { .. })
        })
    }

    #[test]
    fn rejects_requests_without_hub_token() {
        let request = Request {
            method: "GET".to_owned(),
            target: "/api/state".to_owned(),
            body: Vec::new(),
        };
        let directory = tempfile::tempdir().unwrap();
        let sideload = tempfile::tempdir().unwrap();
        let (response, close) =
            handle_request(&request, directory.path(), sideload.path(), "secret");
        assert_eq!(response.status, 403);
        assert!(!close);
    }

    #[test]
    fn serves_authenticated_compatibility_state_asset() {
        let request = Request {
            method: "GET".to_owned(),
            target: "/compatibility-state.js?token=secret".to_owned(),
            body: Vec::new(),
        };
        let directory = tempfile::tempdir().unwrap();
        let sideload = tempfile::tempdir().unwrap();

        let (response, close) =
            handle_request(&request, directory.path(), sideload.path(), "secret");

        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/javascript; charset=utf-8");
        assert!(String::from_utf8(response.body)
            .unwrap()
            .contains("activeRootFinding"));
        assert!(!close);
    }

    #[test]
    fn imported_profiles_are_validated_and_never_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let source = STARTER_PROFILES[0].source.as_bytes();
        let first = handle_profile_import(source, directory.path());
        assert_eq!(first.status, 201);
        let second = handle_profile_import(source, directory.path());
        assert_eq!(second.status, 409);

        let invalid = handle_profile_import(
            br#"{"schema_version":2,"name":"","package_name":"","bindings":[]}"#,
            directory.path(),
        );
        assert_eq!(invalid.status, 422);
    }

    #[test]
    fn only_known_performance_presets_are_accepted() {
        assert_eq!(
            launch_resolution(&json!({"width": 1280, "height": 720})).unwrap(),
            (1280, 720)
        );
        assert!(launch_resolution(&json!({"width": 1024, "height": 768})).is_err());
    }

    #[test]
    fn game_mode_launch_flag_defaults_on_and_rejects_non_booleans() {
        assert!(launch_game_mode(&json!({})).unwrap());
        assert!(!launch_game_mode(&json!({"gameMode": false})).unwrap());
        assert!(launch_game_mode(&json!({"gameMode": "auto"})).is_err());
    }

    #[test]
    fn package_names_become_safe_profile_ids() {
        assert_eq!(profile_id("com.Example.Game"), "com.example.game");
        assert_eq!(profile_id(" Custom Game! "), "custom-game");
        assert_eq!(profile_id("!!!"), "custom-game");
    }

    #[test]
    fn hub_uses_short_game_titles_without_mutating_profile_names() {
        assert_eq!(
            display_name("PUBG Mobile — default landscape HUD"),
            "PUBG Mobile"
        );
        assert_eq!(display_name("Custom Game"), "Custom Game");
    }

    #[test]
    fn target_game_catalog_matches_only_verified_exact_packages() {
        use crate::commands::game_catalog::family_for_package;

        for (package, kind) in [
            ("com.tencent.ig", "pubg"),
            ("com.pubg.krmobile", "pubg"),
            ("com.vng.pubgmobile", "pubg"),
            ("com.rekoo.pubgm", "pubg"),
            ("com.pubg.imobile", "pubg"),
            ("com.dts.freefireth", "freefire"),
            ("com.dts.freefiremax", "freefire"),
            ("com.supercell.brawlstars", "brawl"),
            ("com.axlebolt.standoff2", "standoff"),
        ] {
            assert_eq!(family_for_package(package).unwrap().kind, kind, "{package}");
        }
        assert!(family_for_package("com.tencent.ig.fake").is_none());
        assert!(family_for_package("com.dts.freefiremax.clone").is_none());
    }

    #[test]
    fn target_game_catalog_prefers_canonical_when_editions_coexist() {
        use crate::commands::game_catalog::{installed_variant, GAME_FAMILIES};

        let installed = vec!["com.pubg.krmobile".to_owned(), "com.tencent.ig".to_owned()];
        let selected = installed_variant(&GAME_FAMILIES[0], &installed).unwrap();

        assert_eq!(selected.package, "com.tencent.ig");
        assert_eq!(game_kind("com.pubg.krmobile"), "pubg");
        assert_eq!(starter_order("com.pubg.krmobile"), 0);
        assert_eq!(
            game_description("com.dts.freefiremax"),
            "Fast battle royale · tuned for low latency"
        );
    }

    #[test]
    fn installed_variant_profile_clones_current_controls_for_exact_package() {
        let directory = tempfile::tempdir().unwrap();
        bootstrap_library(directory.path()).unwrap();
        let (profiles, _) = library_profiles(directory.path()).unwrap();
        let canonical = profiles
            .iter()
            .find(|profile| profile.profile.package_name == "com.tencent.ig")
            .unwrap()
            .profile
            .clone();

        let report = reconcile_installed_game_variants(
            directory.path(),
            &profiles,
            &["com.pubg.krmobile".to_owned()],
        );

        assert_eq!(report.created, ["pubg-mobile-korea"]);
        assert!(report.warnings.is_empty());
        let path = directory.path().join("pubg-mobile-korea.json");
        let derived = ProfileV2::load_from_path(&path).unwrap();
        assert_eq!(derived.name, "PUBG Mobile Korea");
        assert_eq!(derived.package_name, "com.pubg.krmobile");
        assert_eq!(derived.bindings, canonical.bindings);
        assert_eq!(calibration_json(&path)["state"], "needed");
    }

    #[test]
    fn installed_variant_reconciliation_is_idempotent_after_library_reload() {
        let directory = tempfile::tempdir().unwrap();
        bootstrap_library(directory.path()).unwrap();
        let installed = ["com.dts.freefiremax".to_owned()];
        let (profiles, _) = library_profiles(directory.path()).unwrap();
        assert_eq!(
            reconcile_installed_game_variants(directory.path(), &profiles, &installed).created,
            ["free-fire-max"]
        );

        let (reloaded, _) = library_profiles(directory.path()).unwrap();
        let second = reconcile_installed_game_variants(directory.path(), &reloaded, &installed);

        assert!(second.created.is_empty());
        assert!(second.warnings.is_empty());
    }

    #[test]
    fn installed_variant_existing_profile_by_package_is_never_duplicated() {
        let directory = tempfile::tempdir().unwrap();
        bootstrap_library(directory.path()).unwrap();
        let (profiles, _) = library_profiles(directory.path()).unwrap();
        let mut custom = profiles
            .iter()
            .find(|profile| profile.profile.package_name == "com.tencent.ig")
            .unwrap()
            .profile
            .clone();
        custom.name = "My Korea controls".to_owned();
        custom.package_name = "com.pubg.krmobile".to_owned();
        custom
            .save_to_path(directory.path().join("my-korea.json"))
            .unwrap();
        let (profiles, _) = library_profiles(directory.path()).unwrap();

        let report = reconcile_installed_game_variants(
            directory.path(),
            &profiles,
            &["com.pubg.krmobile".to_owned()],
        );

        assert!(report.created.is_empty());
        assert!(!directory.path().join("pubg-mobile-korea.json").exists());
        assert_eq!(
            ProfileV2::load_from_path(directory.path().join("my-korea.json"))
                .unwrap()
                .name,
            "My Korea controls"
        );
    }

    #[test]
    fn installed_variant_stable_id_collision_is_preserved_byte_for_byte() {
        let directory = tempfile::tempdir().unwrap();
        bootstrap_library(directory.path()).unwrap();
        let (profiles, _) = library_profiles(directory.path()).unwrap();
        let collision = directory.path().join("free-fire-max.json");
        fs::write(&collision, b"user-owned collision").unwrap();

        let report = reconcile_installed_game_variants(
            directory.path(),
            &profiles,
            &["com.dts.freefiremax".to_owned()],
        );

        assert!(report.created.is_empty());
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(fs::read(collision).unwrap(), b"user-owned collision");
    }

    #[test]
    fn calibration_state_distinguishes_missing_ready_and_invalid_assets() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("game.json");
        assert_eq!(calibration_json(&profile)["state"], "needed");

        let asset = directory
            .path()
            .join(".wroid-assets")
            .join("game.json.background");
        fs::create_dir_all(asset.parent().unwrap()).unwrap();
        fs::write(&asset, b"\x89PNG\r\n\x1a\npreview").unwrap();
        let ready = calibration_json(&profile);
        assert_eq!(ready["state"], "ready");
        assert_eq!(ready["ready"], true);

        fs::write(&asset, b"broken").unwrap();
        let invalid = calibration_json(&profile);
        assert_eq!(invalid["state"], "invalid");
        assert_eq!(invalid["ready"], false);
    }

    #[test]
    fn calibration_requires_the_selected_game_package() {
        let packages = vec!["com.tencent.ig".to_owned()];
        ensure_package_available_for_calibration("com.tencent.ig", &packages).unwrap();
        let error =
            ensure_package_available_for_calibration("com.dts.freefireth", &packages).unwrap_err();
        assert!(error.to_string().contains("not installed"));
    }

    #[test]
    fn calibration_retries_until_the_selected_package_is_visible() {
        let mut package_lists = VecDeque::from([
            Ok(vec!["com.android.settings".to_owned()]),
            Ok(vec!["com.axlebolt.standoff2".to_owned()]),
        ]);

        wait_for_package_available_for_calibration(
            "com.axlebolt.standoff2",
            2,
            Duration::ZERO,
            || package_lists.pop_front().unwrap(),
        )
        .unwrap();
        assert!(package_lists.is_empty());
    }

    #[test]
    fn input_device_state_preserves_names_and_preferred_order() {
        let state = input_devices_json(Ok(vec![
            InputDeviceInfo {
                path: PathBuf::from("/dev/input/by-id/gaming-event-kbd"),
                name: "Gaming Keyboard".to_owned(),
            },
            InputDeviceInfo {
                path: PathBuf::from("/dev/input/by-id/backup-event-kbd"),
                name: "Backup Keyboard".to_owned(),
            },
        ]));

        assert_eq!(state["value"], "/dev/input/by-id/gaming-event-kbd");
        assert_eq!(state["devices"][0]["name"], "Gaming Keyboard");
        assert_eq!(state["devices"][0]["preferred"], true);
        assert_eq!(state["devices"][1]["preferred"], false);
    }

    #[test]
    fn launch_accepts_only_a_freshly_discovered_device_path() {
        let devices = vec![InputDeviceInfo {
            path: PathBuf::from("/dev/input/by-id/gaming-event-kbd"),
            name: "Gaming Keyboard".to_owned(),
        }];
        let selected = selected_input_device(
            &json!({"keyboard": "/dev/input/by-id/gaming-event-kbd"}),
            "keyboard",
            Ok(devices.clone()),
        )
        .unwrap();
        assert_eq!(
            selected,
            Some(PathBuf::from("/dev/input/by-id/gaming-event-kbd"))
        );

        assert!(selected_input_device(
            &json!({"keyboard": "/tmp/not-an-input-device"}),
            "keyboard",
            Ok(devices)
        )
        .is_err());
    }

    #[test]
    fn input_self_test_uses_bounded_production_launch_without_a_package() {
        let command = input_self_test_command(
            Path::new("/opt/wroid"),
            Path::new("/profiles/pubg-v2.json"),
            1280,
            720,
            Some(Path::new("/dev/input/by-id/gaming-event-kbd")),
            Some(Path::new("/dev/input/by-id/gaming-event-mouse")),
        );

        assert_eq!(
            command,
            [
                "/opt/wroid",
                "launch-v2",
                "/profiles/pubg-v2.json",
                "--width",
                "1280",
                "--height",
                "720",
                "--no-launch",
                "--trace-input",
                "--exit-after-seconds",
                "20",
                "--keyboard",
                "/dev/input/by-id/gaming-event-kbd",
                "--mouse",
                "/dev/input/by-id/gaming-event-mouse",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn helper_setup_opens_only_the_typed_installer_command() {
        assert_eq!(
            system_helper_setup_command(Path::new("/opt/wroid/bin/wroid")),
            ["/opt/wroid/bin/wroid", "helper", "install"].map(OsString::from)
        );
    }

    #[test]
    fn omitted_device_keeps_cli_autodiscovery_even_if_enumeration_failed() {
        let selected = selected_input_device(
            &json!({}),
            "mouse",
            Err(anyhow::anyhow!("device scan failed")),
        )
        .unwrap();
        assert_eq!(selected, None);
    }

    #[test]
    fn input_bridge_state_preserves_active_owner_and_probe_errors() {
        let active = input_bridge_json(Ok(crate::commands::launch_v2::ActiveGameSessionState {
            owner: Some("PID 42 · PUBG Mobile".to_owned()),
            can_stop: true,
        }));
        assert_eq!(active["busy"], true);
        assert_eq!(active["owner"], "PID 42 · PUBG Mobile");
        assert_eq!(active["canStop"], true);
        assert!(active["error"].is_null());

        let failed = input_bridge_json(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "lock unreadable",
        )));
        assert_eq!(failed["busy"], false);
        assert_eq!(failed["canStop"], false);
        assert_eq!(failed["error"], "lock unreadable");
    }

    #[test]
    fn last_session_state_is_serialized_for_the_hub() {
        let state: crate::commands::launch_v2::LastGameSessionState =
            serde_json::from_value(json!({
                "version": 1,
                "pid": 42,
                "profilePath": "/profiles/pubg-v2.json",
                "profileName": "PUBG Mobile",
                "packageName": "com.tencent.ig",
                "state": "failed",
                "detail": "package launch failed",
                "finishedUnixMillis": 123456,
                "performance": {
                    "framesSubmitted": 1200,
                    "peakSimultaneousContacts": 4,
                    "mouseAimRecenters": 3,
                    "readerToInject": {
                        "samples": 800,
                        "p50Micros": 300,
                        "p95Micros": 900,
                        "p99Micros": 1400,
                        "maxMicros": 2100
                    },
                    "kernelToInject": null,
                    "rejectedKernelTimestamps": 0
                }
            }))
            .unwrap();
        let value = last_game_session_json(Ok(Some(state)));

        assert_eq!(value["profileName"], "PUBG Mobile");
        assert_eq!(value["state"], "failed");
        assert_eq!(value["finishedUnixMillis"], 123456);
        assert_eq!(value["performance"]["readerToInject"]["p95Micros"], 900);
        assert!(last_game_session_json(Ok(None)).is_null());
    }
}
