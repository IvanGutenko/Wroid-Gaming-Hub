//! Low-latency host input capture and normalization.
//!
//! This crate owns physical input device access and converts Linux evdev
//! events into backend-independent runtime actions. It does not know about
//! Waydroid lifecycle, Android packages, GUI toolkits, or profile storage.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use evdev::{Device, EventSummary, InputEvent, KeyCode};
use thiserror::Error;
use wroid_runtime::DirectionalInput;

pub mod mouse;

const INPUT_BY_ID: &str = "/dev/input/by-id";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceInfo {
    pub path: PathBuf,
    pub name: String,
}

const REQUIRED_KEYS: [(KeyCode, &str); 6] = [
    (KeyCode::KEY_W, "W"),
    (KeyCode::KEY_A, "A"),
    (KeyCode::KEY_S, "S"),
    (KeyCode::KEY_D, "D"),
    (KeyCode::KEY_ESC, "Esc"),
    (KeyCode::KEY_F12, "F12"),
];

/// Logical movement keys used by the first keyboard-to-joystick runtime slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovementKey {
    Up,
    Left,
    Down,
    Right,
    Exit,
}

/// Profile-visible host keys supported by the current keyboard capture backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostKey {
    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
    A,
    B,
    C,
    W,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    X,
    Y,
    Z,
    Space,
    Tab,
    LeftShift,
    LeftCtrl,
    LeftAlt,
    ArrowUp,
    ArrowLeft,
    ArrowDown,
    ArrowRight,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Esc,
}

impl HostKey {
    pub const fn profile_name(self) -> &'static str {
        match self {
            Self::Num0 => "0",
            Self::Num1 => "1",
            Self::Num2 => "2",
            Self::Num3 => "3",
            Self::Num4 => "4",
            Self::Num5 => "5",
            Self::Num6 => "6",
            Self::Num7 => "7",
            Self::Num8 => "8",
            Self::Num9 => "9",
            Self::A => "a",
            Self::B => "b",
            Self::C => "c",
            Self::D => "d",
            Self::E => "e",
            Self::F => "f",
            Self::G => "g",
            Self::H => "h",
            Self::I => "i",
            Self::J => "j",
            Self::K => "k",
            Self::L => "l",
            Self::M => "m",
            Self::N => "n",
            Self::O => "o",
            Self::P => "p",
            Self::Q => "q",
            Self::R => "r",
            Self::S => "s",
            Self::T => "t",
            Self::U => "u",
            Self::V => "v",
            Self::W => "w",
            Self::X => "x",
            Self::Y => "y",
            Self::Z => "z",
            Self::Space => "space",
            Self::Tab => "tab",
            Self::LeftShift => "shift",
            Self::LeftCtrl => "ctrl",
            Self::LeftAlt => "alt",
            Self::ArrowUp => "up",
            Self::ArrowLeft => "left",
            Self::ArrowDown => "down",
            Self::ArrowRight => "right",
            Self::F1 => "f1",
            Self::F2 => "f2",
            Self::F3 => "f3",
            Self::F4 => "f4",
            Self::F5 => "f5",
            Self::F6 => "f6",
            Self::F7 => "f7",
            Self::F8 => "f8",
            Self::F9 => "f9",
            Self::F10 => "f10",
            Self::F11 => "f11",
            Self::F12 => "f12",
            Self::Esc => "esc",
        }
    }

    pub const fn movement_key(self) -> Option<MovementKey> {
        match self {
            Self::W => Some(MovementKey::Up),
            Self::A => Some(MovementKey::Left),
            Self::S => Some(MovementKey::Down),
            Self::D => Some(MovementKey::Right),
            Self::Esc => Some(MovementKey::Exit),
            _ => None,
        }
    }
}

/// Linux key lifecycle after evdev values have been normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTransition {
    Pressed,
    Released,
    Repeated,
}

/// One profile-visible physical keyboard event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostKeyEvent {
    pub key: HostKey,
    pub transition: KeyTransition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyEventBatch {
    pub events: Vec<HostKeyEvent>,
    pub kernel_timestamp: Option<SystemTime>,
}

impl HostKeyEvent {
    pub const fn new(key: HostKey, transition: KeyTransition) -> Self {
        Self { key, transition }
    }

    pub fn movement_event(self) -> Option<KeyboardEvent> {
        self.key
            .movement_key()
            .map(|key| KeyboardEvent::new(key, self.transition))
    }
}

/// One movement-specific keyboard event retained for compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardEvent {
    pub key: MovementKey,
    pub transition: KeyTransition,
}

impl KeyboardEvent {
    pub const fn new(key: MovementKey, transition: KeyTransition) -> Self {
        Self { key, transition }
    }
}

/// Result of applying a keyboard event to the movement state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardAction {
    DirectionChanged(DirectionalInput),
    ExitRequested,
    Ignored,
}

/// Stateful WASD normalizer.
///
/// Duplicate presses, duplicate releases, and kernel auto-repeat events do not
/// create runtime updates. Opposing keys remain represented independently; the
/// virtual joystick runtime decides that opposing directions cancel per axis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirectionalKeyState {
    current: DirectionalInput,
}

impl DirectionalKeyState {
    pub const fn current(&self) -> DirectionalInput {
        self.current
    }

    pub fn apply(&mut self, event: KeyboardEvent) -> KeyboardAction {
        if event.key == MovementKey::Exit {
            return match event.transition {
                KeyTransition::Pressed => KeyboardAction::ExitRequested,
                KeyTransition::Released | KeyTransition::Repeated => KeyboardAction::Ignored,
            };
        }

        let pressed = match event.transition {
            KeyTransition::Pressed => true,
            KeyTransition::Released => false,
            KeyTransition::Repeated => return KeyboardAction::Ignored,
        };

        let target = match event.key {
            MovementKey::Up => &mut self.current.up,
            MovementKey::Left => &mut self.current.left,
            MovementKey::Down => &mut self.current.down,
            MovementKey::Right => &mut self.current.right,
            MovementKey::Exit => unreachable!("exit was handled above"),
        };

        if *target == pressed {
            return KeyboardAction::Ignored;
        }

        *target = pressed;
        KeyboardAction::DirectionChanged(self.current)
    }

    /// Releases every logical movement key.
    ///
    /// Returns `Some(neutral)` when state changed, allowing callers to issue one
    /// final joystick release during focus loss or controlled shutdown.
    pub fn release_all(&mut self) -> Option<DirectionalInput> {
        if self.current == DirectionalInput::default() {
            return None;
        }

        self.current = DirectionalInput::default();
        Some(self.current)
    }
}

#[derive(Debug, Error)]
pub enum KeyboardDeviceError {
    #[error("failed to discover {kind} devices under /dev/input/by-id: {source}")]
    Discovery {
        kind: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("no usable {kind} device found under {directory}; pass an explicit device path")]
    NoMatchingDevice {
        kind: &'static str,
        directory: PathBuf,
    },
    #[error("failed to open keyboard device {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("input device {path} does not report keyboard keys")]
    MissingKeyCapabilities { path: PathBuf },
    #[error("input device {path} is missing required keys: {keys}")]
    MissingRequiredKeys { path: PathBuf, keys: String },
    #[error("failed to grab keyboard device {path}: {source}")]
    Grab {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to release keyboard device {path}: {source}")]
    Ungrab {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to configure keyboard device {path}: {source}")]
    Configure {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read keyboard device {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl KeyboardDeviceError {
    pub fn is_would_block(&self) -> bool {
        matches!(self, Self::Read { source, .. } if source.kind() == io::ErrorKind::WouldBlock)
    }
}

/// Blocking evdev keyboard reader with optional exclusive grab.
///
/// Exclusive grab is intentionally opt-in because it prevents the compositor,
/// terminal, and other clients from receiving events from the selected device.
/// Closing the file descriptor releases the kernel grab even after abnormal
/// process termination.
pub struct EvdevKeyboard {
    device: Device,
    path: PathBuf,
    name: String,
    grabbed: bool,
}

impl EvdevKeyboard {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, KeyboardDeviceError> {
        let path = path.as_ref().to_path_buf();
        let device = Device::open(&path).map_err(|source| KeyboardDeviceError::Open {
            path: path.clone(),
            source,
        })?;
        let name = device.name().unwrap_or("Unnamed evdev device").to_owned();

        let supported = device
            .supported_keys()
            .ok_or_else(|| KeyboardDeviceError::MissingKeyCapabilities { path: path.clone() })?;
        let missing = REQUIRED_KEYS
            .iter()
            .filter_map(|(code, label)| (!supported.contains(*code)).then_some(*label))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(KeyboardDeviceError::MissingRequiredKeys {
                path,
                keys: missing.join(", "),
            });
        }

        Ok(Self {
            device,
            path,
            name,
            grabbed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn is_grabbed(&self) -> bool {
        self.grabbed
    }

    pub fn set_nonblocking(&mut self, nonblocking: bool) -> Result<(), KeyboardDeviceError> {
        self.device
            .set_nonblocking(nonblocking)
            .map_err(|source| KeyboardDeviceError::Configure {
                path: self.path.clone(),
                source,
            })
    }

    /// Raw evdev descriptor, exposed so a reader can block in `poll` until the
    /// kernel has an event instead of waking on a fixed timer.
    pub fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;

        self.device.as_raw_fd()
    }

    pub fn grab(&mut self) -> Result<(), KeyboardDeviceError> {
        if self.grabbed {
            return Ok(());
        }

        self.device
            .grab()
            .map_err(|source| KeyboardDeviceError::Grab {
                path: self.path.clone(),
                source,
            })?;
        self.grabbed = true;
        Ok(())
    }

    pub fn ungrab(&mut self) -> Result<(), KeyboardDeviceError> {
        if !self.grabbed {
            return Ok(());
        }

        self.device
            .ungrab()
            .map_err(|source| KeyboardDeviceError::Ungrab {
                path: self.path.clone(),
                source,
            })?;
        self.grabbed = false;
        Ok(())
    }

    /// Blocks until at least one kernel event is available and returns only the
    /// profile-visible key events supported by the current runtime slice unless
    /// the device was configured as nonblocking by the caller.
    pub fn next_host_key_events(&mut self) -> Result<Vec<HostKeyEvent>, KeyboardDeviceError> {
        Ok(self.next_host_key_batch()?.events)
    }

    /// Returns normalized key events with the oldest matching evdev timestamp.
    pub fn next_host_key_batch(&mut self) -> Result<HostKeyEventBatch, KeyboardDeviceError> {
        let events = self
            .device
            .fetch_events()
            .map_err(|source| KeyboardDeviceError::Read {
                path: self.path.clone(),
                source,
            })?;
        let mut normalized = Vec::new();
        let mut kernel_timestamp = None;
        for event in events {
            let timestamp = event.timestamp();
            if let Some(event) = normalize_host_input_event(event) {
                kernel_timestamp = oldest_timestamp(kernel_timestamp, timestamp);
                normalized.push(event);
            }
        }
        Ok(HostKeyEventBatch {
            events: normalized,
            kernel_timestamp,
        })
    }

    /// Blocks until at least one kernel event is available and returns only the
    /// movement events relevant to the current runtime slice unless the device
    /// was configured as nonblocking by the caller.
    pub fn next_events(&mut self) -> Result<Vec<KeyboardEvent>, KeyboardDeviceError> {
        let events = self
            .device
            .fetch_events()
            .map_err(|source| KeyboardDeviceError::Read {
                path: self.path.clone(),
                source,
            })?;
        Ok(events.filter_map(normalize_input_event).collect())
    }
}

fn oldest_timestamp(current: Option<SystemTime>, next: SystemTime) -> Option<SystemTime> {
    Some(current.map_or(next, |current| current.min(next)))
}

impl Drop for EvdevKeyboard {
    fn drop(&mut self) {
        if self.grabbed {
            let _ = self.device.ungrab();
        }
    }
}

pub fn discover_keyboard_path() -> Result<PathBuf, KeyboardDeviceError> {
    Ok(discover_keyboard_devices()?
        .into_iter()
        .next()
        .expect("device discovery returns an error when no keyboard matches")
        .path)
}

pub fn discover_keyboard_devices() -> Result<Vec<InputDeviceInfo>, KeyboardDeviceError> {
    let candidates =
        by_id_candidates("-event-kbd").map_err(|source| KeyboardDeviceError::Discovery {
            kind: "keyboard",
            source,
        })?;
    let mut last_error = None;
    let mut matches = Vec::new();
    for path in candidates {
        match EvdevKeyboard::open(&path) {
            Ok(device) => matches.push((
                device_preference(device.name(), &path, "keyboard"),
                InputDeviceInfo {
                    path,
                    name: device.name().to_owned(),
                },
            )),
            Err(error) => last_error = Some(error),
        }
    }
    matches.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.path.cmp(&right.1.path))
    });
    if !matches.is_empty() {
        return Ok(matches.into_iter().map(|(_, device)| device).collect());
    }
    Err(
        last_error.unwrap_or_else(|| KeyboardDeviceError::NoMatchingDevice {
            kind: "keyboard",
            directory: PathBuf::from(INPUT_BY_ID),
        }),
    )
}

pub fn discover_mouse_path() -> Result<PathBuf, mouse::MouseDeviceError> {
    Ok(discover_mouse_devices()?
        .into_iter()
        .next()
        .expect("device discovery returns an error when no mouse matches")
        .path)
}

pub fn discover_mouse_devices() -> Result<Vec<InputDeviceInfo>, mouse::MouseDeviceError> {
    let candidates =
        by_id_candidates("-event-mouse").map_err(|source| mouse::MouseDeviceError::Discovery {
            kind: "mouse",
            source,
        })?;
    let mut last_error = None;
    let mut matches = Vec::new();
    for path in candidates {
        match mouse::EvdevMouse::open(&path) {
            Ok(device) => matches.push((
                device_preference(device.name(), &path, "mouse"),
                InputDeviceInfo {
                    path,
                    name: device.name().to_owned(),
                },
            )),
            Err(error) => last_error = Some(error),
        }
    }
    matches.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.path.cmp(&right.1.path))
    });
    if !matches.is_empty() {
        return Ok(matches.into_iter().map(|(_, device)| device).collect());
    }
    Err(
        last_error.unwrap_or_else(|| mouse::MouseDeviceError::NoMatchingDevice {
            kind: "mouse",
            directory: PathBuf::from(INPUT_BY_ID),
        }),
    )
}

fn by_id_candidates(suffix: &str) -> io::Result<Vec<PathBuf>> {
    let mut candidates = fs::read_dir(INPUT_BY_ID)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(suffix))
                .then_some(entry.path())
        })
        .collect::<Vec<_>>();
    candidates.sort();
    Ok(candidates)
}

fn device_preference(device_name: &str, path: &Path, expected_kind: &str) -> u8 {
    let device_name = device_name.to_ascii_lowercase();
    let path = path.to_string_lossy().to_ascii_lowercase();
    let other_kind = if expected_kind == "keyboard" {
        "mouse"
    } else {
        "keyboard"
    };
    u8::from(device_name.contains(expected_kind)) * 4
        + u8::from(!device_name.contains(other_kind)) * 2
        + u8::from(path.contains(expected_kind))
}

fn normalize_input_event(event: InputEvent) -> Option<KeyboardEvent> {
    normalize_host_input_event(event).and_then(HostKeyEvent::movement_event)
}

fn normalize_host_input_event(event: InputEvent) -> Option<HostKeyEvent> {
    let EventSummary::Key(_, code, value) = event.destructure() else {
        return None;
    };

    let key = match code {
        KeyCode::KEY_0 => HostKey::Num0,
        KeyCode::KEY_1 => HostKey::Num1,
        KeyCode::KEY_2 => HostKey::Num2,
        KeyCode::KEY_3 => HostKey::Num3,
        KeyCode::KEY_4 => HostKey::Num4,
        KeyCode::KEY_5 => HostKey::Num5,
        KeyCode::KEY_6 => HostKey::Num6,
        KeyCode::KEY_7 => HostKey::Num7,
        KeyCode::KEY_8 => HostKey::Num8,
        KeyCode::KEY_9 => HostKey::Num9,
        KeyCode::KEY_A => HostKey::A,
        KeyCode::KEY_B => HostKey::B,
        KeyCode::KEY_C => HostKey::C,
        KeyCode::KEY_D => HostKey::D,
        KeyCode::KEY_E => HostKey::E,
        KeyCode::KEY_F => HostKey::F,
        KeyCode::KEY_G => HostKey::G,
        KeyCode::KEY_H => HostKey::H,
        KeyCode::KEY_I => HostKey::I,
        KeyCode::KEY_J => HostKey::J,
        KeyCode::KEY_K => HostKey::K,
        KeyCode::KEY_L => HostKey::L,
        KeyCode::KEY_M => HostKey::M,
        KeyCode::KEY_N => HostKey::N,
        KeyCode::KEY_O => HostKey::O,
        KeyCode::KEY_P => HostKey::P,
        KeyCode::KEY_Q => HostKey::Q,
        KeyCode::KEY_R => HostKey::R,
        KeyCode::KEY_S => HostKey::S,
        KeyCode::KEY_T => HostKey::T,
        KeyCode::KEY_U => HostKey::U,
        KeyCode::KEY_V => HostKey::V,
        KeyCode::KEY_W => HostKey::W,
        KeyCode::KEY_X => HostKey::X,
        KeyCode::KEY_Y => HostKey::Y,
        KeyCode::KEY_Z => HostKey::Z,
        KeyCode::KEY_SPACE => HostKey::Space,
        KeyCode::KEY_TAB => HostKey::Tab,
        KeyCode::KEY_LEFTSHIFT => HostKey::LeftShift,
        KeyCode::KEY_LEFTCTRL => HostKey::LeftCtrl,
        KeyCode::KEY_LEFTALT => HostKey::LeftAlt,
        KeyCode::KEY_UP => HostKey::ArrowUp,
        KeyCode::KEY_LEFT => HostKey::ArrowLeft,
        KeyCode::KEY_DOWN => HostKey::ArrowDown,
        KeyCode::KEY_RIGHT => HostKey::ArrowRight,
        KeyCode::KEY_F1 => HostKey::F1,
        KeyCode::KEY_F2 => HostKey::F2,
        KeyCode::KEY_F3 => HostKey::F3,
        KeyCode::KEY_F4 => HostKey::F4,
        KeyCode::KEY_F5 => HostKey::F5,
        KeyCode::KEY_F6 => HostKey::F6,
        KeyCode::KEY_F7 => HostKey::F7,
        KeyCode::KEY_F8 => HostKey::F8,
        KeyCode::KEY_F9 => HostKey::F9,
        KeyCode::KEY_F10 => HostKey::F10,
        KeyCode::KEY_F11 => HostKey::F11,
        KeyCode::KEY_F12 => HostKey::F12,
        KeyCode::KEY_ESC => HostKey::Esc,
        _ => return None,
    };
    let transition = match value {
        0 => KeyTransition::Released,
        1 => KeyTransition::Pressed,
        2 => KeyTransition::Repeated,
        _ => return None,
    };

    Some(HostKeyEvent::new(key, transition))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(key: MovementKey, transition: KeyTransition) -> KeyboardEvent {
        KeyboardEvent::new(key, transition)
    }

    #[test]
    fn profile_key_names_match_profile_schema_examples() {
        assert_eq!(HostKey::W.profile_name(), "w");
        assert_eq!(HostKey::R.profile_name(), "r");
        assert_eq!(HostKey::Space.profile_name(), "space");
        assert_eq!(HostKey::Tab.profile_name(), "tab");
        assert_eq!(HostKey::ArrowUp.profile_name(), "up");
        assert_eq!(HostKey::Num1.profile_name(), "1");
        assert_eq!(HostKey::F12.profile_name(), "f12");
        assert_eq!(HostKey::F10.profile_name(), "f10");
        assert_eq!(HostKey::Esc.profile_name(), "esc");
    }

    #[test]
    fn normalizes_f12_as_the_capture_toggle() {
        let event = InputEvent::new(evdev::EventType::KEY.0, KeyCode::KEY_F12.0, 1);

        assert_eq!(
            normalize_host_input_event(event),
            Some(HostKeyEvent::new(HostKey::F12, KeyTransition::Pressed))
        );
    }

    #[test]
    fn discovery_prefers_real_device_kind_over_composite_interfaces() {
        assert!(
            device_preference(
                "Logitech G403 Gaming Mouse",
                Path::new("/dev/input/by-id/logitech-event-mouse"),
                "mouse"
            ) > device_preference(
                "Hexgears Gaming Keyboard",
                Path::new("/dev/input/by-id/hexgears-if02-event-mouse"),
                "mouse"
            )
        );
        assert!(
            device_preference(
                "Hexgears Gaming Keyboard",
                Path::new("/dev/input/by-id/hexgears-event-kbd"),
                "keyboard"
            ) > device_preference(
                "Logitech G403 Gaming Mouse",
                Path::new("/dev/input/by-id/logitech-if01-event-kbd"),
                "keyboard"
            )
        );
    }

    #[test]
    fn host_key_events_convert_to_movement_events() {
        assert_eq!(
            HostKeyEvent::new(HostKey::W, KeyTransition::Pressed).movement_event(),
            Some(KeyboardEvent::new(MovementKey::Up, KeyTransition::Pressed))
        );
        assert_eq!(
            HostKeyEvent::new(HostKey::Space, KeyTransition::Pressed).movement_event(),
            None
        );
    }

    #[test]
    fn press_and_release_update_direction_once() {
        let mut state = DirectionalKeyState::default();

        assert_eq!(
            state.apply(event(MovementKey::Up, KeyTransition::Pressed)),
            KeyboardAction::DirectionChanged(DirectionalInput::new(true, false, false, false))
        );
        assert_eq!(
            state.apply(event(MovementKey::Up, KeyTransition::Pressed)),
            KeyboardAction::Ignored
        );
        assert_eq!(
            state.apply(event(MovementKey::Up, KeyTransition::Released)),
            KeyboardAction::DirectionChanged(DirectionalInput::default())
        );
    }

    #[test]
    fn repeat_events_do_not_create_runtime_updates() {
        let mut state = DirectionalKeyState::default();

        assert_eq!(
            state.apply(event(MovementKey::Right, KeyTransition::Repeated)),
            KeyboardAction::Ignored
        );
        assert_eq!(state.current(), DirectionalInput::default());
    }

    #[test]
    fn opposing_keys_are_tracked_independently() {
        let mut state = DirectionalKeyState::default();
        state.apply(event(MovementKey::Left, KeyTransition::Pressed));

        assert_eq!(
            state.apply(event(MovementKey::Right, KeyTransition::Pressed)),
            KeyboardAction::DirectionChanged(DirectionalInput::new(false, true, false, true))
        );
        assert_eq!(
            state.apply(event(MovementKey::Left, KeyTransition::Released)),
            KeyboardAction::DirectionChanged(DirectionalInput::new(false, false, false, true))
        );
    }

    #[test]
    fn escape_requests_exit_only_on_press() {
        let mut state = DirectionalKeyState::default();

        assert_eq!(
            state.apply(event(MovementKey::Exit, KeyTransition::Pressed)),
            KeyboardAction::ExitRequested
        );
        assert_eq!(
            state.apply(event(MovementKey::Exit, KeyTransition::Released)),
            KeyboardAction::Ignored
        );
    }
}
