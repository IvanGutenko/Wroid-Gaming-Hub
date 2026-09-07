//! KDE Wayland compositor isolation for Wroid input devices.
//!
//! EVIOCGRAB remains the low-latency input source boundary. KWin can already
//! have an active libinput stream for the same device, though, and on some
//! Plasma systems pointer/button/scroll events continue reaching the desktop.
//! KWin also discovers Wroid's virtual touchscreen before it is passed through
//! to Android; without isolation, every injected Android touch is duplicated
//! on the host desktop. Disabling the exact KWin device closes both leaks. The
//! original state is restored on release and on drop.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::blocking::Connection;

const KWIN_SERVICE: &str = "org.kde.KWin";
const KWIN_INPUT_INTERFACE: &str = "org.kde.KWin.InputDevice";
const DBUS_TIMEOUT: Duration = Duration::from_secs(2);
const DEVICE_DISCOVERY_ATTEMPTS: usize = 40;
const DEVICE_DISCOVERY_INTERVAL: Duration = Duration::from_millis(50);
const RESTORE_WATCHDOG_SCRIPT: &str = r#"
IFS= read -r _ || true
exec /usr/bin/busctl --user --timeout=2s set-property \
  org.kde.KWin "$1" org.kde.KWin.InputDevice enabled b "$2"
"#;

#[derive(Debug, Clone, Copy)]
enum InputCapability {
    Pointer,
    Touchscreen,
}

impl InputCapability {
    const fn property(self) -> &'static str {
        match self {
            Self::Pointer => "pointer",
            Self::Touchscreen => "touch",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Pointer => "pointer",
            Self::Touchscreen => "touchscreen",
        }
    }
}

pub(crate) struct KwinInputIsolation {
    connection: Connection,
    object_path: String,
    sys_name: String,
    capability: InputCapability,
    original_enabled: bool,
    isolated: bool,
    watchdog: Option<IsolationWatchdog>,
}

impl KwinInputIsolation {
    /// Resolve an evdev symlink to KWin's matching InputDevice object.
    ///
    /// Non-KDE desktops return `None`. On KDE, an unusable matching object is
    /// an error: silently pretending capture succeeded would leak input to the
    /// desktop, which is precisely what this guard exists to prevent.
    pub(crate) fn for_pointer_evdev(path: &Path) -> io::Result<Option<Self>> {
        Self::for_evdev(path, InputCapability::Pointer)
    }

    /// Resolve Wroid's Android-only touchscreen to its KWin device.
    pub(crate) fn for_touchscreen_evdev(path: &Path) -> io::Result<Option<Self>> {
        Self::for_evdev(path, InputCapability::Touchscreen)
    }

    fn for_evdev(path: &Path, capability: InputCapability) -> io::Result<Option<Self>> {
        if !is_kde_desktop() {
            return Ok(None);
        }
        let Some(sys_name) = evdev_sys_name(path)? else {
            return Ok(None);
        };
        let connection = Connection::new_session()
            .map_err(|error| io::Error::other(format!("connect to KDE session bus: {error}")))?;
        let object_path = format!("/org/kde/KWin/InputDevice/{sys_name}");
        // udev can publish a fresh uinput node shortly before KWin exposes the
        // corresponding D-Bus object. Wait here, before any touch is emitted.
        let device_has_capability =
            read_device_property_retry(&connection, &object_path, capability, &sys_name)?;
        let proxy = connection.with_proxy(KWIN_SERVICE, object_path.as_str(), DBUS_TIMEOUT);
        let supports_disable: bool = proxy
            .get(KWIN_INPUT_INTERFACE, "supportsDisableEvents")
            .map_err(|error| {
                kwin_error(&sys_name, capability, "inspect disable capability", error)
            })?;
        if !device_has_capability || !supports_disable {
            return Ok(None);
        }
        let original_enabled: bool = proxy
            .get(KWIN_INPUT_INTERFACE, "enabled")
            .map_err(|error| kwin_error(&sys_name, capability, "read enabled state", error))?;
        drop(proxy);

        Ok(Some(Self {
            connection,
            object_path,
            sys_name,
            capability,
            original_enabled,
            isolated: false,
            watchdog: None,
        }))
    }

    pub(crate) fn sys_name(&self) -> &str {
        &self.sys_name
    }

    /// Exclude/include the physical pointer from KWin and verify the result.
    pub(crate) fn set_isolated(&mut self, isolated: bool) -> io::Result<()> {
        if isolated == self.isolated {
            return Ok(());
        }
        let enabled = if isolated {
            false
        } else {
            self.original_enabled
        };
        // Arm a separate process before disabling the compositor device. It
        // owns the read end of a private pipe and restores KWin as soon as the
        // game worker exits, including SIGKILL paths where Drop cannot run.
        if isolated {
            self.watchdog = Some(IsolationWatchdog::spawn(
                &self.object_path,
                self.original_enabled,
            )?);
        }
        let proxy =
            self.connection
                .with_proxy(KWIN_SERVICE, self.object_path.as_str(), DBUS_TIMEOUT);
        let update = proxy
            .set(KWIN_INPUT_INTERFACE, "enabled", enabled)
            .map_err(|error| {
                kwin_error(&self.sys_name, self.capability, "set enabled state", error)
            })
            .and_then(|()| {
                let actual: bool = proxy
                    .get(KWIN_INPUT_INTERFACE, "enabled")
                    .map_err(|error| {
                        kwin_error(
                            &self.sys_name,
                            self.capability,
                            "verify enabled state",
                            error,
                        )
                    })?;
                if actual != enabled {
                    return Err(io::Error::other(format!(
                        "KWin {} {} stayed enabled={actual}, expected {enabled}",
                        self.capability.label(),
                        self.sys_name
                    )));
                }
                Ok(())
            });

        match update {
            Ok(()) => {
                self.isolated = isolated;
                if !isolated {
                    self.stop_watchdog()?;
                }
                Ok(())
            }
            Err(error) => {
                let watchdog_error = self.stop_watchdog().err();
                match watchdog_error {
                    Some(watchdog) => Err(io::Error::other(format!(
                        "{error}; pointer restore watchdog also failed: {watchdog}"
                    ))),
                    None => Err(error),
                }
            }
        }
    }

    fn stop_watchdog(&mut self) -> io::Result<()> {
        self.watchdog
            .take()
            .map(IsolationWatchdog::finish)
            .transpose()
            .map(|_| ())
    }
}

impl Drop for KwinInputIsolation {
    fn drop(&mut self) {
        if self.isolated {
            let _ = self.set_isolated(false);
        }
    }
}

struct IsolationWatchdog {
    child: Option<Child>,
    input: Option<ChildStdin>,
}

impl IsolationWatchdog {
    fn spawn(object_path: &str, original_enabled: bool) -> io::Result<Self> {
        let mut child = Command::new("/usr/bin/bash")
            .args([
                "-c",
                RESTORE_WATCHDOG_SCRIPT,
                "wroid-pointer-restore",
                object_path,
                if original_enabled { "true" } else { "false" },
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("start pointer restore watchdog: {error}"),
                )
            })?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("pointer restore watchdog has no control pipe"))?;
        Ok(Self {
            child: Some(child),
            input: Some(input),
        })
    }

    fn finish(mut self) -> io::Result<()> {
        self.restore_and_wait()
    }

    fn restore_and_wait(&mut self) -> io::Result<()> {
        if let Some(mut input) = self.input.take() {
            let _ = input.write_all(b"restore\n");
        }
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let status = child.wait()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "pointer restore watchdog exited with {status}"
            )))
        }
    }
}

impl Drop for IsolationWatchdog {
    fn drop(&mut self) {
        let _ = self.restore_and_wait();
    }
}

fn is_kde_desktop() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("kde")
}

fn evdev_sys_name(path: &Path) -> io::Result<Option<String>> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        io::Error::new(error.kind(), format!("resolve {}: {error}", path.display()))
    })?;
    let Some(name) = canonical.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let Some(number) = name.strip_prefix("event") else {
        return Ok(None);
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(None);
    }
    Ok(Some(name.to_owned()))
}

fn read_device_property_retry(
    connection: &Connection,
    object_path: &str,
    capability: InputCapability,
    sys_name: &str,
) -> io::Result<bool> {
    let mut last_error = None;
    for attempt in 0..DEVICE_DISCOVERY_ATTEMPTS {
        let proxy = connection.with_proxy(KWIN_SERVICE, object_path, DBUS_TIMEOUT);
        match proxy.get(KWIN_INPUT_INTERFACE, capability.property()) {
            Ok(value) => return Ok(value),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < DEVICE_DISCOVERY_ATTEMPTS {
            std::thread::sleep(DEVICE_DISCOVERY_INTERVAL);
        }
    }
    Err(kwin_error(
        sys_name,
        capability,
        "wait for input-device discovery",
        last_error.expect("at least one discovery attempt"),
    ))
}

fn kwin_error(
    sys_name: &str,
    capability: InputCapability,
    action: &str,
    error: dbus::Error,
) -> io::Error {
    io::Error::other(format!(
        "KWin {} {sys_name}: {action}: {error}",
        capability.label()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn resolves_by_id_symlink_to_kwin_event_name() {
        let directory = tempfile::tempdir().unwrap();
        let event = directory.path().join("event37");
        fs::write(&event, []).unwrap();
        let by_id = directory.path().join("gaming-mouse-event-mouse");
        symlink(&event, &by_id).unwrap();

        assert_eq!(evdev_sys_name(&by_id).unwrap().as_deref(), Some("event37"));
    }

    #[test]
    fn rejects_non_event_device_names() {
        let directory = tempfile::tempdir().unwrap();
        let device = directory.path().join("mouse0");
        fs::write(&device, []).unwrap();

        assert_eq!(evdev_sys_name(&device).unwrap(), None);
    }

    #[test]
    #[ignore = "temporarily toggles a real KDE pointer named by WROID_TEST_MOUSE"]
    fn live_kwin_pointer_isolation_round_trip() {
        let path = std::env::var_os("WROID_TEST_MOUSE").expect("set WROID_TEST_MOUSE");
        let mut isolation = KwinInputIsolation::for_pointer_evdev(Path::new(&path))
            .unwrap()
            .expect("KWin pointer isolation unavailable");
        assert!(isolation.original_enabled);
        isolation.set_isolated(true).unwrap();
        assert!(isolation.isolated);
        isolation.set_isolated(false).unwrap();
        assert!(!isolation.isolated);
    }
}
