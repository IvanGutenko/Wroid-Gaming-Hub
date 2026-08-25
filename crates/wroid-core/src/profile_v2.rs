use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Point, Resolution};

pub const PROFILE_SCHEMA_VERSION: u16 = 2;
const EPSILON: f64 = 0.000_000_001;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileV2 {
    #[serde(default = "default_schema_version")]
    pub schema_version: u16,
    pub name: String,
    pub package_name: String,
    #[serde(default)]
    pub orientation: Orientation,
    #[serde(default)]
    pub layers: Vec<LayerV2>,
    #[serde(default)]
    pub bindings: Vec<BindingV2>,
}

impl ProfileV2 {
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ProfileV2LoadError> {
        let data = fs::read_to_string(path.as_ref()).map_err(ProfileV2LoadError::Io)?;
        serde_json::from_str(&data).map_err(ProfileV2LoadError::Json)
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), ProfileV2SaveError> {
        self.validate()?;
        let path = path.as_ref();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("profile.json");
        let temporary = parent.join(format!(".{file_name}.wroid-{}.tmp", std::process::id()));
        let mut data = serde_json::to_string_pretty(self)?;
        data.push('\n');
        fs::write(&temporary, data).map_err(ProfileV2SaveError::Io)?;
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(ProfileV2SaveError::Io(error));
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ProfileV2ValidationError> {
        let mut errors = Vec::new();

        if self.schema_version != PROFILE_SCHEMA_VERSION {
            errors.push(format!(
                "schema_version must be {PROFILE_SCHEMA_VERSION}, got {}",
                self.schema_version
            ));
        }
        if self.name.trim().is_empty() {
            errors.push("name must not be empty".to_owned());
        }
        if self.package_name.trim().is_empty() {
            errors.push("package_name must not be empty".to_owned());
        }

        let mut layer_names = HashSet::new();
        let mut declared_layers = HashSet::new();
        for layer in &self.layers {
            let layer_name = layer.name.trim();
            if layer_name.is_empty() {
                errors.push("layer name must not be empty".to_owned());
            } else if !layer_names.insert(layer_name.to_owned()) {
                errors.push(format!("duplicate layer name: {}", layer.name));
            }
            if layer_name.eq_ignore_ascii_case("base") {
                errors.push("layer name base is reserved".to_owned());
            }
            declared_layers.insert(layer.name.clone());
        }
        if self.layers.len() > 64 {
            errors.push("profile may declare at most 64 layers".to_owned());
        }

        let mut activation_keys = HashSet::new();
        let mut layer_activation_keys = HashSet::new();
        for layer in &self.layers {
            let key = layer_activation_key(&layer.activation);
            let canonical_key = canonical_key(key);
            if !known_key_name(key) {
                errors.push(format!(
                    "layer {} uses unsupported activation key: {key}",
                    layer.name
                ));
            }
            if !activation_keys.insert(canonical_key.clone()) {
                errors.push(format!("duplicate layer activation key: {key}"));
            }
            layer_activation_keys.insert((layer.name.as_str(), canonical_key));
        }

        for binding in &self.bindings {
            if binding.layer.is_none() && binding.modifier.is_none() {
                for key in binding_input_keys(&binding.input) {
                    if activation_keys.contains(&canonical_key(key)) {
                        errors.push(format!(
                            "layer activation key {key} cannot be used by a base-layer binding"
                        ));
                    }
                }
            }
        }

        let mut names = HashSet::new();
        let mut scoped_input_keys = HashSet::new();
        for binding in &self.bindings {
            let binding_name = binding.name.trim();
            if binding_name.is_empty() {
                errors.push("binding name must not be empty".to_owned());
            } else if !names.insert(binding_name.to_owned()) {
                errors.push(format!("duplicate binding name: {}", binding.name));
            }

            validate_input(&binding.input, &binding.name, &mut errors);
            validate_action(&binding.action, &binding.name, &mut errors);
            validate_binding_compatibility(
                &binding.input,
                &binding.action,
                &binding.name,
                &mut errors,
            );

            if let Some(layer) = &binding.layer {
                if !declared_layers.contains(layer) {
                    errors.push(format!(
                        "binding {} references unknown layer: {layer}",
                        binding.name
                    ));
                }
            }

            if let Some(modifier) = &binding.modifier {
                if !known_key_name(modifier) {
                    errors.push(format!(
                        "binding {} uses unsupported modifier: {modifier}",
                        binding.name
                    ));
                }
                validate_modifier_input_keys(binding, modifier, &mut errors);
                if matches!(&binding.input, InputV2::MouseMove) {
                    errors.push(format!(
                        "binding {} cannot use a modifier with mouse_move input",
                        binding.name
                    ));
                }
                if canonical_key(modifier) == "ctrl" {
                    for key in binding_input_keys(&binding.input) {
                        let key = canonical_key(key);
                        if matches!(key.as_str(), "esc" | "c") {
                            errors.push(format!(
                                "binding {} uses ctrl+{key}, which is reserved for the session exit hotkey",
                                binding.name
                            ));
                        }
                    }
                }
            }

            let scope_layer = binding.layer.as_deref().unwrap_or("base");
            let scope_modifier = binding.modifier.as_deref().map(canonical_key);
            let mut binding_keys = HashSet::new();
            for (key, input_kind) in binding_scope_input_keys(&binding.input) {
                let canonical_key = canonical_key(key);
                if !binding_keys.insert((input_kind, canonical_key.clone())) {
                    continue;
                }
                if !scoped_input_keys.insert((
                    scope_layer.to_owned(),
                    scope_modifier.clone(),
                    input_kind,
                    canonical_key.clone(),
                )) {
                    let modifier = scope_modifier
                        .as_deref()
                        .map(|modifier| format!("with modifier {modifier}"))
                        .unwrap_or_else(|| "without a modifier".to_owned());
                    errors.push(format!(
                        "{input_kind} {key} drives multiple bindings in {scope_layer} layer {modifier}"
                    ));
                }
                if input_kind == "key"
                    && binding.layer.is_some()
                    && layer_activation_keys.contains(&(scope_layer, canonical_key.clone()))
                {
                    errors.push(format!(
                        "layer activation key {key} cannot be used by binding {} inside layer {scope_layer}",
                        binding.name
                    ));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ProfileV2ValidationError { errors })
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    #[default]
    Landscape,
    Portrait,
    Any,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindingV2 {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modifier: Option<String>,
    pub input: InputV2,
    pub action: ActionV2,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerV2 {
    pub name: String,
    pub activation: LayerActivation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayerActivation {
    Hold { key: String },
    Toggle { key: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputV2 {
    Key {
        key: String,
    },
    KeyCluster {
        up: String,
        left: String,
        down: String,
        right: String,
    },
    MouseButton {
        button: String,
    },
    MouseMove,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionV2 {
    Tap {
        point: NormalizedPoint,
    },
    Hold {
        point: NormalizedPoint,
    },
    VirtualJoystick {
        center: NormalizedPoint,
        radius: f64,
        #[serde(default)]
        dead_zone: f64,
        #[serde(default)]
        mode: JoystickMode,
        #[serde(default)]
        reaffirm_ms: Option<u64>,
    },
    MouseAim {
        region: NormalizedRect,
        #[serde(default = "default_sensitivity")]
        sensitivity: f64,
        #[serde(default)]
        toggle_key: Option<String>,
        #[serde(default = "default_recenter_threshold")]
        recenter_threshold: f64,
        #[serde(default)]
        recenter_gap_ms: u64,
        #[serde(default)]
        ads_multiplier: Option<f64>,
        #[serde(default)]
        reaffirm_ms: Option<u64>,
    },
    Macro {
        steps: Vec<ActionV2>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoystickMode {
    #[default]
    Hold,
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormalizedPoint {
    pub x: f64,
    pub y: f64,
}

impl NormalizedPoint {
    pub fn materialize(self, resolution: Resolution) -> Point {
        Point {
            x: materialize_axis(self.x, resolution.width),
            y: materialize_axis(self.y, resolution.height),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormalizedRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Error)]
pub enum ProfileV2LoadError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum ProfileV2SaveError {
    #[error(transparent)]
    InvalidProfile(#[from] ProfileV2ValidationError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileV2ValidationError {
    pub errors: Vec<String>,
}

impl std::fmt::Display for ProfileV2ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} validation error(s)", self.errors.len())
    }
}

impl std::error::Error for ProfileV2ValidationError {}

pub fn materialize_axis(value: f64, limit: u32) -> u32 {
    if limit == 0 {
        return 0;
    }
    let maximum = limit - 1;
    ((value.clamp(0.0, 1.0) * f64::from(maximum)).round() as u32).min(maximum)
}

pub fn materialize_radius(value: f64, resolution: Resolution) -> u32 {
    let base = f64::from(resolution.width.min(resolution.height));
    (value.clamp(0.0, 1.0) * base).round().max(1.0) as u32
}

pub fn materialize_dead_zone(value: f64, radius: u32, resolution: Resolution) -> u32 {
    if value <= 0.0 || radius <= 1 {
        return 0;
    }

    let base = f64::from(resolution.width.min(resolution.height));
    let dead_zone = (value.clamp(0.0, 1.0) * base).round() as u32;
    dead_zone.min(radius - 1)
}

fn layer_activation_key(activation: &LayerActivation) -> &str {
    match activation {
        LayerActivation::Hold { key } | LayerActivation::Toggle { key } => key,
    }
}

fn canonical_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn binding_input_keys(input: &InputV2) -> Vec<&str> {
    match input {
        InputV2::Key { key } => vec![key],
        InputV2::KeyCluster {
            up,
            left,
            down,
            right,
        } => vec![up, left, down, right],
        InputV2::MouseButton { .. } | InputV2::MouseMove => Vec::new(),
    }
}

fn binding_scope_input_keys(input: &InputV2) -> Vec<(&str, &'static str)> {
    match input {
        InputV2::Key { key } => vec![(key, "key")],
        InputV2::KeyCluster {
            up,
            left,
            down,
            right,
        } => vec![(up, "key"), (left, "key"), (down, "key"), (right, "key")],
        InputV2::MouseButton { button } => vec![(button, "mouse_button")],
        InputV2::MouseMove => Vec::new(),
    }
}

fn validate_modifier_input_keys(binding: &BindingV2, modifier: &str, errors: &mut Vec<String>) {
    let modifier = canonical_key(modifier);
    match &binding.input {
        InputV2::Key { key } if canonical_key(key) == modifier => errors.push(format!(
            "binding {} modifier must differ from input key: {key}",
            binding.name
        )),
        InputV2::KeyCluster {
            up,
            left,
            down,
            right,
        } => {
            for key in [up, left, down, right] {
                if canonical_key(key) == modifier {
                    errors.push(format!(
                        "binding {} modifier must differ from key_cluster key: {key}",
                        binding.name
                    ));
                }
            }
        }
        InputV2::Key { .. } | InputV2::MouseButton { .. } | InputV2::MouseMove => {}
    }
}

fn validate_input(input: &InputV2, binding: &str, errors: &mut Vec<String>) {
    match input {
        InputV2::Key { key } if key.trim().is_empty() => {
            errors.push(format!("binding {binding} has an empty key input"));
        }
        InputV2::Key { key } if !known_key_name(key) => {
            errors.push(format!(
                "binding {binding} uses unsupported key input: {key}"
            ));
        }
        InputV2::KeyCluster {
            up,
            left,
            down,
            right,
        } if [up, left, down, right]
            .iter()
            .any(|value| value.trim().is_empty()) =>
        {
            errors.push(format!("binding {binding} has an empty key_cluster input"));
        }
        InputV2::KeyCluster {
            up,
            left,
            down,
            right,
        } if [up, left, down, right]
            .iter()
            .any(|value| !known_key_name(value)) =>
        {
            errors.push(format!(
                "binding {binding} key_cluster contains an unsupported key"
            ));
        }
        InputV2::MouseButton { button } if button.trim().is_empty() => {
            errors.push(format!("binding {binding} has an empty mouse button"));
        }
        InputV2::MouseButton { button }
            if !matches!(
                button.trim().to_ascii_lowercase().as_str(),
                "left" | "right" | "middle" | "side" | "extra"
            ) =>
        {
            errors.push(format!(
                "binding {binding} uses unsupported mouse button: {button}"
            ));
        }
        InputV2::Key { .. }
        | InputV2::KeyCluster { .. }
        | InputV2::MouseButton { .. }
        | InputV2::MouseMove => {}
    }
}

fn validate_action(action: &ActionV2, binding: &str, errors: &mut Vec<String>) {
    match action {
        ActionV2::Tap { point } | ActionV2::Hold { point } => {
            let kind = if matches!(action, ActionV2::Tap { .. }) {
                "tap"
            } else {
                "hold"
            };
            validate_point(*point, &format!("binding {binding} {kind} point"), errors);
        }
        ActionV2::VirtualJoystick {
            center,
            radius,
            dead_zone,
            reaffirm_ms,
            ..
        } => {
            validate_point(
                *center,
                &format!("binding {binding} virtual_joystick center"),
                errors,
            );
            if !radius.is_finite() || *radius <= 0.0 || *radius > 1.0 {
                errors.push(format!(
                    "binding {binding} virtual_joystick radius must be finite and within 0.0..=1.0"
                ));
            }
            if !dead_zone.is_finite() || *dead_zone < 0.0 || *dead_zone >= 1.0 {
                errors.push(format!(
                    "binding {binding} virtual_joystick dead_zone must be finite and within 0.0..1.0"
                ));
            } else if radius.is_finite() && *dead_zone >= *radius {
                errors.push(format!(
                    "binding {binding} virtual_joystick dead_zone must be smaller than radius"
                ));
            }
            if matches!(reaffirm_ms, Some(0)) {
                errors.push(format!(
                    "binding {binding} virtual_joystick reaffirm_ms must be greater than zero"
                ));
            }
        }
        ActionV2::MouseAim {
            region,
            sensitivity,
            toggle_key,
            recenter_threshold,
            ads_multiplier,
            reaffirm_ms,
            ..
        } => {
            validate_rect(
                *region,
                &format!("binding {binding} mouse_aim region"),
                errors,
            );
            if !sensitivity.is_finite() || *sensitivity <= 0.0 {
                errors.push(format!(
                    "binding {binding} mouse_aim sensitivity must be finite and greater than zero"
                ));
            }
            if toggle_key
                .as_deref()
                .is_some_and(|key| !known_key_name(key))
            {
                errors.push(format!(
                    "binding {binding} mouse_aim toggle_key must be a supported key name"
                ));
            }
            if !recenter_threshold.is_finite() || !(0.1..=1.0).contains(recenter_threshold) {
                errors.push(format!(
                    "binding {binding} mouse_aim recenter_threshold must be finite and within 0.1..=1.0"
                ));
            }
            if ads_multiplier
                .is_some_and(|value| !value.is_finite() || !(0.1..=1.0).contains(&value))
            {
                errors.push(format!(
                    "binding {binding} mouse_aim ads_multiplier must be finite and within 0.1..=1.0"
                ));
            }
            if matches!(reaffirm_ms, Some(0)) {
                errors.push(format!(
                    "binding {binding} mouse_aim reaffirm_ms must be greater than zero"
                ));
            }
        }
        ActionV2::Macro { steps } => {
            if steps.is_empty() {
                errors.push(format!(
                    "binding {binding} macro must contain at least one step"
                ));
            }
            for (index, step) in steps.iter().enumerate() {
                validate_action(step, &format!("{binding}.step[{index}]"), errors);
            }
        }
    }
}

fn validate_binding_compatibility(
    input: &InputV2,
    action: &ActionV2,
    binding: &str,
    errors: &mut Vec<String>,
) {
    let required = match action {
        ActionV2::Tap { .. } | ActionV2::Hold { .. }
            if !matches!(input, InputV2::Key { .. } | InputV2::MouseButton { .. }) =>
        {
            Some("key or mouse_button")
        }
        ActionV2::VirtualJoystick { .. } if !matches!(input, InputV2::KeyCluster { .. }) => {
            Some("key_cluster")
        }
        ActionV2::MouseAim { .. } if !matches!(input, InputV2::MouseMove) => Some("mouse_move"),
        ActionV2::Tap { .. }
        | ActionV2::Hold { .. }
        | ActionV2::VirtualJoystick { .. }
        | ActionV2::MouseAim { .. }
        | ActionV2::Macro { .. } => None,
    };
    if let Some(required) = required {
        errors.push(format!(
            "binding {binding} pairs {} input with {} action; {} requires {required}",
            input_kind(input),
            action_kind(action),
            action_kind(action),
        ));
    }
}

fn input_kind(input: &InputV2) -> &'static str {
    match input {
        InputV2::Key { .. } => "key",
        InputV2::KeyCluster { .. } => "key_cluster",
        InputV2::MouseButton { .. } => "mouse_button",
        InputV2::MouseMove => "mouse_move",
    }
}

fn action_kind(action: &ActionV2) -> &'static str {
    match action {
        ActionV2::Tap { .. } => "tap",
        ActionV2::Hold { .. } => "hold",
        ActionV2::VirtualJoystick { .. } => "virtual_joystick",
        ActionV2::MouseAim { .. } => "mouse_aim",
        ActionV2::Macro { .. } => "macro",
    }
}

fn validate_point(point: NormalizedPoint, label: &str, errors: &mut Vec<String>) {
    if !normalized(point.x) || !normalized(point.y) {
        errors.push(format!(
            "{label} must use normalized x/y coordinates within 0.0..=1.0"
        ));
    }
}

fn validate_rect(rect: NormalizedRect, label: &str, errors: &mut Vec<String>) {
    if !normalized(rect.x)
        || !normalized(rect.y)
        || !rect.w.is_finite()
        || !rect.h.is_finite()
        || rect.w <= 0.0
        || rect.h <= 0.0
        || rect.x + rect.w > 1.0 + EPSILON
        || rect.y + rect.h > 1.0 + EPSILON
    {
        errors.push(format!(
            "{label} must stay inside the normalized viewport with positive w/h"
        ));
    }
}

fn normalized(value: f64) -> bool {
    value.is_finite() && (-EPSILON..=1.0 + EPSILON).contains(&value)
}

const fn default_schema_version() -> u16 {
    PROFILE_SCHEMA_VERSION
}

const fn default_sensitivity() -> f64 {
    1.0
}

const fn default_recenter_threshold() -> f64 {
    0.7
}

fn known_key_name(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "1"
            | "2"
            | "3"
            | "4"
            | "5"
            | "6"
            | "7"
            | "8"
            | "9"
            | "a"
            | "b"
            | "c"
            | "d"
            | "e"
            | "f"
            | "g"
            | "h"
            | "i"
            | "j"
            | "k"
            | "l"
            | "m"
            | "n"
            | "o"
            | "p"
            | "q"
            | "r"
            | "s"
            | "t"
            | "u"
            | "v"
            | "w"
            | "x"
            | "y"
            | "z"
            | "space"
            | "tab"
            | "shift"
            | "ctrl"
            | "alt"
            // F1–F11 are bindable; F12 is reserved for the session capture
            // hotkey and is deliberately absent so profiles cannot bind a key
            // the session intercepts before the runtime sees it.
            | "f1"
            | "f2"
            | "f3"
            | "f4"
            | "f5"
            | "f6"
            | "f7"
            | "f8"
            | "f9"
            | "f10"
            | "f11"
            | "up"
            | "left"
            | "down"
            | "right"
            | "esc"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_profile_value() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 2,
            "name": "Shooter v2",
            "package_name": "com.example.shooter",
            "bindings": [{
                "name": "fire",
                "input": { "kind": "key", "key": "f" },
                "action": { "kind": "tap", "point": { "x": 0.86, "y": 0.50 } }
            }]
        })
    }

    fn profile_from_value(value: serde_json::Value) -> ProfileV2 {
        serde_json::from_value(value).unwrap()
    }

    fn validation_errors(value: serde_json::Value) -> String {
        profile_from_value(value)
            .validate()
            .unwrap_err()
            .errors
            .join("; ")
    }

    fn valid_profile() -> ProfileV2 {
        serde_json::from_str(
            r#"
            {
              "schema_version": 2,
              "name": "Shooter v2",
              "package_name": "com.example.shooter",
              "orientation": "landscape",
              "bindings": [
                {
                  "name": "movement",
                  "input": { "kind": "key_cluster", "up": "w", "left": "a", "down": "s", "right": "d" },
                  "action": { "kind": "virtual_joystick", "center": { "x": 0.18, "y": 0.78 }, "radius": 0.09, "dead_zone": 0.02, "mode": "hold", "reaffirm_ms": 50 }
                },
                {
                  "name": "fire",
                  "input": { "kind": "mouse_button", "button": "left" },
                  "action": { "kind": "tap", "point": { "x": 0.86, "y": 0.50 } }
                }
              ]
            }
            "#,
        )
        .unwrap()
    }

    #[test]
    fn valid_profile_passes() {
        valid_profile().validate().unwrap();
    }

    #[test]
    fn layers_and_binding_scopes_round_trip_with_both_activation_modes() {
        let mut value = simple_profile_value();
        value["layers"] = serde_json::json!([
            { "name": "grenades", "activation": { "kind": "hold", "key": "g" } },
            { "name": "equipment", "activation": { "kind": "toggle", "key": "q" } }
        ]);
        value["bindings"][0]["layer"] = serde_json::json!("grenades");
        value["bindings"][0]["modifier"] = serde_json::json!("shift");

        let profile = profile_from_value(value);
        profile.validate().unwrap();
        let serialized = serde_json::to_value(profile).unwrap();

        assert_eq!(
            serialized["layers"][0]["activation"],
            serde_json::json!({ "kind": "hold", "key": "g" })
        );
        assert_eq!(
            serialized["layers"][1]["activation"],
            serde_json::json!({ "kind": "toggle", "key": "q" })
        );
        assert_eq!(
            serialized["bindings"][0]["layer"],
            serde_json::json!("grenades")
        );
        assert_eq!(
            serialized["bindings"][0]["modifier"],
            serde_json::json!("shift")
        );
    }

    #[test]
    fn legacy_profiles_default_layers_and_omit_absent_binding_scope_fields() {
        let profile = profile_from_value(simple_profile_value());
        let serialized = serde_json::to_value(profile).unwrap();

        assert_eq!(serialized["layers"], serde_json::json!([]));
        assert!(serialized["bindings"][0].get("layer").is_none());
        assert!(serialized["bindings"][0].get("modifier").is_none());
    }

    #[test]
    fn layer_names_must_be_nonempty_unique_non_base_and_at_most_64() {
        let mut value = simple_profile_value();
        value["layers"] = serde_json::json!([
            { "name": " ", "activation": { "kind": "hold", "key": "g" } },
            { "name": "Combat", "activation": { "kind": "hold", "key": "h" } },
            { "name": "Combat", "activation": { "kind": "toggle", "key": "j" } },
            { "name": "BASE", "activation": { "kind": "hold", "key": "k" } }
        ]);
        let errors = validation_errors(value);
        assert!(errors.contains("layer name must not be empty"));
        assert!(errors.contains("duplicate layer name: Combat"));
        assert!(errors.contains("layer name base is reserved"));

        let mut value = simple_profile_value();
        value["layers"] = serde_json::Value::Array(
            (0..65)
                .map(|index| {
                    serde_json::json!({
                        "name": format!("layer-{index}"),
                        "activation": { "kind": "hold", "key": "a" }
                    })
                })
                .collect(),
        );
        assert!(validation_errors(value).contains("at most 64 layers"));

        let mut value = simple_profile_value();
        value["layers"] = serde_json::Value::Array(
            (0..64)
                .map(|index| {
                    serde_json::json!({
                        "name": format!("layer-{index}"),
                        "activation": { "kind": "hold", "key": "a" }
                    })
                })
                .collect(),
        );
        assert!(!validation_errors(value).contains("at most 64 layers"));
    }

    #[test]
    fn layer_activation_keys_must_be_known_unique_and_unused_by_base_key_bindings() {
        let mut value = simple_profile_value();
        value["layers"] = serde_json::json!([
            { "name": "grenades", "activation": { "kind": "hold", "key": "unknown" } },
            { "name": "equipment", "activation": { "kind": "toggle", "key": "g" } },
            { "name": "combat", "activation": { "kind": "hold", "key": "g" } },
            { "name": "base-clash", "activation": { "kind": "hold", "key": "f" } }
        ]);
        let errors = validation_errors(value);

        assert!(errors.contains("layer grenades uses unsupported activation key: unknown"));
        assert!(errors.contains("duplicate layer activation key: g"));
        assert!(errors.contains("layer activation key f cannot be used by a base-layer binding"));

        let mut value = simple_profile_value();
        value["layers"] = serde_json::json!([
            { "name": "grenades", "activation": { "kind": "hold", "key": "f" } }
        ]);
        value["bindings"][0]["modifier"] = serde_json::json!("ctrl");
        profile_from_value(value).validate().unwrap();

        let mut value = simple_profile_value();
        value["layers"] = serde_json::json!([
            { "name": "grenades", "activation": { "kind": "hold", "key": "g" } }
        ]);
        value["bindings"][0] = serde_json::json!({
            "name": "movement",
            "input": { "kind": "key_cluster", "up": "g", "left": "a", "down": "s", "right": "d" },
            "action": { "kind": "virtual_joystick", "center": { "x": 0.18, "y": 0.78 }, "radius": 0.09 }
        });
        assert!(validation_errors(value)
            .contains("layer activation key g cannot be used by a base-layer binding"));
    }

    #[test]
    fn bindings_must_reference_declared_layers_and_known_modifiers() {
        let mut value = simple_profile_value();
        value["bindings"][0]["layer"] = serde_json::json!("unknown");
        value["bindings"][0]["modifier"] = serde_json::json!("unknown");
        let errors = validation_errors(value);

        assert!(errors.contains("binding fire references unknown layer: unknown"));
        assert!(errors.contains("binding fire uses unsupported modifier: unknown"));
    }

    #[test]
    fn modifier_must_differ_from_every_input_key_and_is_invalid_for_mouse_move() {
        let mut value = simple_profile_value();
        value["bindings"][0]["modifier"] = serde_json::json!("f");
        let errors = validation_errors(value);
        assert!(errors.contains("binding fire modifier must differ from input key: f"));

        let mut value = simple_profile_value();
        value["bindings"][0] = serde_json::json!({
            "name": "movement",
            "modifier": "a",
            "input": { "kind": "key_cluster", "up": "w", "left": "a", "down": "s", "right": "d" },
            "action": { "kind": "virtual_joystick", "center": { "x": 0.18, "y": 0.78 }, "radius": 0.09 }
        });
        let errors = validation_errors(value);
        assert!(errors.contains("binding movement modifier must differ from key_cluster key: a"));

        let mut value = simple_profile_value();
        value["bindings"][0] = serde_json::json!({
            "name": "aim",
            "modifier": "shift",
            "input": { "kind": "mouse_move" },
            "action": { "kind": "mouse_aim", "region": { "x": 0.35, "y": 0.05, "w": 0.6, "h": 0.85 } }
        });
        assert!(validation_errors(value)
            .contains("binding aim cannot use a modifier with mouse_move input"));
    }

    #[test]
    fn ctrl_esc_and_ctrl_c_are_reserved_for_session_exit() {
        let mut value = simple_profile_value();
        value["bindings"] = serde_json::json!([
            {
                "name": "exit-escape",
                "modifier": "ctrl",
                "input": { "kind": "key", "key": "esc" },
                "action": { "kind": "tap", "point": { "x": 0.86, "y": 0.50 } }
            },
            {
                "name": "exit-c",
                "modifier": "ctrl",
                "input": { "kind": "key", "key": "c" },
                "action": { "kind": "tap", "point": { "x": 0.75, "y": 0.50 } }
            }
        ]);
        let errors = validation_errors(value);

        assert!(errors.contains(
            "binding exit-escape uses ctrl+esc, which is reserved for the session exit hotkey"
        ));
        assert!(errors
            .contains("binding exit-c uses ctrl+c, which is reserved for the session exit hotkey"));

        let mut value = simple_profile_value();
        value["bindings"][0] = serde_json::json!({
            "name": "exit-cluster",
            "modifier": "ctrl",
            "input": { "kind": "key_cluster", "up": "esc", "left": "a", "down": "c", "right": "d" },
            "action": { "kind": "virtual_joystick", "center": { "x": 0.18, "y": 0.78 }, "radius": 0.09 }
        });
        let errors = validation_errors(value);
        assert!(errors.contains(
            "binding exit-cluster uses ctrl+esc, which is reserved for the session exit hotkey"
        ));
        assert!(errors.contains(
            "binding exit-cluster uses ctrl+c, which is reserved for the session exit hotkey"
        ));
    }

    #[test]
    fn duplicate_input_keys_are_rejected_only_within_the_same_layer_and_modifier_scope() {
        let mut value = simple_profile_value();
        value["layers"] = serde_json::json!([
            { "name": "grenades", "activation": { "kind": "hold", "key": "g" } }
        ]);
        value["bindings"] = serde_json::json!([
            {
                "name": "base-fire",
                "input": { "kind": "key", "key": "f" },
                "action": { "kind": "tap", "point": { "x": 0.86, "y": 0.50 } }
            },
            {
                "name": "layer-fire",
                "layer": "grenades",
                "input": { "kind": "key", "key": "f" },
                "action": { "kind": "tap", "point": { "x": 0.75, "y": 0.50 } }
            },
            {
                "name": "shift-fire",
                "modifier": "shift",
                "input": { "kind": "key", "key": "f" },
                "action": { "kind": "tap", "point": { "x": 0.65, "y": 0.50 } }
            }
        ]);
        profile_from_value(value).validate().unwrap();

        let mut value = simple_profile_value();
        value["bindings"] = serde_json::json!([
            {
                "name": "primary-fire",
                "input": { "kind": "mouse_button", "button": "left" },
                "action": { "kind": "tap", "point": { "x": 0.86, "y": 0.50 } }
            },
            {
                "name": "secondary-fire",
                "input": { "kind": "mouse_button", "button": "left" },
                "action": { "kind": "tap", "point": { "x": 0.75, "y": 0.50 } }
            }
        ]);
        assert!(validation_errors(value).contains(
            "mouse_button left drives multiple bindings in base layer without a modifier"
        ));

        let mut value = simple_profile_value();
        value["layers"] = serde_json::json!([
            { "name": "grenades", "activation": { "kind": "hold", "key": "g" } }
        ]);
        value["bindings"] = serde_json::json!([
            {
                "name": "base-fire",
                "input": { "kind": "mouse_button", "button": "left" },
                "action": { "kind": "tap", "point": { "x": 0.86, "y": 0.50 } }
            },
            {
                "name": "layer-fire",
                "layer": "grenades",
                "input": { "kind": "mouse_button", "button": "left" },
                "action": { "kind": "tap", "point": { "x": 0.75, "y": 0.50 } }
            },
            {
                "name": "shift-fire",
                "modifier": "shift",
                "input": { "kind": "mouse_button", "button": "left" },
                "action": { "kind": "tap", "point": { "x": 0.65, "y": 0.50 } }
            }
        ]);
        profile_from_value(value).validate().unwrap();

        let mut value = simple_profile_value();
        value["bindings"] = serde_json::json!([
            {
                "name": "arrow-left",
                "input": { "kind": "key", "key": "left" },
                "action": { "kind": "tap", "point": { "x": 0.86, "y": 0.50 } }
            },
            {
                "name": "mouse-left",
                "input": { "kind": "mouse_button", "button": "left" },
                "action": { "kind": "tap", "point": { "x": 0.75, "y": 0.50 } }
            }
        ]);
        profile_from_value(value).validate().unwrap();

        let mut value = simple_profile_value();
        value["bindings"] = serde_json::json!([
            {
                "name": "movement-a",
                "input": { "kind": "key_cluster", "up": "w", "left": "a", "down": "s", "right": "d" },
                "action": { "kind": "virtual_joystick", "center": { "x": 0.18, "y": 0.78 }, "radius": 0.09 }
            },
            {
                "name": "movement-b",
                "input": { "kind": "key_cluster", "up": "w", "left": "j", "down": "k", "right": "l" },
                "action": { "kind": "virtual_joystick", "center": { "x": 0.80, "y": 0.78 }, "radius": 0.09 }
            }
        ]);
        assert!(validation_errors(value)
            .contains("key w drives multiple bindings in base layer without a modifier"));

        let mut value = simple_profile_value();
        value["bindings"][0] = serde_json::json!({
            "name": "repeated-movement-key",
            "input": { "kind": "key_cluster", "up": "w", "left": "w", "down": "s", "right": "d" },
            "action": { "kind": "virtual_joystick", "center": { "x": 0.18, "y": 0.78 }, "radius": 0.09 }
        });
        profile_from_value(value).validate().unwrap();
    }

    #[test]
    fn layer_activation_key_cannot_be_bound_inside_its_own_layer() {
        let mut value = simple_profile_value();
        value["layers"] = serde_json::json!([
            { "name": "grenades", "activation": { "kind": "hold", "key": "g" } }
        ]);
        value["bindings"][0]["layer"] = serde_json::json!("grenades");
        value["bindings"][0]["input"]["key"] = serde_json::json!("g");

        assert!(validation_errors(value).contains(
            "layer activation key g cannot be used by binding fire inside layer grenades"
        ));

        let mut value = simple_profile_value();
        value["layers"] = serde_json::json!([
            { "name": "grenades", "activation": { "kind": "hold", "key": "left" } }
        ]);
        value["bindings"][0]["layer"] = serde_json::json!("grenades");
        value["bindings"][0]["input"] =
            serde_json::json!({ "kind": "mouse_button", "button": "left" });
        profile_from_value(value).validate().unwrap();
    }

    #[test]
    fn incompatible_input_action_pairs_are_rejected() {
        let mut profile = valid_profile();
        profile.bindings[0].input = InputV2::Key {
            key: "w".to_owned(),
        };
        let errors = profile.validate().unwrap_err().errors.join("; ");
        assert!(errors.contains("virtual_joystick requires key_cluster"));

        let mut profile = valid_profile();
        profile.bindings[1].input = InputV2::MouseMove;
        let errors = profile.validate().unwrap_err().errors.join("; ");
        assert!(errors.contains("tap requires key or mouse_button"));
    }

    #[test]
    fn hold_action_round_trips_and_validates_its_point() {
        let action: ActionV2 =
            serde_json::from_str(r#"{"kind":"hold","point":{"x":0.91,"y":0.48}}"#).unwrap();
        assert_eq!(
            action,
            ActionV2::Hold {
                point: NormalizedPoint { x: 0.91, y: 0.48 }
            }
        );
        assert_eq!(
            serde_json::to_value(&action).unwrap()["kind"],
            serde_json::json!("hold")
        );

        let mut profile = valid_profile();
        profile.bindings[0].action = ActionV2::Hold {
            point: NormalizedPoint { x: 1.1, y: 0.48 },
        };
        let errors = profile.validate().unwrap_err().errors.join("; ");
        assert!(errors.contains("hold point"));
    }

    #[test]
    fn materializes_normalized_joystick_values() {
        let resolution = Resolution {
            width: 1920,
            height: 1080,
        };

        assert_eq!(
            NormalizedPoint { x: 0.18, y: 0.78 }.materialize(resolution),
            Point { x: 345, y: 842 }
        );
        assert_eq!(materialize_radius(0.09, resolution), 97);
        assert_eq!(materialize_dead_zone(0.02, 97, resolution), 22);
    }

    #[test]
    fn materialized_dead_zone_stays_smaller_than_runtime_radius() {
        let resolution = Resolution {
            width: 1920,
            height: 1080,
        };

        assert_eq!(materialize_dead_zone(0.0, 97, resolution), 0);
        assert_eq!(materialize_dead_zone(0.02, 1, resolution), 0);
        assert_eq!(materialize_dead_zone(0.09, 97, resolution), 96);
        assert_eq!(materialize_dead_zone(1.0, 97, resolution), 96);
    }

    #[test]
    fn duplicate_binding_names_fail() {
        let mut profile = valid_profile();
        profile.bindings[1].name = "movement".to_owned();

        let error = profile.validate().unwrap_err();

        assert!(error
            .errors
            .iter()
            .any(|item| item.contains("duplicate binding name")));
    }

    #[test]
    fn legacy_virtual_joystick_without_dead_zone_still_passes() {
        let profile: ProfileV2 = serde_json::from_str(
            r#"
            {
              "schema_version": 2,
              "name": "Legacy joystick",
              "package_name": "com.example.shooter",
              "bindings": [
                {
                  "name": "movement",
                  "input": { "kind": "key_cluster", "up": "w", "left": "a", "down": "s", "right": "d" },
                  "action": { "kind": "virtual_joystick", "center": { "x": 0.18, "y": 0.78 }, "radius": 0.09 }
                }
              ]
            }
            "#,
        )
        .unwrap();

        profile.validate().unwrap();
        assert!(matches!(
            profile.bindings[0].action,
            ActionV2::VirtualJoystick { dead_zone: 0.0, .. }
        ));
    }

    #[test]
    fn dead_zone_must_be_smaller_than_radius() {
        let mut profile = valid_profile();
        profile.bindings[0].action = ActionV2::VirtualJoystick {
            center: NormalizedPoint { x: 0.18, y: 0.78 },
            radius: 0.09,
            dead_zone: 0.09,
            mode: JoystickMode::Hold,
            reaffirm_ms: Some(50),
        };

        let error = profile.validate().unwrap_err();

        assert!(error
            .errors
            .iter()
            .any(|item| item.contains("dead_zone must be smaller than radius")));
    }

    #[test]
    fn legacy_mouse_aim_uses_comfort_defaults() {
        let action: ActionV2 = serde_json::from_str(
            r#"{
                "kind": "mouse_aim",
                "region": { "x": 0.35, "y": 0.05, "w": 0.6, "h": 0.85 },
                "sensitivity": 1.2
            }"#,
        )
        .unwrap();

        assert!(matches!(
            action,
            ActionV2::MouseAim {
                toggle_key: None,
                recenter_threshold,
                recenter_gap_ms: 0,
                ads_multiplier: None,
                reaffirm_ms: None,
                ..
            } if (recenter_threshold - 0.7).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn mouse_aim_comfort_fields_validate() {
        let mut profile = valid_profile();
        profile.bindings.push(BindingV2 {
            name: "aim".to_owned(),
            layer: None,
            modifier: None,
            input: InputV2::MouseMove,
            action: ActionV2::MouseAim {
                region: NormalizedRect {
                    x: 0.35,
                    y: 0.05,
                    w: 0.6,
                    h: 0.85,
                },
                sensitivity: 1.2,
                toggle_key: Some("tab".to_owned()),
                recenter_threshold: 0.7,
                recenter_gap_ms: 0,
                ads_multiplier: Some(0.6),
                reaffirm_ms: Some(50),
            },
        });

        profile.validate().unwrap();

        {
            let ActionV2::MouseAim {
                ref mut toggle_key, ..
            } = profile.bindings.last_mut().unwrap().action
            else {
                unreachable!()
            };
            *toggle_key = Some("f10".to_owned());
        }
        profile.validate().unwrap();

        let ActionV2::MouseAim {
            ref mut toggle_key,
            ref mut recenter_threshold,
            ref mut ads_multiplier,
            ref mut reaffirm_ms,
            ..
        } = profile.bindings.last_mut().unwrap().action
        else {
            unreachable!()
        };
        *toggle_key = Some("unknown".to_owned());
        *recenter_threshold = 0.05;
        *ads_multiplier = Some(1.2);
        *reaffirm_ms = Some(0);

        let errors = profile.validate().unwrap_err().errors.join("; ");
        assert!(errors.contains("toggle_key"));
        assert!(errors.contains("recenter_threshold"));
        assert!(errors.contains("ads_multiplier"));
        assert!(errors.contains("reaffirm_ms"));
    }

    #[test]
    fn saves_valid_profile_atomically_and_loads_it_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        let profile = valid_profile();

        profile.save_to_path(&path).unwrap();

        assert_eq!(ProfileV2::load_from_path(&path).unwrap(), profile);
        assert!(fs::read_to_string(path).unwrap().ends_with('\n'));
    }

    #[test]
    fn refuses_to_save_invalid_profile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        let mut profile = valid_profile();
        profile.name.clear();

        assert!(matches!(
            profile.save_to_path(&path),
            Err(ProfileV2SaveError::InvalidProfile(_))
        ));
        assert!(!path.exists());
    }
}
