use std::time::Duration;

use thiserror::Error;
use wroid_core::profile_v2::{
    materialize_axis, ActionV2, BindingV2, InputV2, JoystickMode, LayerActivation, ProfileV2,
    ProfileV2ValidationError,
};
use wroid_core::{Point, Resolution};

use crate::{
    ContactId, MouseAim, MouseAimConfigError, MouseAimRegion, MouseAimSensitivity,
    MouseAimSettings, VirtualJoystick, VirtualJoystickConfigError,
};

/// Stable runtime identifier for a control layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayerId(u16);

impl LayerId {
    pub const BASE: Self = Self(0);

    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerMode {
    Hold,
    Toggle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLayer {
    pub name: String,
    pub activation_key: HostKeyName,
    pub mode: LayerMode,
    pub id: LayerId,
}

/// Profile-visible keyboard key resolved to a stable bit index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum HostKeyName {
    Num0 = 0,
    Num1 = 1,
    Num2 = 2,
    Num3 = 3,
    Num4 = 4,
    Num5 = 5,
    Num6 = 6,
    Num7 = 7,
    Num8 = 8,
    Num9 = 9,
    A = 10,
    B = 11,
    C = 12,
    D = 13,
    E = 14,
    F = 15,
    G = 16,
    H = 17,
    I = 18,
    J = 19,
    K = 20,
    L = 21,
    M = 22,
    N = 23,
    O = 24,
    P = 25,
    Q = 26,
    R = 27,
    S = 28,
    T = 29,
    U = 30,
    V = 31,
    W = 32,
    X = 33,
    Y = 34,
    Z = 35,
    Space = 36,
    Tab = 37,
    Shift = 38,
    Ctrl = 39,
    Alt = 40,
    Up = 41,
    Left = 42,
    Down = 43,
    Right = 44,
    Esc = 45,
    F1 = 46,
    F2 = 47,
    F3 = 48,
    F4 = 49,
    F5 = 50,
    F6 = 51,
    F7 = 52,
    F8 = 53,
    F9 = 54,
    F10 = 55,
    F11 = 56,
}

impl HostKeyName {
    const PROFILE_NAMES: [(&'static str, Self); 57] = [
        ("0", Self::Num0),
        ("1", Self::Num1),
        ("2", Self::Num2),
        ("3", Self::Num3),
        ("4", Self::Num4),
        ("5", Self::Num5),
        ("6", Self::Num6),
        ("7", Self::Num7),
        ("8", Self::Num8),
        ("9", Self::Num9),
        ("a", Self::A),
        ("b", Self::B),
        ("c", Self::C),
        ("d", Self::D),
        ("e", Self::E),
        ("f", Self::F),
        ("g", Self::G),
        ("h", Self::H),
        ("i", Self::I),
        ("j", Self::J),
        ("k", Self::K),
        ("l", Self::L),
        ("m", Self::M),
        ("n", Self::N),
        ("o", Self::O),
        ("p", Self::P),
        ("q", Self::Q),
        ("r", Self::R),
        ("s", Self::S),
        ("t", Self::T),
        ("u", Self::U),
        ("v", Self::V),
        ("w", Self::W),
        ("x", Self::X),
        ("y", Self::Y),
        ("z", Self::Z),
        ("space", Self::Space),
        ("tab", Self::Tab),
        ("shift", Self::Shift),
        ("ctrl", Self::Ctrl),
        ("alt", Self::Alt),
        ("up", Self::Up),
        ("left", Self::Left),
        ("down", Self::Down),
        ("right", Self::Right),
        ("esc", Self::Esc),
        ("f1", Self::F1),
        ("f2", Self::F2),
        ("f3", Self::F3),
        ("f4", Self::F4),
        ("f5", Self::F5),
        ("f6", Self::F6),
        ("f7", Self::F7),
        ("f8", Self::F8),
        ("f9", Self::F9),
        ("f10", Self::F10),
        ("f11", Self::F11),
    ];

    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        Self::PROFILE_NAMES
            .iter()
            .find_map(|(name, key)| value.eq_ignore_ascii_case(name).then_some(*key))
    }

    pub const fn index(self) -> u8 {
        self as u8
    }

    pub const fn bit(self) -> u64 {
        1_u64 << self.index()
    }

    /// Canonical spelling used by profile v2 JSON and human-readable reports.
    pub fn profile_name(self) -> &'static str {
        Self::PROFILE_NAMES[usize::from(self.index())].0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModifierMask(u64);

impl ModifierMask {
    pub const EMPTY: Self = Self(0);

    pub const fn from_key(key: HostKeyName) -> Self {
        Self(key.bit())
    }

    pub const fn contains(self, key: HostKeyName) -> bool {
        self.0 & key.bit() != 0
    }

    pub fn insert(&mut self, key: HostKeyName) {
        self.0 |= key.bit();
    }

    pub fn remove(&mut self, key: HostKeyName) {
        self.0 &= !key.bit();
    }

    const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HostMouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
    Side = 3,
    Extra = 4,
}

impl HostMouseButton {
    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        [
            ("left", Self::Left),
            ("right", Self::Right),
            ("middle", Self::Middle),
            ("side", Self::Side),
            ("extra", Self::Extra),
        ]
        .into_iter()
        .find_map(|(name, button)| value.eq_ignore_ascii_case(name).then_some(button))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimePhysicalInput {
    Key(HostKeyName),
    MouseButton(HostMouseButton),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModifierSibling {
    input: RuntimePhysicalInput,
    modifiers: ModifierMask,
}

const EMPTY_MODIFIER_SIBLING: ModifierSibling = ModifierSibling {
    input: RuntimePhysicalInput::Key(HostKeyName::Num0),
    modifiers: ModifierMask::EMPTY,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeSiblingSuppression {
    entries: [ModifierSibling; 4],
    len: u8,
}

impl RuntimeSiblingSuppression {
    fn new() -> Self {
        Self {
            entries: [EMPTY_MODIFIER_SIBLING; 4],
            len: 0,
        }
    }

    fn add(&mut self, input: RuntimePhysicalInput, modifier: HostKeyName) {
        if let Some(entry) = self.entries[..usize::from(self.len)]
            .iter_mut()
            .find(|entry| entry.input == input)
        {
            entry.modifiers.insert(modifier);
            return;
        }

        let entry = &mut self.entries[usize::from(self.len)];
        *entry = ModifierSibling {
            input,
            modifiers: ModifierMask::from_key(modifier),
        };
        self.len += 1;
    }

    fn is_suppressed(&self, input: RuntimePhysicalInput, held: ModifierMask) -> bool {
        self.entries[..usize::from(self.len)]
            .iter()
            .any(|entry| entry.input == input && entry.modifiers.intersects(held))
    }
}

/// Runtime-ready controls materialized from a profile v2 document.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeControlPlan {
    pub profile_name: String,
    pub package_name: String,
    pub resolution: Resolution,
    pub layers: Vec<RuntimeLayer>,
    pub modifier_keys: ModifierMask,
    pub controls: Vec<RuntimeControlBinding>,
}

impl RuntimeControlPlan {
    pub fn from_profile_v2(
        profile: &ProfileV2,
        resolution: Resolution,
    ) -> Result<Self, RuntimeControlPlanError> {
        let binding_layers = resolve_binding_layers(profile)?;
        profile.validate()?;

        let layers = profile
            .layers
            .iter()
            .enumerate()
            .map(|(index, layer)| {
                let (key, mode) = match &layer.activation {
                    LayerActivation::Hold { key } => (key, LayerMode::Hold),
                    LayerActivation::Toggle { key } => (key, LayerMode::Toggle),
                };
                RuntimeLayer {
                    name: layer.name.clone(),
                    activation_key: HostKeyName::parse(key)
                        .expect("validated layer activation key must resolve"),
                    mode,
                    id: LayerId::new(index as u16 + 1),
                }
            })
            .collect();

        let mut modifier_keys = ModifierMask::EMPTY;
        for modifier in profile
            .bindings
            .iter()
            .filter_map(|binding| binding.modifier.as_deref())
        {
            modifier_keys.insert(
                HostKeyName::parse(modifier).expect("validated binding modifier must resolve"),
            );
        }

        let mut next_contact_id = 1_u16;
        let mut controls = Vec::with_capacity(profile.bindings.len());
        for (index, binding) in profile.bindings.iter().enumerate() {
            let action = materialize_action(binding, resolution, &mut next_contact_id)?;
            controls.push(RuntimeControlBinding {
                name: binding.name.clone(),
                input: binding.input.clone(),
                action,
                layer: binding_layers[index],
                modifier: binding.modifier.as_deref().and_then(HostKeyName::parse),
                sibling_suppression: precompute_sibling_suppression(
                    index,
                    profile,
                    &binding_layers,
                ),
            });
        }

        Ok(Self {
            profile_name: profile.name.clone(),
            package_name: profile.package_name.clone(),
            resolution,
            layers,
            modifier_keys,
            controls,
        })
    }

    pub fn control(&self, name: &str) -> Option<&RuntimeControlBinding> {
        self.controls.iter().find(|control| control.name == name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeControlBinding {
    pub name: String,
    pub input: InputV2,
    pub action: RuntimeControlAction,
    pub layer: LayerId,
    pub modifier: Option<HostKeyName>,
    sibling_suppression: RuntimeSiblingSuppression,
}

impl RuntimeControlBinding {
    pub fn is_suppressed(&self, input: RuntimePhysicalInput, held: ModifierMask) -> bool {
        self.modifier.is_none() && self.sibling_suppression.is_suppressed(input, held)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeControlAction {
    Tap {
        point: Point,
    },
    Hold {
        point: Point,
    },
    VirtualJoystick {
        joystick: VirtualJoystick,
        mode: JoystickMode,
        reaffirm_interval: Option<Duration>,
        slow_key: Option<HostKeyName>,
        walk_scale: f64,
    },
    MouseAim {
        aim: MouseAim,
        settings: MouseAimSettings,
    },
    RadialSelect {
        center: Point,
        target: Point,
    },
}

#[derive(Debug, Error)]
pub enum RuntimeControlPlanError {
    #[error(transparent)]
    InvalidProfile(#[from] ProfileV2ValidationError),
    #[error("binding {binding} references unknown layer: {layer}")]
    UnknownLayer { binding: String, layer: String },
    #[error("binding {binding} uses unsupported runtime action kind: {kind}")]
    UnsupportedAction { binding: String, kind: &'static str },
    #[error("binding {binding} has invalid virtual joystick geometry: {source}")]
    InvalidVirtualJoystick {
        binding: String,
        #[source]
        source: VirtualJoystickConfigError,
    },
    #[error("binding {binding} has invalid mouse aim geometry: {source}")]
    InvalidMouseAim {
        binding: String,
        #[source]
        source: MouseAimConfigError,
    },
    #[error("binding {binding} mouse aim sensitivity {sensitivity} cannot be represented safely")]
    InvalidMouseAimSensitivity { binding: String, sensitivity: f64 },
}

fn resolve_binding_layers(profile: &ProfileV2) -> Result<Vec<LayerId>, RuntimeControlPlanError> {
    profile
        .bindings
        .iter()
        .map(|binding| {
            let Some(layer_name) = binding.layer.as_deref() else {
                return Ok(LayerId::BASE);
            };
            profile
                .layers
                .iter()
                .position(|layer| layer.name == layer_name)
                .map(|index| LayerId::new(index as u16 + 1))
                .ok_or_else(|| RuntimeControlPlanError::UnknownLayer {
                    binding: binding.name.clone(),
                    layer: layer_name.to_owned(),
                })
        })
        .collect()
}

fn precompute_sibling_suppression(
    binding_index: usize,
    profile: &ProfileV2,
    binding_layers: &[LayerId],
) -> RuntimeSiblingSuppression {
    let mut suppression = RuntimeSiblingSuppression::new();
    if profile.bindings[binding_index].modifier.is_some() {
        return suppression;
    }

    for input in resolved_physical_inputs(&profile.bindings[binding_index].input)
        .into_iter()
        .flatten()
    {
        for (sibling_index, sibling) in profile.bindings.iter().enumerate() {
            if binding_layers[sibling_index] != binding_layers[binding_index] {
                continue;
            }
            let Some(modifier) = sibling.modifier.as_deref().and_then(HostKeyName::parse) else {
                continue;
            };
            if resolved_physical_inputs(&sibling.input)
                .into_iter()
                .flatten()
                .any(|sibling_input| sibling_input == input)
            {
                suppression.add(input, modifier);
            }
        }
    }
    suppression
}

fn resolved_physical_inputs(input: &InputV2) -> [Option<RuntimePhysicalInput>; 4] {
    let key = |value: &str| HostKeyName::parse(value).map(RuntimePhysicalInput::Key);
    match input {
        InputV2::Key { key: value } => [key(value), None, None, None],
        InputV2::KeyCluster {
            up,
            left,
            down,
            right,
        } => [key(up), key(left), key(down), key(right)],
        InputV2::MouseButton { button } => [
            HostMouseButton::parse(button).map(RuntimePhysicalInput::MouseButton),
            None,
            None,
            None,
        ],
        InputV2::MouseMove => [None, None, None, None],
    }
}

fn materialize_action(
    binding: &BindingV2,
    resolution: Resolution,
    next_contact_id: &mut u16,
) -> Result<RuntimeControlAction, RuntimeControlPlanError> {
    match &binding.action {
        ActionV2::Tap { point } => Ok(RuntimeControlAction::Tap {
            point: point.materialize(resolution),
        }),
        ActionV2::Hold { point } => Ok(RuntimeControlAction::Hold {
            point: point.materialize(resolution),
        }),
        ActionV2::VirtualJoystick {
            center,
            radius,
            dead_zone,
            mode,
            reaffirm_ms,
            slow_key,
            slow_scale,
        } => {
            let contact_id = allocate_contact_id(next_contact_id);
            let joystick = VirtualJoystick::from_profile_v2_geometry(
                contact_id, *center, *radius, *dead_zone, resolution,
            )
            .map_err(|source| RuntimeControlPlanError::InvalidVirtualJoystick {
                binding: binding.name.clone(),
                source,
            })?;
            let slow_key = slow_key.as_deref().and_then(HostKeyName::parse);
            // A slow key without an explicit scale walks at half speed.
            let walk_scale = if slow_key.is_some() {
                slow_scale.unwrap_or(0.5)
            } else {
                1.0
            };
            Ok(RuntimeControlAction::VirtualJoystick {
                joystick,
                mode: mode.clone(),
                reaffirm_interval: reaffirm_ms.map(Duration::from_millis),
                slow_key,
                walk_scale,
            })
        }
        ActionV2::MouseAim {
            region,
            sensitivity,
            toggle_key,
            recenter_threshold,
            recenter_gap_ms,
            ads_multiplier,
            reaffirm_ms,
        } => {
            let contact_id = allocate_contact_id(next_contact_id);
            let alternate_contact_id = allocate_contact_id(next_contact_id);
            let left = materialize_axis(region.x, resolution.width);
            let top = materialize_axis(region.y, resolution.height);
            let right = materialize_axis(region.x + region.w, resolution.width);
            let bottom = materialize_axis(region.y + region.h, resolution.height);
            let region = MouseAimRegion {
                left,
                top,
                right,
                bottom,
            };
            let origin = Point {
                x: left + (right - left) / 2,
                y: top + (bottom - top) / 2,
            };
            let sensitivity = materialize_mouse_sensitivity(*sensitivity).ok_or_else(|| {
                RuntimeControlPlanError::InvalidMouseAimSensitivity {
                    binding: binding.name.clone(),
                    sensitivity: *sensitivity,
                }
            })?;
            let aim = MouseAim::new(contact_id, origin, region, resolution, sensitivity).map_err(
                |source| RuntimeControlPlanError::InvalidMouseAim {
                    binding: binding.name.clone(),
                    source,
                },
            )?;
            let ads_multiplier = ads_multiplier
                .map(|value| {
                    materialize_mouse_sensitivity(value).ok_or_else(|| {
                        RuntimeControlPlanError::InvalidMouseAimSensitivity {
                            binding: binding.name.clone(),
                            sensitivity: value,
                        }
                    })
                })
                .transpose()?;
            let settings = MouseAimSettings {
                alternate_contact_id,
                toggle_key: toggle_key.clone(),
                recenter_threshold_milli: (*recenter_threshold * 1_000.0).round() as u16,
                recenter_gap: Duration::from_millis(*recenter_gap_ms),
                ads_multiplier,
                reaffirm_interval: reaffirm_ms.map(Duration::from_millis),
            };
            Ok(RuntimeControlAction::MouseAim { aim, settings })
        }
        ActionV2::RadialSelect {
            center,
            distance,
            direction,
        } => {
            use wroid_core::profile_v2::{materialize_radius, RadialDirection};
            let center = center.materialize(resolution);
            let reach = i64::from(materialize_radius(*distance, resolution));
            let (dx, dy) = match direction {
                RadialDirection::Up => (0, -reach),
                RadialDirection::Down => (0, reach),
                RadialDirection::Left => (-reach, 0),
                RadialDirection::Right => (reach, 0),
            };
            let clamp_axis = |value: i64, limit: u32| -> u32 {
                value.clamp(0, i64::from(limit.saturating_sub(1))) as u32
            };
            Ok(RuntimeControlAction::RadialSelect {
                center,
                target: Point {
                    x: clamp_axis(i64::from(center.x) + dx, resolution.width),
                    y: clamp_axis(i64::from(center.y) + dy, resolution.height),
                },
            })
        }
        ActionV2::Macro { .. } => Err(RuntimeControlPlanError::UnsupportedAction {
            binding: binding.name.clone(),
            kind: "macro",
        }),
    }
}

fn allocate_contact_id(next_contact_id: &mut u16) -> ContactId {
    let contact_id = ContactId::new(*next_contact_id);
    *next_contact_id = (*next_contact_id).saturating_add(1);
    contact_id
}

fn materialize_mouse_sensitivity(value: f64) -> Option<MouseAimSensitivity> {
    const DENOMINATOR: u32 = 1_000;

    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let scaled = (value * f64::from(DENOMINATOR)).round();
    if scaled < 1.0 || scaled > f64::from(u32::MAX) {
        return None;
    }
    MouseAimSensitivity::new(scaled as u32, DENOMINATOR).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wroid_core::profile_v2::{
        JoystickMode, LayerActivation, LayerV2, NormalizedPoint, NormalizedRect,
    };

    fn tap_binding(
        name: &str,
        key: &str,
        layer: Option<&str>,
        modifier: Option<&str>,
    ) -> BindingV2 {
        BindingV2 {
            name: name.to_owned(),
            layer: layer.map(str::to_owned),
            modifier: modifier.map(str::to_owned),
            input: InputV2::Key {
                key: key.to_owned(),
            },
            action: ActionV2::Tap {
                point: NormalizedPoint { x: 0.5, y: 0.5 },
            },
        }
    }

    fn profile() -> ProfileV2 {
        ProfileV2 {
            schema_version: 2,
            name: "Shooter v2".to_owned(),
            package_name: "com.example.shooter".to_owned(),
            orientation: Default::default(),
            layers: Vec::new(),
            bindings: vec![
                BindingV2 {
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
                        center: NormalizedPoint { x: 0.18, y: 0.78 },
                        radius: 0.09,
                        dead_zone: 0.02,
                        mode: JoystickMode::Hold,
                        reaffirm_ms: Some(50),
                        slow_key: Some("shift".to_owned()),
                        slow_scale: Some(0.5),
                    },
                },
                BindingV2 {
                    name: "aim".to_owned(),
                    layer: None,
                    modifier: None,
                    input: InputV2::MouseMove,
                    action: ActionV2::MouseAim {
                        region: NormalizedRect {
                            x: 0.35,
                            y: 0.06,
                            w: 0.60,
                            h: 0.78,
                        },
                        sensitivity: 1.2,
                        toggle_key: Some("tab".to_owned()),
                        recenter_threshold: 0.7,
                        recenter_gap_ms: 0,
                        ads_multiplier: Some(0.6),
                        reaffirm_ms: Some(50),
                    },
                },
                BindingV2 {
                    name: "fire".to_owned(),
                    layer: None,
                    modifier: None,
                    input: InputV2::MouseButton {
                        button: "left".to_owned(),
                    },
                    action: ActionV2::Tap {
                        point: NormalizedPoint { x: 0.86, y: 0.50 },
                    },
                },
                BindingV2 {
                    name: "automatic_fire".to_owned(),
                    layer: None,
                    modifier: None,
                    input: InputV2::MouseButton {
                        button: "side".to_owned(),
                    },
                    action: ActionV2::Hold {
                        point: NormalizedPoint { x: 0.80, y: 0.40 },
                    },
                },
            ],
        }
    }

    #[test]
    fn materializes_supported_profile_v2_controls() {
        let resolution = Resolution {
            width: 1920,
            height: 1080,
        };

        let plan = RuntimeControlPlan::from_profile_v2(&profile(), resolution).unwrap();

        assert_eq!(plan.profile_name, "Shooter v2");
        assert_eq!(plan.package_name, "com.example.shooter");
        assert_eq!(plan.resolution, resolution);
        assert_eq!(plan.controls.len(), 4);

        let movement = plan.control("movement").unwrap();
        let RuntimeControlAction::VirtualJoystick {
            joystick,
            mode,
            reaffirm_interval,
            ..
        } = &movement.action
        else {
            panic!("movement should materialize as a virtual joystick");
        };
        assert_eq!(joystick.contact_id(), ContactId::new(1));
        assert_eq!(joystick.center(), Point { x: 345, y: 842 });
        assert_eq!(joystick.radius(), 97);
        assert_eq!(joystick.dead_zone(), 22);
        assert_eq!(mode, &JoystickMode::Hold);
        assert_eq!(*reaffirm_interval, Some(Duration::from_millis(50)));

        let aim = plan.control("aim").unwrap();
        let RuntimeControlAction::MouseAim { aim, settings } = &aim.action else {
            panic!("aim should materialize as mouse aim");
        };
        assert_eq!(aim.contact_id(), ContactId::new(2));
        assert_eq!(settings.alternate_contact_id, ContactId::new(3));
        assert_eq!(settings.toggle_key.as_deref(), Some("tab"));
        assert_eq!(settings.recenter_threshold_milli, 700);
        assert_eq!(
            settings.ads_multiplier,
            Some(MouseAimSensitivity::new(600, 1000).unwrap())
        );
        assert_eq!(settings.reaffirm_interval, Some(Duration::from_millis(50)));
        assert_eq!(
            aim.region(),
            MouseAimRegion {
                left: 672,
                top: 65,
                right: 1823,
                bottom: 906,
            }
        );
        assert_eq!(aim.origin(), Point { x: 1247, y: 485 });
        assert_eq!(
            aim.sensitivity(),
            MouseAimSensitivity::new(1200, 1000).unwrap()
        );

        let fire = plan.control("fire").unwrap();
        assert_eq!(
            &fire.action,
            &RuntimeControlAction::Tap {
                point: Point { x: 1650, y: 540 }
            }
        );

        let automatic_fire = plan.control("automatic_fire").unwrap();
        assert_eq!(
            &automatic_fire.action,
            &RuntimeControlAction::Hold {
                point: Point { x: 1535, y: 432 }
            }
        );
    }

    #[test]
    fn macro_remains_explicitly_unsupported() {
        let mut profile = profile();
        profile.bindings[0].action = ActionV2::Macro {
            steps: vec![ActionV2::Tap {
                point: NormalizedPoint { x: 0.5, y: 0.5 },
            }],
        };

        let error = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RuntimeControlPlanError::UnsupportedAction {
                binding,
                kind: "macro"
            } if binding == "movement"
        ));
    }

    #[test]
    fn declared_layers_materialize_in_profile_order_with_activation_modes() {
        let mut profile = profile();
        profile.layers = vec![
            LayerV2 {
                name: "aim".to_owned(),
                activation: LayerActivation::Hold {
                    key: " TaB ".to_owned(),
                },
            },
            LayerV2 {
                name: "vehicle".to_owned(),
                activation: LayerActivation::Toggle {
                    key: "ALT".to_owned(),
                },
            },
        ];

        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap();

        assert_eq!(
            plan.layers,
            vec![
                RuntimeLayer {
                    name: "aim".to_owned(),
                    activation_key: HostKeyName::Tab,
                    mode: LayerMode::Hold,
                    id: LayerId::new(1),
                },
                RuntimeLayer {
                    name: "vehicle".to_owned(),
                    activation_key: HostKeyName::Alt,
                    mode: LayerMode::Toggle,
                    id: LayerId::new(2),
                },
            ]
        );
    }

    #[test]
    fn base_and_declared_binding_layers_resolve_once() {
        let mut profile = profile();
        profile.layers.push(LayerV2 {
            name: "aim".to_owned(),
            activation: LayerActivation::Hold {
                key: "tab".to_owned(),
            },
        });
        profile.bindings.push(tap_binding(
            "alternate reload",
            "R",
            Some("aim"),
            Some(" ShIfT "),
        ));

        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap();

        assert_eq!(LayerId::BASE.get(), 0);
        assert_eq!(plan.control("fire").unwrap().layer, LayerId::BASE);
        let alternate = plan.control("alternate reload").unwrap();
        assert_eq!(alternate.layer, LayerId::new(1));
        assert_eq!(alternate.modifier, Some(HostKeyName::Shift));
        assert!(plan.modifier_keys.contains(HostKeyName::Shift));
        assert!(!plan.modifier_keys.contains(HostKeyName::Ctrl));
    }

    #[test]
    fn unknown_binding_layer_has_a_dedicated_materialization_error() {
        let mut profile = profile();
        profile.bindings.push(tap_binding(
            "unknown layer binding",
            "r",
            Some("missing"),
            None,
        ));

        let error = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RuntimeControlPlanError::UnknownLayer { binding, layer }
                if binding == "unknown layer binding" && layer == "missing"
        ));
    }

    #[test]
    fn resolved_input_names_trim_and_match_ascii_case_insensitively() {
        let known_names = [
            "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "a", "b", "c", "d", "e", "f", "g",
            "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s", "t", "u", "v", "w", "x",
            "y", "z", "space", "tab", "shift", "ctrl", "alt", "up", "left", "down", "right", "esc",
            "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11",
        ];
        for (index, name) in known_names.into_iter().enumerate() {
            let key = HostKeyName::parse(name).unwrap();
            assert_eq!(usize::from(key.index()), index);
            assert_eq!(key.bit(), 1_u64 << index);
        }
        assert_eq!(HostKeyName::parse(" R "), Some(HostKeyName::R));
        assert_eq!(HostKeyName::parse("sHiFt"), Some(HostKeyName::Shift));
        assert_ne!(HostKeyName::R.bit(), HostKeyName::Shift.bit());
        assert_eq!(HostKeyName::parse("f12"), None);
        assert_eq!(
            HostMouseButton::parse(" LeFt "),
            Some(HostMouseButton::Left)
        );
        assert_eq!(HostMouseButton::parse("SIDE"), Some(HostMouseButton::Side));
    }

    #[test]
    fn modifier_siblings_suppress_an_unmodified_key_for_any_held_sibling_modifier() {
        let mut profile = profile();
        profile.bindings.extend([
            tap_binding("reload", "r", None, None),
            tap_binding("alternate reload", "r", None, Some("shift")),
            tap_binding("special reload", "r", None, Some("ctrl")),
        ]);

        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap();
        let reload = plan.control("reload").unwrap();
        let input = RuntimePhysicalInput::Key(HostKeyName::R);

        assert!(!reload.is_suppressed(input, ModifierMask::EMPTY));
        assert!(reload.is_suppressed(input, ModifierMask::from_key(HostKeyName::Shift)));
        assert!(reload.is_suppressed(input, ModifierMask::from_key(HostKeyName::Ctrl)));
        assert!(!reload.is_suppressed(input, ModifierMask::from_key(HostKeyName::Alt)));
    }

    #[test]
    fn key_cluster_suppression_is_scoped_to_each_constituent_key() {
        let mut profile = profile();
        profile
            .bindings
            .push(tap_binding("shift forward", "w", None, Some("shift")));

        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap();
        let movement = plan.control("movement").unwrap();
        let shift = ModifierMask::from_key(HostKeyName::Shift);

        assert!(movement.is_suppressed(RuntimePhysicalInput::Key(HostKeyName::W), shift));
        for key in [HostKeyName::A, HostKeyName::S, HostKeyName::D] {
            assert!(!movement.is_suppressed(RuntimePhysicalInput::Key(key), shift));
        }
    }

    #[test]
    fn same_physical_key_in_another_layer_does_not_suppress_base() {
        let mut profile = profile();
        profile.layers.push(LayerV2 {
            name: "aim".to_owned(),
            activation: LayerActivation::Toggle {
                key: "tab".to_owned(),
            },
        });
        profile.bindings.extend([
            tap_binding("reload", "r", None, None),
            tap_binding("aim reload", "r", Some("aim"), Some("shift")),
        ]);

        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap();

        assert!(!plan.control("reload").unwrap().is_suppressed(
            RuntimePhysicalInput::Key(HostKeyName::R),
            ModifierMask::from_key(HostKeyName::Shift),
        ));
    }

    #[test]
    fn modifier_siblings_also_suppress_unmodified_mouse_buttons() {
        let mut profile = profile();
        profile.bindings.push(BindingV2 {
            name: "alternate fire".to_owned(),
            layer: None,
            modifier: Some("shift".to_owned()),
            input: InputV2::MouseButton {
                button: "left".to_owned(),
            },
            action: ActionV2::Tap {
                point: NormalizedPoint { x: 0.7, y: 0.5 },
            },
        });

        let plan = RuntimeControlPlan::from_profile_v2(
            &profile,
            Resolution {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap();

        assert!(plan.control("fire").unwrap().is_suppressed(
            RuntimePhysicalInput::MouseButton(HostMouseButton::Left),
            ModifierMask::from_key(HostKeyName::Shift),
        ));
    }

    #[test]
    fn resolved_host_keys_keep_their_profile_spelling() {
        for (input, expected) in [("0", "0"), ("r", "r"), ("Shift", "shift"), ("esc", "esc")] {
            assert_eq!(HostKeyName::parse(input).unwrap().profile_name(), expected);
        }
    }
}
