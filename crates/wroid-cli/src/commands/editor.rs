use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use wroid_core::profile_v2::ProfileV2;

use super::desktop_webview::{self, CONTROLS_WINDOW};
use super::kwin_screenshot;
use super::local_web_app::{LocalWebApp, WebUiMode};
use super::preferences;
use super::terminal::spawn_terminal;

const INDEX_HTML: &str = include_str!("../../assets/editor/index.html");
const STYLES_CSS: &str = include_str!("../../assets/editor/styles.css");
const PROFILE_MODEL_JS: &str = include_str!("../../assets/editor/profile-model.js");
const SETUP_GUIDE_JS: &str = include_str!("../../assets/editor/setup-guide.js");
const APP_JS: &str = include_str!("../../assets/editor/app.js");
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_PROFILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Native live-stream state: 0 idle, 1 streaming, 2 stop requested.
static LIVE_STREAM_STATE: AtomicU8 = AtomicU8::new(0);
const LIVE_STREAM_INTERVAL: Duration = Duration::from_millis(80);

pub(crate) fn edit_v2(path: PathBuf, port: u16, mode: WebUiMode) -> Result<()> {
    if mode == WebUiMode::Native {
        return desktop_webview::run_native_app(CONTROLS_WINDOW, move || start_editor(path, port));
    }
    let app = start_editor(path, port)?;

    println!("Wroid Controls Studio");
    println!("The server listens on localhost only. Save & Close stops it.");
    match mode {
        WebUiMode::Browser => open_url(&app.authenticated_url()),
        WebUiMode::Headless => println!("Editor: {}", app.authenticated_url()),
        WebUiMode::Native => unreachable!("handled before starting the native application"),
    }
    app.wait()
}

pub(crate) fn start_editor(path: PathBuf, port: u16) -> Result<LocalWebApp> {
    let path = absolute_existing_file(&path)?;
    let profile = ProfileV2::load_from_path(&path)
        .with_context(|| format!("failed to load profile v2 {}", path.display()))?;
    profile
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid profile v2: {}", error.errors.join("; ")))?;

    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
        .context("failed to bind the local profile editor")?;
    let address = listener.local_addr()?;
    let token = local_token()?;
    listener
        .set_nonblocking(true)
        .context("failed to configure the local profile editor")?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = Arc::clone(&shutdown);
    LocalWebApp::spawn(address, token.clone(), Arc::clone(&shutdown), move || {
        while !server_shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let path = path.clone();
                    let token = token.clone();
                    let shutdown = Arc::clone(&server_shutdown);
                    thread::spawn(move || serve_connection(stream, &path, &token, &shutdown));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error).context("profile editor connection failed"),
            }
        }
        Ok(())
    })
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

fn serve_connection(mut stream: TcpStream, path: &Path, token: &str, shutdown: &AtomicBool) {
    if let Err(error) = stream.set_nonblocking(false) {
        eprintln!("Warning: could not configure editor client: {error}");
        return;
    }
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            let _ = write_response(
                &mut stream,
                Response::json(400, &json_error(&error.to_string())),
            );
            return;
        }
    };
    // The live stream outlives a single buffered response; serve it inline
    // until the browser disconnects or requests a stop.
    if request.method == "GET" && request.target.starts_with("/api/live.mjpg") {
        serve_live_stream(&mut stream, &request, token);
        return;
    }
    let (response, close) = handle_request(&request, path, token);
    if let Err(error) = write_response(&mut stream, response) {
        eprintln!("Warning: editor client disconnected: {error}");
    }
    if close {
        shutdown.store(true, Ordering::Release);
    }
}

/// Stream desktop frames as multipart/x-mixed-replace BMP parts.
///
/// This bypasses xdg-desktop-portal entirely (its ScreenCast backend can die
/// with PipeWire "target not found" on Plasma): each part is one KWin
/// ScreenShot2 capture. One stream at a time keeps CPU bounded.
fn serve_live_stream(stream: &mut TcpStream, request: &Request, token: &str) {
    let authorized = request
        .target
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or("")
        .split('&')
        .filter_map(|part| part.split_once('='))
        .any(|(name, value)| name == "token" && value == token);
    if !authorized {
        let _ = write_response(
            stream,
            Response::json(403, r#"{"ok":false,"error":"Invalid editor token"}"#),
        );
        return;
    }
    if LIVE_STREAM_STATE
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        let _ = write_response(
            stream,
            Response::json(
                409,
                &json_error("a live window stream is already active in another tab"),
            ),
        );
        return;
    }

    let header = "HTTP/1.1 200 OK\r\n\
         Content-Type: multipart/x-mixed-replace; boundary=wroidframe\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         X-Content-Type-Options: nosniff\r\n\r\n";
    let result = (|| -> io::Result<()> {
        stream.write_all(header.as_bytes())?;
        loop {
            if LIVE_STREAM_STATE.load(Ordering::Acquire) == 2 {
                break;
            }
            let started = Instant::now();
            match kwin_screenshot::capture_active_screen() {
                Ok(frame) => {
                    let bmp = kwin_screenshot::frame_to_bmp(&frame);
                    write!(
                        stream,
                        "--wroidframe\r\nContent-Type: image/bmp\r\nContent-Length: {}\r\n\r\n",
                        bmp.len()
                    )?;
                    stream.write_all(&bmp)?;
                    stream.flush()?;
                }
                Err(error) => {
                    eprintln!("[live-stream] capture failed: {error:#}");
                    let message =
                        format!("--wroidframe\r\nContent-Type: text/plain\r\n\r\n{error}\n");
                    stream.write_all(message.as_bytes())?;
                    stream.flush()?;
                    break;
                }
            }
            let spent = started.elapsed();
            if spent < LIVE_STREAM_INTERVAL {
                thread::sleep(LIVE_STREAM_INTERVAL - spent);
            }
        }
        Ok(())
    })();
    LIVE_STREAM_STATE.store(0, Ordering::Release);
    if let Err(error) = result {
        // Browser disconnects are the normal way a stream ends.
        if error.kind() != io::ErrorKind::UnexpectedEof
            && error.kind() != io::ErrorKind::BrokenPipe
            && error.kind() != io::ErrorKind::ConnectionReset
        {
            eprintln!("[live-stream] ended with {error}");
        }
    }
}

fn absolute_existing_file(path: &Path) -> Result<PathBuf> {
    if !path.is_file() {
        bail!("profile v2 file does not exist: {}", path.display());
    }
    path.canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))
}

fn local_token() -> Result<String> {
    let mut bytes = [0_u8; 24];
    fs::File::open("/dev/urandom")
        .context("failed to open system random source")?
        .read_exact(&mut bytes)
        .context("failed to generate editor access token")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    method: String,
    target: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut data = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            bail!("connection closed before request headers");
        }
        data.extend_from_slice(&buffer[..read]);
        if data.len() > MAX_HEADER_BYTES {
            bail!("request headers are too large");
        }
        if let Some(index) = find_bytes(&data, b"\r\n\r\n") {
            break index + 4;
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
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        bail!("unsupported HTTP version");
    }

    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| {
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>())
        })
        .transpose()
        .context("invalid Content-Length")?
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        bail!("request body is too large");
    }

    while data.len() - header_end < content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            bail!("connection closed before request body");
        }
        data.extend_from_slice(&buffer[..read]);
    }

    Ok(Request {
        method,
        target,
        body: data[header_end..header_end + content_length].to_vec(),
    })
}

fn handle_request(request: &Request, profile_path: &Path, token: &str) -> (Response, bool) {
    let (route, query) = request
        .target
        .split_once('?')
        .unwrap_or((&request.target, ""));
    eprintln!(
        "[editor-http] {} {} (body={} bytes)",
        request.method,
        request.target,
        request.body.len()
    );
    let authorized = query
        .split('&')
        .filter_map(|part| part.split_once('='))
        .any(|(name, value)| name == "token" && value == token);

    match (request.method.as_str(), route) {
        ("GET", "/styles.css") => (Response::css(STYLES_CSS), false),
        ("GET", "/profile-model.js") => (Response::javascript(PROFILE_MODEL_JS), false),
        ("GET", "/setup-guide.js") => (Response::javascript(SETUP_GUIDE_JS), false),
        ("GET", "/app.js") => (Response::javascript(APP_JS), false),
        ("GET", "/") if authorized => (Response::html(INDEX_HTML), false),
        ("GET", "/api/profile") if authorized => match fs::read_to_string(profile_path) {
            Ok(profile) => (Response::json(200, &profile), false),
            Err(error) => (Response::json(500, &json_error(&error.to_string())), false),
        },
        // The frontend compares this against the mtime it loaded/saved to
        // detect another editor window overwriting the file behind its back.
        ("GET", "/api/profile-status") if authorized => match fs::metadata(profile_path) {
            Ok(metadata) => {
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as u64)
                    .unwrap_or(0);
                (
                    Response::json(
                        200,
                        &serde_json::json!({"ok": true, "modified": modified}).to_string(),
                    ),
                    false,
                )
            }
            Err(error) => (Response::json(500, &json_error(&error.to_string())), false),
        },
        ("GET", "/api/profile-backup") if authorized => {
            let path = profile_backup_path(profile_path);
            match ProfileV2::load_from_path(&path) {
                Ok(profile) => match profile.validate() {
                    Ok(()) => (
                        Response::json(
                            200,
                            &serde_json::json!({
                                "ok": true,
                                "profile": profile,
                            })
                            .to_string(),
                        ),
                        false,
                    ),
                    Err(error) => (
                        Response::json(
                            500,
                            &json_error(&format!(
                                "previous profile save is invalid: {}",
                                error.errors.join("; ")
                            )),
                        ),
                        false,
                    ),
                },
                Err(wroid_core::profile_v2::ProfileV2LoadError::Io(error))
                    if error.kind() == io::ErrorKind::NotFound =>
                {
                    (
                        Response::json(404, &json_error("no previous profile save")),
                        false,
                    )
                }
                Err(error) => (Response::json(500, &json_error(&error.to_string())), false),
            }
        }
        ("GET", "/api/preferences") if authorized => match preferences::load_default() {
            Ok(preferences) => (
                Response::json(
                    200,
                    &serde_json::json!({
                        "ok": true,
                        "preferences": preferences,
                    })
                    .to_string(),
                ),
                false,
            ),
            Err(error) => (Response::json(500, &json_error(&error.to_string())), false),
        },
        ("PUT", "/api/preferences") if authorized => {
            match preferences::update_default(&request.body) {
                Ok(preferences) => (
                    Response::json(
                        200,
                        &serde_json::json!({
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
        ("GET", "/api/background") if authorized => {
            let path = background_path(profile_path);
            match fs::read(&path) {
                Ok(data) => match image_content_type(&data) {
                    Some(content_type) => (Response::bytes(200, content_type, data), false),
                    None => (
                        Response::json(500, &json_error("stored background is not a valid image")),
                        false,
                    ),
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => (
                    Response::json(404, &json_error("no saved background")),
                    false,
                ),
                Err(error) => (Response::json(500, &json_error(&error.to_string())), false),
            }
        }
        ("PUT", "/api/profile") if authorized => {
            if request.body.len() > MAX_PROFILE_BYTES {
                return (
                    Response::json(413, &json_error("profile document is too large")),
                    false,
                );
            }
            let profile: ProfileV2 = match serde_json::from_slice(&request.body) {
                Ok(profile) => profile,
                Err(error) => {
                    return (
                        Response::json(422, &json_error(&format!("invalid JSON: {error}"))),
                        false,
                    )
                }
            };
            if let Err(error) = profile.validate() {
                let body = serde_json::json!({
                    "ok": false,
                    "errors": error.errors,
                })
                .to_string();
                return (Response::json(422, &body), false);
            }
            match save_profile_with_backup(profile_path, &profile) {
                Ok(changed) => {
                    let modified = fs::metadata(profile_path)
                        .ok()
                        .and_then(|metadata| metadata.modified().ok())
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_millis() as u64);
                    (
                        Response::json(
                            200,
                            &serde_json::json!({
                                "ok": true,
                                "message": if changed { "Profile saved" } else { "Profile unchanged" },
                                "changed": changed,
                                "backupAvailable": profile_backup_path(profile_path).is_file(),
                                "modified": modified,
                            })
                            .to_string(),
                        ),
                        false,
                    )
                }
                Err(error) => (Response::json(500, &json_error(&error.to_string())), false),
            }
        }
        ("PUT", "/api/background") if authorized => {
            let Some(content_type) = image_content_type(&request.body) else {
                return (
                    Response::json(
                        422,
                        &json_error("background must be a PNG, JPEG, WebP, GIF, or BMP image"),
                    ),
                    false,
                );
            };
            match save_background(profile_path, &request.body) {
                Ok(()) => (
                    Response::json(
                        200,
                        &serde_json::json!({
                            "ok": true,
                            "message": "Calibration background saved",
                            "contentType": content_type,
                        })
                        .to_string(),
                    ),
                    false,
                ),
                Err(error) => (Response::json(500, &json_error(&error.to_string())), false),
            }
        }
        ("DELETE", "/api/background") if authorized => {
            let path = background_path(profile_path);
            match fs::remove_file(path) {
                Ok(()) => (
                    Response::json(200, r#"{"ok":true,"message":"Background removed"}"#),
                    false,
                ),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (
                    Response::json(200, r#"{"ok":true,"message":"No background was saved"}"#),
                    false,
                ),
                Err(error) => (Response::json(500, &json_error(&error.to_string())), false),
            }
        }
        ("POST", "/api/snapshot") if authorized => {
            // Primary: KWin ScreenShot2 of the gaming monitor only (highest
            // refresh output, or WROID_SNAPSHOT_OUTPUT). Spectacle `-f` grabs
            // the whole desktop across every monitor, so it is only a
            // fallback when the KWin path is unavailable.
            match kwin_screenshot::capture_primary_screen() {
                Ok(frame) => (
                    Response::bytes(200, "image/bmp", kwin_screenshot::frame_to_bmp(&frame)),
                    false,
                ),
                Err(kwin_error) => {
                    eprintln!(
                        "[snapshot] KWin ScreenShot2 failed, trying spectacle: {kwin_error:#}"
                    );
                    match capture_screen_snapshot() {
                        Ok(bytes) => (Response::bytes(200, "image/png", bytes), false),
                        Err(spectacle_error) => (
                            Response::json(
                                500,
                                &json_error(&format!(
                                    "KWin ScreenShot2 failed: {kwin_error:#}; spectacle fallback also failed: {spectacle_error}"
                                )),
                            ),
                            false,
                        ),
                    }
                }
            }
        }
        ("GET", "/api/live-capability") if authorized => (
            Response::json(
                200,
                &serde_json::json!({
                    "ok": true,
                    "nativeLive": kwin_screenshot::is_kde_wayland(),
                })
                .to_string(),
            ),
            false,
        ),
        ("DELETE", "/api/live.mjpg") if authorized => {
            LIVE_STREAM_STATE.store(2, Ordering::Release);
            (
                Response::json(200, r#"{"ok":true,"message":"Live stream stopped"}"#),
                false,
            )
        }
        ("POST", "/api/live-test") if authorized => {
            let resolution = match parse_live_test_resolution(&request.body) {
                Ok(resolution) => resolution,
                Err(error) => {
                    return (Response::json(422, &json_error(&error.to_string())), false);
                }
            };
            match open_live_test(profile_path, resolution.0, resolution.1) {
                Ok(message) => (
                    Response::json(
                        200,
                        &serde_json::json!({ "ok": true, "message": message }).to_string(),
                    ),
                    false,
                ),
                Err(error) => (Response::json(500, &json_error(&error.to_string())), false),
            }
        }
        ("POST", "/api/close") if authorized => (
            Response::json(200, r#"{"ok":true,"message":"Editor closed"}"#),
            true,
        ),
        (_, _) if !authorized => (
            Response::json(403, r#"{"ok":false,"error":"Invalid editor token"}"#),
            false,
        ),
        _ => (
            Response::json(404, r#"{"ok":false,"error":"Not found"}"#),
            false,
        ),
    }
}

fn parse_live_test_resolution(body: &[u8]) -> Result<(u32, u32)> {
    let request: serde_json::Value =
        serde_json::from_slice(body).context("live-test request must be valid JSON")?;
    let width = request
        .get("width")
        .and_then(serde_json::Value::as_u64)
        .context("live-test width is required")?;
    let height = request
        .get("height")
        .and_then(serde_json::Value::as_u64)
        .context("live-test height is required")?;
    let resolution = (u32::try_from(width)?, u32::try_from(height)?);
    if !matches!(resolution, (1280, 720) | (1600, 900) | (1920, 1080)) {
        bail!("unsupported live-test resolution {width}x{height}");
    }
    Ok(resolution)
}

fn open_live_test(profile_path: &Path, width: u32, height: u32) -> Result<String> {
    if let Some(owner) = super::launch_v2::active_game_session_owner()? {
        bail!(
            "another Wroid game session is already active ({owner}); stop it with Ctrl+Esc before testing this map"
        );
    }
    let executable = env::current_exe().context("failed to locate the wroid executable")?;
    let command = live_test_command(&executable, profile_path, width, height);
    let terminal = spawn_terminal(&command)?;
    Ok(format!(
        "Live profile test opened in {terminal}. Focus Waydroid and press Ctrl+Esc to stop."
    ))
}

fn live_test_command(
    executable: &Path,
    profile_path: &Path,
    width: u32,
    height: u32,
) -> Vec<OsString> {
    vec![
        executable.as_os_str().to_owned(),
        OsString::from("launch-v2"),
        profile_path.as_os_str().to_owned(),
        OsString::from("--width"),
        OsString::from(width.to_string()),
        OsString::from("--height"),
        OsString::from(height.to_string()),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CalibrationBackgroundState {
    Missing,
    Ready,
    Invalid(String),
}

pub(crate) fn calibration_background_state(profile_path: &Path) -> CalibrationBackgroundState {
    let path = background_path(profile_path);
    match fs::read(&path) {
        Ok(data) if image_content_type(&data).is_some() => CalibrationBackgroundState::Ready,
        Ok(_) => CalibrationBackgroundState::Invalid(format!(
            "saved calibration background is not a valid image: {}",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            CalibrationBackgroundState::Missing
        }
        Err(error) => CalibrationBackgroundState::Invalid(format!(
            "could not read calibration background {}: {error}",
            path.display()
        )),
    }
}

fn background_path(profile_path: &Path) -> PathBuf {
    let parent = profile_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = profile_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("profile.json"));
    let mut background_name = file_name.to_os_string();
    background_name.push(".background");
    parent.join(".wroid-assets").join(background_name)
}

fn profile_backup_path(profile_path: &Path) -> PathBuf {
    let parent = profile_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = profile_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("profile.json"));
    let mut backup_name = file_name.to_os_string();
    backup_name.push(".previous.json");
    parent.join(".wroid-assets").join(backup_name)
}

fn save_profile_with_backup(profile_path: &Path, profile: &ProfileV2) -> Result<bool> {
    let current = ProfileV2::load_from_path(profile_path)
        .with_context(|| format!("failed to read current profile {}", profile_path.display()))?;
    current.validate().map_err(|error| {
        anyhow::anyhow!(
            "current profile cannot be retained before saving: {}",
            error.errors.join("; ")
        )
    })?;
    if current == *profile {
        return Ok(false);
    }

    let backup_path = profile_backup_path(profile_path);
    let directory = backup_path
        .parent()
        .context("profile backup path has no parent")?;
    fs::create_dir_all(directory).with_context(|| {
        format!(
            "failed to create profile asset directory {}",
            directory.display()
        )
    })?;
    current.save_to_path(&backup_path).with_context(|| {
        format!(
            "failed to retain previous profile {}",
            backup_path.display()
        )
    })?;
    profile
        .save_to_path(profile_path)
        .with_context(|| format!("failed to save profile {}", profile_path.display()))?;
    Ok(true)
}

fn image_content_type(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.starts_with(b"BM") && data.len() > 54 {
        Some("image/bmp")
    } else {
        None
    }
}

fn save_background(profile_path: &Path, data: &[u8]) -> io::Result<()> {
    let path = background_path(profile_path);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(".background-{}-{sequence}.tmp", std::process::id()));
    fs::write(&temporary, data)?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(temporary);
        return Err(error);
    }
    Ok(())
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

    fn bytes(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            body,
        }
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
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Content Too Large",
        422 => "Unprocessable Content",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'self'; img-src 'self' blob: data:; style-src 'self'; script-src 'self'; connect-src 'self'\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len(),
    )?;
    stream.write_all(&response.body)
}

fn json_error(message: &str) -> String {
    serde_json::json!({ "ok": false, "error": message }).to_string()
}

/// Grab a full-desktop frame with Spectacle for calibration backgrounds.
///
/// The WebKit live-capture path depends on the xdg-desktop-portal ScreenCast
/// backend, which fails on some Plasma setups ("target not found"); a
/// compositor screenshot is deterministic and needs no portal at all.
fn capture_screen_snapshot() -> Result<Vec<u8>, String> {
    let path = env::temp_dir().join(format!("wroid-editor-snapshot-{}.png", std::process::id()));
    let _ = fs::remove_file(&path);
    eprintln!("[snapshot] running spectacle -> {}", path.display());
    let output = Command::new("spectacle")
        .args(["-b", "-n", "-f", "-o"])
        .arg(&path)
        .output()
        .map_err(|error| {
            let detail = format!("could not run spectacle: {error}");
            eprintln!("[snapshot] {detail}");
            detail
        })?;
    eprintln!(
        "[snapshot] spectacle exit={:?} stderr={:?}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        let detail = format!(
            "spectacle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("[snapshot] {detail}");
        return Err(detail);
    }
    // Spectacle may return while the file is still being flushed.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match fs::read(&path) {
            Ok(bytes) if bytes.starts_with(b"\x89PNG") => {
                let _ = fs::remove_file(&path);
                return Ok(bytes);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("could not read snapshot: {error}")),
        }
        if std::time::Instant::now() >= deadline {
            return Err("spectacle did not produce a screenshot in time".to_owned());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_page_close_stops_the_server_handle() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles/examples/standoff2-v2.json"),
            &path,
        )
        .unwrap();
        let app = start_editor(path, 0).unwrap();
        let address: std::net::SocketAddr = app
            .origin()
            .as_str()
            .strip_prefix("http://")
            .unwrap()
            .parse()
            .unwrap();
        let token_query = app
            .authenticated_url()
            .split_once('?')
            .unwrap()
            .1
            .to_owned();
        let mut client = TcpStream::connect(address).unwrap();
        write!(
            client,
            "POST /api/close?{token_query} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        assert!(response.starts_with("HTTP/1.1 200"));
        app.wait().unwrap();
    }

    #[test]
    fn serves_the_profile_model_asset() {
        let request = Request {
            method: "GET".to_owned(),
            target: "/profile-model.js".to_owned(),
            body: Vec::new(),
        };

        let (response, close) = handle_request(&request, Path::new("/missing"), "secret");

        assert_eq!(response.status, 200);
        assert!(!close);
        assert!(String::from_utf8(response.body)
            .unwrap()
            .contains("WroidProfileModel"));
    }

    #[test]
    fn serves_the_setup_guide_asset() {
        let request = Request {
            method: "GET".to_owned(),
            target: "/setup-guide.js".to_owned(),
            body: Vec::new(),
        };

        let (response, close) = handle_request(&request, Path::new("/missing"), "secret");

        assert_eq!(response.status, 200);
        assert!(!close);
        assert!(String::from_utf8(response.body)
            .unwrap()
            .contains("WroidSetupGuide"));
    }

    #[test]
    fn rejects_requests_without_editor_token() {
        let request = Request {
            method: "GET".to_owned(),
            target: "/api/profile".to_owned(),
            body: Vec::new(),
        };
        let (response, close) = handle_request(&request, Path::new("/missing"), "secret");

        assert_eq!(response.status, 403);
        assert!(!close);
    }

    #[test]
    fn saves_valid_profile_and_rejects_invalid_update() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        let source = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles/examples/pubg-v2.json"),
        )
        .unwrap();
        fs::write(&path, &source).unwrap();

        let request = Request {
            method: "PUT".to_owned(),
            target: "/api/profile?token=secret".to_owned(),
            body: source.as_bytes().to_vec(),
        };
        let (response, _) = handle_request(&request, &path, "secret");
        assert_eq!(response.status, 200);
        let saved = ProfileV2::load_from_path(&path).unwrap();
        saved.validate().unwrap();
        assert!(saved.bindings.iter().any(|binding| matches!(
            binding.action,
            wroid_core::profile_v2::ActionV2::Hold { .. }
        )));

        let mut incompatible: serde_json::Value = serde_json::from_str(&source).unwrap();
        incompatible["bindings"][0]["input"] = serde_json::json!({ "kind": "key", "key": "w" });
        let request = Request {
            method: "PUT".to_owned(),
            target: "/api/profile?token=secret".to_owned(),
            body: serde_json::to_vec(&incompatible).unwrap(),
        };
        let (response, _) = handle_request(&request, &path, "secret");
        assert_eq!(response.status, 422);
        assert!(String::from_utf8(response.body)
            .unwrap()
            .contains("virtual_joystick requires key_cluster"));

        let request = Request {
            method: "PUT".to_owned(),
            target: "/api/profile?token=secret".to_owned(),
            body: br#"{"schema_version":2,"name":"","package_name":"","layers":[],"bindings":[]}"#
                .to_vec(),
        };
        let (response, _) = handle_request(&request, &path, "secret");
        assert_eq!(response.status, 422);
    }

    #[test]
    fn changed_profile_retains_the_previous_valid_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        let source = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles/examples/pubg-v2.json"),
        )
        .unwrap();
        fs::write(&path, &source).unwrap();
        let original = ProfileV2::load_from_path(&path).unwrap();
        let mut changed = original.clone();
        changed.name = "My tuned PUBG map".to_owned();

        assert!(save_profile_with_backup(&path, &changed).unwrap());
        assert_eq!(
            ProfileV2::load_from_path(profile_backup_path(&path)).unwrap(),
            original
        );
        assert_eq!(ProfileV2::load_from_path(&path).unwrap(), changed);

        assert!(!save_profile_with_backup(&path, &changed).unwrap());
        assert_eq!(
            ProfileV2::load_from_path(profile_backup_path(&path)).unwrap(),
            original
        );

        let request = Request {
            method: "GET".to_owned(),
            target: "/api/profile-backup?token=secret".to_owned(),
            body: Vec::new(),
        };
        let (response, _) = handle_request(&request, &path, "secret");
        assert_eq!(response.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["profile"]["name"], original.name);
    }

    #[test]
    fn missing_previous_profile_save_returns_not_found() {
        let directory = tempfile::tempdir().unwrap();
        let request = Request {
            method: "GET".to_owned(),
            target: "/api/profile-backup?token=secret".to_owned(),
            body: Vec::new(),
        };

        let (response, _) =
            handle_request(&request, &directory.path().join("profile.json"), "secret");
        assert_eq!(response.status, 404);
    }

    #[test]
    fn close_endpoint_requests_server_shutdown() {
        let request = Request {
            method: "POST".to_owned(),
            target: "/api/close?token=secret".to_owned(),
            body: Vec::new(),
        };
        let (response, close) = handle_request(&request, Path::new("/missing"), "secret");

        assert_eq!(response.status, 200);
        assert!(close);
    }

    #[test]
    fn background_round_trip_is_stored_beside_profile() {
        let directory = tempfile::tempdir().unwrap();
        let profile_path = directory.path().join("game.json");
        fs::write(&profile_path, "{}").unwrap();
        let png = b"\x89PNG\r\n\x1a\nfake-image".to_vec();
        assert_eq!(
            calibration_background_state(&profile_path),
            CalibrationBackgroundState::Missing
        );

        let put = Request {
            method: "PUT".to_owned(),
            target: "/api/background?token=secret".to_owned(),
            body: png.clone(),
        };
        let (response, _) = handle_request(&put, &profile_path, "secret");
        assert_eq!(response.status, 200);
        assert_eq!(fs::read(background_path(&profile_path)).unwrap(), png);
        assert_eq!(
            calibration_background_state(&profile_path),
            CalibrationBackgroundState::Ready
        );

        let get = Request {
            method: "GET".to_owned(),
            target: "/api/background?token=secret".to_owned(),
            body: Vec::new(),
        };
        let (response, _) = handle_request(&get, &profile_path, "secret");
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "image/png");

        let delete = Request {
            method: "DELETE".to_owned(),
            target: "/api/background?token=secret".to_owned(),
            body: Vec::new(),
        };
        let (response, _) = handle_request(&delete, &profile_path, "secret");
        assert_eq!(response.status, 200);
        assert!(!background_path(&profile_path).exists());
        assert_eq!(
            calibration_background_state(&profile_path),
            CalibrationBackgroundState::Missing
        );
    }

    #[test]
    fn background_rejects_non_image_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let profile_path = directory.path().join("game.json");
        let request = Request {
            method: "PUT".to_owned(),
            target: "/api/background?token=secret".to_owned(),
            body: b"<script>not an image</script>".to_vec(),
        };
        let (response, _) = handle_request(&request, &profile_path, "secret");
        assert_eq!(response.status, 422);
        assert!(!background_path(&profile_path).exists());
    }

    #[test]
    fn invalid_saved_background_is_not_reported_as_calibrated() {
        let directory = tempfile::tempdir().unwrap();
        let profile_path = directory.path().join("game.json");
        let path = background_path(&profile_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not-an-image").unwrap();

        let CalibrationBackgroundState::Invalid(error) =
            calibration_background_state(&profile_path)
        else {
            panic!("invalid calibration asset was accepted");
        };
        assert!(error.contains("not a valid image"));
    }

    #[test]
    fn live_test_uses_the_production_game_session() {
        let command = live_test_command(
            Path::new("/opt/wroid"),
            Path::new("/profiles/pubg-v2.json"),
            1920,
            1080,
        );
        assert_eq!(
            command,
            [
                "/opt/wroid",
                "launch-v2",
                "/profiles/pubg-v2.json",
                "--width",
                "1920",
                "--height",
                "1080",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn live_test_resolution_accepts_only_editor_presets() {
        assert_eq!(
            parse_live_test_resolution(br#"{"width":1280,"height":720}"#).unwrap(),
            (1280, 720)
        );
        assert!(
            parse_live_test_resolution(br#"{"width":1366,"height":768}"#)
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
        assert!(parse_live_test_resolution(br#"{}"#).is_err());
        assert!(parse_live_test_resolution(b"not-json").is_err());
    }
}
