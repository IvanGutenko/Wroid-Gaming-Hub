//! Portal-free desktop frame capture through the KWin ScreenShot2 D-Bus API.
//!
//! The xdg-desktop-portal ScreenCast backend can fail on Plasma setups
//! (PipeWire "target not found"), which leaves the editor's live alignment
//! with a stream that never delivers frames. KWin's ScreenShot2 interface
//! does not depend on PipeWire at all: it returns raw BGRA pixels on a file
//! descriptor we provide.
//!
//! Authorization model: KWin allows ScreenShot2 callers whose executable has
//! a `.desktop` entry declaring `X-KDE-DBUS-Restricted-Interfaces=
//! org.kde.KWin.ScreenShot2`, matched by `Exec=` against `/proc/<pid>/exe`.
//! This module maintains that entry for the running wroid executable.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use dbus::arg::{OwnedFd, RefArg, Variant};
use dbus::blocking::{BlockingSender, SyncConnection};

const DBUS_TIMEOUT: Duration = Duration::from_secs(8);
const PIXEL_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// QImage::Format_ARGB32_Premultiplied: 4-byte BGRA in memory on LE hosts.
const FORMAT_ARGB32_PREMULTIPLIED: u64 = 6;

#[derive(Debug)]
pub(crate) struct ScreenFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per source row; may exceed `width * 4`.
    pub stride: usize,
    /// Packed BGRA rows (`stride` bytes per row, `height` rows).
    pub data: Vec<u8>,
}

pub(crate) fn is_kde_wayland() -> bool {
    std::env::var("KDE_SESSION_VERSION").as_deref() == Ok("6")
        && std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
}

fn executable_path() -> Result<PathBuf> {
    fs::read_link("/proc/self/exe").context("failed to resolve the wroid executable path")
}

fn applications_dir() -> Result<PathBuf> {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        if !data_home.is_empty() {
            return Ok(PathBuf::from(data_home).join("applications"));
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    Ok(home.join(".local/share/applications"))
}

static AUTHORIZATION: OnceLock<Result<(), String>> = OnceLock::new();

/// Make sure a desktop entry authorizes this executable for ScreenShot2.
///
/// KWin resolves the calling process through `/proc/<pid>/exe`, so the entry
/// must carry the resolved binary path in `Exec=`. The entry is rewritten
/// whenever the executable path changes (dev builds vs installed builds).
fn ensure_authorization() -> Result<()> {
    let result =
        AUTHORIZATION.get_or_init(|| ensure_authorization_uncached().map_err(|e| e.to_string()));
    match result {
        Ok(()) => Ok(()),
        Err(message) => bail!("ScreenShot2 authorization failed: {message}"),
    }
}

fn ensure_authorization_uncached() -> Result<()> {
    let executable = executable_path()?;
    let directory = applications_dir()?;
    fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let entry_path = directory.join("wroid-screen-capture.desktop");
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Wroid Screen Capture\n\
         NoDisplay=true\n\
         Exec={}\n\
         X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2\n",
        executable.display()
    );
    let stale = fs::read_to_string(&entry_path).is_ok_and(|existing| existing != contents);
    if stale || !entry_path.is_file() {
        fs::write(&entry_path, contents)
            .with_context(|| format!("failed to write {}", entry_path.display()))?;
        refresh_sycoca();
    }
    Ok(())
}

fn refresh_sycoca() {
    for program in ["kbuildsycoca6", "kbuildsycoca5"] {
        if Command::new(program)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
    }
    // Best-effort: KApplicationTrader sometimes picks up new entries even
    // without an explicit cache rebuild.
}

/// The output whose current mode has the highest refresh rate.
///
/// `kscreen-doctor -o` prints blocks like:
/// `Output: 2 DP-2 <uuid>` followed by
/// `Modes: 30:1920x1080@239.76* 31:...` where `*` marks the current mode.
fn highest_refresh_screen() -> Option<String> {
    let output = Command::new("kscreen-doctor")
        .arg("-o")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut best: Option<(f64, String)> = None;
    let mut current_name: Option<String> = None;
    for raw_line in text.lines() {
        // Drop ANSI escapes and control bytes, keep spaces and punctuation.
        let line: String = raw_line
            .chars()
            .filter(|c| c.is_ascii_graphic() || *c == ' ')
            .collect();
        if let Some(rest) = line.strip_prefix("Output:") {
            current_name = rest.split_whitespace().nth(1).map(str::to_owned);
        } else if line.starts_with("Modes:") {
            let Some(name) = current_name.clone() else {
                continue;
            };
            if let Some(refresh) = current_mode_refresh(&line) {
                if best
                    .as_ref()
                    .is_none_or(|(best_refresh, _)| refresh > *best_refresh)
                {
                    best = Some((refresh, name));
                }
            }
        }
    }
    best.map(|(_, name)| name)
}

/// The refresh rate of the current (`*`-marked) mode in a Modes: line.
fn current_mode_refresh(line: &str) -> Option<f64> {
    for token in line.split_whitespace() {
        let Some(mode) = token.strip_suffix('*') else {
            continue;
        };
        let refresh = mode.split('@').nth(1)?;
        return refresh.parse::<f64>().ok();
    }
    None
}

struct Pipe {
    read: RawFd,
    write: RawFd,
}

fn create_pipe() -> io::Result<Pipe> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element buffer for pipe(2).
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let pipe = Pipe {
        read: fds[0],
        write: fds[1],
    };
    for fd in [pipe.read, pipe.write] {
        // SAFETY: both descriptors are owned by this pipe.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(pipe.read);
                libc::close(pipe.write);
            }
            return Err(error);
        }
    }
    Ok(pipe)
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: both descriptors are owned until dropped here.
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

type Metadata = HashMap<String, Variant<Box<dyn RefArg>>>;

fn metadata_value(metadata: &Metadata, key: &str) -> Result<u64> {
    metadata
        .get(key)
        .and_then(RefArg::as_u64)
        .ok_or_else(|| anyhow!("KWin screenshot metadata lacks integer {key}"))
}

/// Grab one raw frame of the active screen.
pub(crate) fn capture_active_screen() -> Result<ScreenFrame> {
    capture_frame("CaptureActiveScreen", None)
}

/// Grab one raw frame of a specific output (e.g. the gaming monitor).
pub(crate) fn capture_screen_named(name: &str) -> Result<ScreenFrame> {
    capture_frame("CaptureScreen", Some(name))
}

/// Grab one raw frame of the snapshot target: the highest-refresh output
/// (the gaming monitor), or the active screen when that cannot be resolved.
pub(crate) fn capture_primary_screen() -> Result<ScreenFrame> {
    match target_screen_name() {
        Some(name) => capture_screen_named(&name),
        None => capture_active_screen(),
    }
}

/// The output snapshots should capture.
///
/// `WROID_SNAPSHOT_OUTPUT` wins when set; otherwise the output whose current
/// mode has the highest refresh rate is the gaming monitor in practice.
pub(crate) fn target_screen_name() -> Option<String> {
    if let Ok(name) = std::env::var("WROID_SNAPSHOT_OUTPUT") {
        if !name.trim().is_empty() {
            return Some(name.trim().to_owned());
        }
    }
    highest_refresh_screen()
}

fn capture_frame(method: &str, screen_name: Option<&str>) -> Result<ScreenFrame> {
    ensure_authorization()?;
    let connection =
        SyncConnection::new_session().context("failed to connect to the session D-Bus")?;
    let pipe = create_pipe().context("failed to create the pixel pipe")?;

    let member: dbus::strings::Member = method.into();
    let message = dbus::Message::method_call(
        &"org.kde.KWin".into(),
        &"/org/kde/KWin/ScreenShot2".into(),
        &"org.kde.KWin.ScreenShot2".into(),
        &member,
    );
    let mut options: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
    options.insert("include-cursor".to_owned(), Variant(Box::new(true)));
    // The transfer descriptor is a duplicate: `OwnedFd` closes it on drop
    // while the pipe keeps ownership of its own end exactly once.
    let transfer_fd = unsafe { OwnedFd::from_raw_fd(libc::dup(pipe.write)) };
    let message = match screen_name {
        Some(name) => message.append3(name, options, transfer_fd),
        None => message.append2(options, transfer_fd),
    };

    let reply = connection
        .send_with_reply_and_block(message, DBUS_TIMEOUT)
        .map_err(|error| anyhow!("KWin rejected the screen capture: {error}"))?;

    let metadata: Metadata = reply
        .read1()
        .map_err(|error| anyhow!("unexpected ScreenShot2 reply: {error}"))?;
    let width = metadata_value(&metadata, "width")? as u32;
    let height = metadata_value(&metadata, "height")? as u32;
    let stride = metadata_value(&metadata, "stride")? as usize;
    let format = metadata_value(&metadata, "format")?;
    if !(width > 0 && height > 0 && stride >= width as usize * 4) {
        bail!(
            "KWin reported an impossible frame: {}x{}, stride {stride}",
            width,
            height
        );
    }
    if format != FORMAT_ARGB32_PREMULTIPLIED {
        bail!("unsupported KWin frame format {format} (expected BGRA)");
    }

    let expected = stride
        .checked_mul(height as usize)
        .context("frame overflows")?;
    let data = read_exact_from_fd(pipe.read, expected)?;
    Ok(ScreenFrame {
        width,
        height,
        stride,
        data,
    })
}

fn read_exact_from_fd(fd: RawFd, expected: usize) -> Result<Vec<u8>> {
    let deadline = Instant::now() + PIXEL_READ_TIMEOUT;
    let mut data = vec![0_u8; expected];
    let mut filled = 0;
    while filled < expected {
        let mut poll_fds = [libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        let timeout_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(i64::MAX as u128) as libc::c_int;
        // SAFETY: `poll_fds` is a valid one-element array for poll(2).
        let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), 1, timeout_ms) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(anyhow!("pixel pipe poll failed: {error}"));
        }
        if ready == 0 {
            bail!("timed out waiting for {} frame bytes", expected - filled);
        }
        // SAFETY: `fd` is valid and readable per poll; the slice covers only
        // the unfilled tail of `data`.
        let read = unsafe { libc::read(fd, data[filled..].as_mut_ptr().cast(), expected - filled) };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(anyhow!("pixel pipe read failed: {error}"));
        }
        if read == 0 {
            bail!("KWin closed the pixel pipe early ({filled}/{expected} bytes)");
        }
        filled += read as usize;
    }
    Ok(data)
}

/// Encode a captured frame as an uncompressed 32-bit top-down BMP.
pub(crate) fn frame_to_bmp(frame: &ScreenFrame) -> Vec<u8> {
    let row_bytes = frame.width as usize * 4;
    let pixel_bytes = row_bytes * frame.height as usize;
    let header_size = 14 + 40;
    let file_size = header_size + pixel_bytes;

    let mut bmp = Vec::with_capacity(file_size);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
    bmp.extend_from_slice(&0_u32.to_le_bytes());
    bmp.extend_from_slice(&(header_size as u32).to_le_bytes());

    // BITMAPINFOHEADER; negative height selects top-down rows.
    bmp.extend_from_slice(&40_u32.to_le_bytes());
    bmp.extend_from_slice(&(frame.width as i32).to_le_bytes());
    bmp.extend_from_slice(&(-(frame.height as i32)).to_le_bytes());
    bmp.extend_from_slice(&1_u16.to_le_bytes());
    bmp.extend_from_slice(&32_u16.to_le_bytes());
    bmp.extend_from_slice(&0_u32.to_le_bytes()); // BI_RGB
    bmp.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
    bmp.extend_from_slice(&2835_u32.to_le_bytes());
    bmp.extend_from_slice(&2835_u32.to_le_bytes());
    bmp.extend_from_slice(&0_u32.to_le_bytes());
    bmp.extend_from_slice(&0_u32.to_le_bytes());

    for row in 0..frame.height as usize {
        let start = row * frame.stride;
        bmp.extend_from_slice(&frame.data[start..start + row_bytes]);
    }
    debug_assert_eq!(bmp.len(), file_size);
    bmp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_to_bmp_encodes_top_down_bgra_rows() {
        let frame = ScreenFrame {
            width: 2,
            height: 2,
            stride: 12,
            data: vec![
                1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 0, 7, 8, 9, 255, 10, 11, 12, 255, 0, 0, 0, 0,
            ],
        };

        let bmp = frame_to_bmp(&frame);

        assert_eq!(bmp.len(), 54 + 2 * 2 * 4);
        assert_eq!(&bmp[..2], b"BM");
        assert_eq!(
            u32::from_le_bytes(bmp[2..6].try_into().unwrap()),
            bmp.len() as u32
        );
        assert_eq!(u32::from_le_bytes(bmp[10..14].try_into().unwrap()), 54);
        assert_eq!(u32::from_le_bytes(bmp[14..18].try_into().unwrap()), 40);
        let width = i32::from_le_bytes(bmp[18..22].try_into().unwrap());
        let height = i32::from_le_bytes(bmp[22..26].try_into().unwrap());
        assert_eq!(width, 2);
        assert_eq!(height, -2);
        let planes = u16::from_le_bytes(bmp[26..28].try_into().unwrap());
        let bits = u16::from_le_bytes(bmp[28..30].try_into().unwrap());
        assert_eq!(planes, 1);
        assert_eq!(bits, 32);
        assert_eq!(&bmp[54..58], &[1, 2, 3, 255]);
        assert_eq!(&bmp[62..66], &[7, 8, 9, 255]);
    }

    #[test]
    fn kde_wayland_detection_requires_version_and_display() {
        // SAFETY: single-threaded test body touching test-only variables.
        unsafe {
            std::env::set_var("KDE_SESSION_VERSION", "6");
            std::env::remove_var("WAYLAND_DISPLAY");
            assert!(!is_kde_wayland());
            std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
            assert!(is_kde_wayland());
            std::env::set_var("KDE_SESSION_VERSION", "5");
            assert!(!is_kde_wayland());
            std::env::remove_var("KDE_SESSION_VERSION");
            std::env::remove_var("WAYLAND_DISPLAY");
        }
    }

    #[test]
    fn current_mode_refresh_reads_the_star_marked_mode() {
        let line = "Modes: 1:1280x720@60.00! 2:1920x1080@60.00* 3:1920x1080@59.94";
        assert_eq!(current_mode_refresh(line), Some(60.00));
        let gaming = "Modes: 29:1920x1080@60.00! 30:1920x1080@239.76* 31:1920x1080@119.98";
        assert_eq!(current_mode_refresh(gaming), Some(239.76));
        assert_eq!(current_mode_refresh("Modes: 1:800x600@60"), None);
    }

    #[test]
    fn highest_refresh_screen_picks_the_gaming_monitor() {
        let sample = "\
Output: 1 HDMI-A-4 d0f552a1-c742-41fc-b3e1-300b444cfa7e
\tenabled
\tModes: 1:1280x720@60.00! 2:1920x1080@60.00* 3:1920x1080@59.94
\tGeometry: 1920,0 1920x1080
Output: 2 DP-2 cd62d27e-6cfb-4ee4-bc4c-5e0e26892c22
\tenabled
\tModes: 29:1920x1080@60.00! 30:1920x1080@239.76* 31:1920x1080@119.98
\tGeometry: 0,0 1920x1080";
        // Mirror the parsing loop over the sample lines.
        let mut best: Option<(f64, String)> = None;
        let mut current_name: Option<String> = None;
        for raw_line in sample.lines() {
            let line: String = raw_line
                .chars()
                .filter(|c| c.is_ascii_graphic() || *c == ' ')
                .collect();
            if let Some(rest) = line.strip_prefix("Output:") {
                current_name = rest.split_whitespace().nth(1).map(str::to_owned);
            } else if line.starts_with("Modes:") {
                let Some(name) = current_name.clone() else {
                    continue;
                };
                if let Some(refresh) = current_mode_refresh(&line) {
                    if best
                        .as_ref()
                        .is_none_or(|(best_refresh, _)| refresh > *best_refresh)
                    {
                        best = Some((refresh, name));
                    }
                }
            }
        }
        assert_eq!(best.map(|(_, name)| name).as_deref(), Some("DP-2"));
    }
}
