use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use wroid_core::profile_v2::{
    materialize_radius, ActionV2, InputV2, JoystickMode, NormalizedPoint, ProfileV2,
};
use wroid_core::Resolution;
use wroid_inject::{
    cleanup_live_keyboard_bridge, run_live_keyboard_session, KeyTapBinding, LiveKeyboardOptions,
    DEFAULT_HOLD_LOG_INTERVAL, DEFAULT_LIVE_HEIGHT, DEFAULT_LIVE_WIDTH, DEFAULT_READY_DELAY,
    DEFAULT_REAFFIRM_INTERVAL,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

fn main() -> Result<()> {
    let Some(options) = Options::parse(std::env::args().skip(1))? else {
        print_usage();
        return Ok(());
    };

    if options.cleanup {
        return cleanup_live_keyboard_bridge();
    }

    let profile_path = options.profile_path.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "profile path is required unless --cleanup is used",
        )
    })?;
    let keyboard_path = options.keyboard_path.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "keyboard event node is required unless --cleanup is used",
        )
    })?;

    let profile = ProfileV2::load_from_path(profile_path)?;
    if let Err(error) = profile.validate() {
        eprintln!("Profile v2 validation failed:");
        for item in &error.errors {
            eprintln!("  - {item}");
        }
        return Err(Box::new(error));
    }

    let movement = find_wasd_joystick(&profile)?;
    let resolution = Resolution {
        width: options.width,
        height: options.height,
    };
    let key_taps = find_key_taps(&profile, resolution);

    let mut live =
        LiveKeyboardOptions::with_resolution(keyboard_path, options.width, options.height);
    live.joystick_center = movement.center.materialize(resolution);
    live.joystick_radius = materialize_radius(movement.radius, resolution);
    live.key_taps = key_taps;
    live.grab = options.grab;
    live.show_ui = options.show_ui;
    live.trace_android = options.trace_android;
    live.reaffirm_interval = options.reaffirm_override.unwrap_or_else(|| {
        movement
            .reaffirm_ms
            .map(Duration::from_millis)
            .or(Some(DEFAULT_REAFFIRM_INTERVAL))
    });
    live.hold_log_interval = options.hold_log_interval;
    live.ready_delay = options.ready_delay;

    println!(
        "Profile v{} OK: {} ({}) orientation={:?}",
        profile.schema_version, profile.name, profile.package_name, profile.orientation
    );
    println!(
        "Using binding '{}' as WASD virtual joystick: center={},{} radius={}px reaffirm={:?}",
        movement.name,
        live.joystick_center.x,
        live.joystick_center.y,
        live.joystick_radius,
        live.reaffirm_interval.map(|duration| duration.as_millis())
    );
    if live.key_taps.is_empty() {
        println!("No key tap bindings found in profile v2.");
    } else {
        println!(
            "Loaded {} key tap binding(s): {}",
            live.key_taps.len(),
            live.key_taps
                .iter()
                .map(|binding| format!("{}->{},{}", binding.key, binding.point.x, binding.point.y))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    run_live_keyboard_session(live)
}

#[derive(Debug, Clone)]
struct MovementBinding {
    name: String,
    center: NormalizedPoint,
    radius: f64,
    reaffirm_ms: Option<u64>,
}

fn find_wasd_joystick(profile: &ProfileV2) -> Result<MovementBinding> {
    for binding in &profile.bindings {
        if binding.layer.is_some() || binding.modifier.is_some() {
            continue;
        }
        let InputV2::KeyCluster {
            up,
            left,
            down,
            right,
        } = &binding.input
        else {
            continue;
        };
        if !key_eq(up, "w") || !key_eq(left, "a") || !key_eq(down, "s") || !key_eq(right, "d") {
            continue;
        }

        let ActionV2::VirtualJoystick {
            center,
            radius,
            mode,
            reaffirm_ms,
            dead_zone: _,
            ..
        } = &binding.action
        else {
            continue;
        };
        if *mode != JoystickMode::Hold {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "binding '{}' uses virtual_joystick mode {:?}; profile smoke currently supports hold mode only",
                    binding.name, mode
                ),
            )
            .into());
        }

        return Ok(MovementBinding {
            name: binding.name.clone(),
            center: *center,
            radius: *radius,
            reaffirm_ms: *reaffirm_ms,
        });
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "profile does not contain a WASD key_cluster binding with a hold-mode virtual_joystick action",
    )
    .into())
}

fn find_key_taps(profile: &ProfileV2, resolution: Resolution) -> Vec<KeyTapBinding> {
    profile
        .bindings
        .iter()
        .filter_map(|binding| {
            if binding.layer.is_some() || binding.modifier.is_some() {
                return None;
            }
            let (InputV2::Key { key }, ActionV2::Tap { point, .. }) =
                (&binding.input, &binding.action)
            else {
                return None;
            };
            Some(KeyTapBinding {
                key: key.trim().to_ascii_lowercase(),
                point: point.materialize(resolution),
            })
        })
        .collect()
}

fn key_eq(value: &str, expected: &str) -> bool {
    value.trim().eq_ignore_ascii_case(expected)
}

#[derive(Debug)]
struct Options {
    profile_path: Option<PathBuf>,
    keyboard_path: Option<PathBuf>,
    width: u32,
    height: u32,
    grab: bool,
    show_ui: bool,
    trace_android: bool,
    reaffirm_override: Option<Option<Duration>>,
    hold_log_interval: Option<Duration>,
    ready_delay: Duration,
    cleanup: bool,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Self>> {
        let mut positional = Vec::new();
        let mut options = Self {
            profile_path: None,
            keyboard_path: None,
            width: DEFAULT_LIVE_WIDTH,
            height: DEFAULT_LIVE_HEIGHT,
            grab: true,
            show_ui: true,
            trace_android: true,
            reaffirm_override: None,
            hold_log_interval: Some(DEFAULT_HOLD_LOG_INTERVAL),
            ready_delay: DEFAULT_READY_DELAY,
            cleanup: false,
        };

        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--help" | "-h" => return Ok(None),
                "--cleanup" => options.cleanup = true,
                "--grab" => options.grab = true,
                "--no-grab" => options.grab = false,
                "--no-ui" => options.show_ui = false,
                "--no-trace" => options.trace_android = false,
                "--no-reaffirm" => options.reaffirm_override = Some(None),
                "--no-hold-log" => options.hold_log_interval = None,
                "--reaffirm-ms" => {
                    options.reaffirm_override =
                        Some(Some(parse_positive_millis(&mut args, "--reaffirm-ms")?));
                }
                "--hold-log-ms" => {
                    options.hold_log_interval =
                        Some(parse_positive_millis(&mut args, "--hold-log-ms")?);
                }
                "--ready-delay-ms" => {
                    options.ready_delay = parse_millis(&mut args, "--ready-delay-ms")?;
                }
                "--width" => {
                    options.width = parse_next(&mut args, "--width")?;
                    if options.width == 0 {
                        return Err(invalid_input("--width must be greater than zero"));
                    }
                }
                "--height" => {
                    options.height = parse_next(&mut args, "--height")?;
                    if options.height == 0 {
                        return Err(invalid_input("--height must be greater than zero"));
                    }
                }
                value if value.starts_with("--") => {
                    return Err(invalid_input(format!("unknown option: {value}")));
                }
                value => positional.push(PathBuf::from(value)),
            }
        }

        if options.cleanup {
            return Ok(Some(options));
        }
        if positional.len() != 2 {
            return Err(invalid_input(
                "expected: <profile-v2.json> <keyboard-event-node>; use --help for usage",
            ));
        }
        options.profile_path = Some(positional.remove(0));
        options.keyboard_path = Some(positional.remove(0));

        Ok(Some(options))
    }
}

fn parse_positive_millis(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<Duration> {
    let duration = parse_millis(args, flag)?;
    if duration.is_zero() {
        return Err(invalid_input(format!("{flag} must be greater than zero")));
    }
    Ok(duration)
}

fn parse_millis(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<Duration> {
    let millis: u64 = parse_next(args, flag)?;
    Ok(Duration::from_millis(millis))
}

fn parse_next<T>(args: &mut impl Iterator<Item = String>, label: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: Error + Send + Sync + 'static,
{
    let value = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing {label}")))?;
    value.parse::<T>().map_err(|source| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {label} '{value}': {source}"),
        )
        .into()
    })
}

fn invalid_input(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    io::Error::new(io::ErrorKind::InvalidInput, message.into()).into()
}

fn print_usage() {
    println!(
        "Usage: wroid-waydroid-profile-smoke <profile-v2.json> <keyboard-event-node> [--width W] [--height H] [--no-grab] [--no-ui] [--no-trace] [--ready-delay-ms N] [--reaffirm-ms N|--no-reaffirm] [--hold-log-ms N|--no-hold-log]"
    );
    println!(
        "Example: sudo ./target/release/wroid-waydroid-profile-smoke profiles/examples/shooter-v2.json /dev/input/event7 --width 1920 --height 1080 --no-trace"
    );
    println!("Recovery: sudo ./target/release/wroid-waydroid-profile-smoke --cleanup");
}

#[cfg(test)]
mod tests {
    use super::*;
    use wroid_core::profile_v2::{BindingV2, LayerActivation, LayerV2};

    fn layered_profile() -> ProfileV2 {
        ProfileV2 {
            schema_version: 2,
            name: "Layered smoke".to_owned(),
            package_name: "com.example.layered".to_owned(),
            orientation: Default::default(),
            layers: vec![LayerV2 {
                name: "grenades".to_owned(),
                activation: LayerActivation::Hold {
                    key: "g".to_owned(),
                },
            }],
            bindings: vec![
                joystick_binding(
                    "layer movement",
                    Some("grenades"),
                    None,
                    JoystickMode::Toggle,
                    0.1,
                ),
                joystick_binding(
                    "modified movement",
                    None,
                    Some("shift"),
                    JoystickMode::Hold,
                    0.2,
                ),
                joystick_binding("base movement", None, None, JoystickMode::Hold, 0.3),
                tap_binding("layer tap", "1", Some("grenades"), None, 0.1),
                tap_binding("modified tap", "2", None, Some("shift"), 0.2),
                tap_binding("base tap", "2", None, None, 0.3),
                tap_binding("other base tap", "3", None, None, 0.4),
            ],
        }
    }

    fn joystick_binding(
        name: &str,
        layer: Option<&str>,
        modifier: Option<&str>,
        mode: JoystickMode,
        coordinate: f64,
    ) -> BindingV2 {
        BindingV2 {
            name: name.to_owned(),
            layer: layer.map(str::to_owned),
            modifier: modifier.map(str::to_owned),
            input: InputV2::KeyCluster {
                up: "w".to_owned(),
                left: "a".to_owned(),
                down: "s".to_owned(),
                right: "d".to_owned(),
            },
            action: ActionV2::VirtualJoystick {
                center: NormalizedPoint {
                    x: coordinate,
                    y: coordinate,
                },
                radius: 0.1,
                dead_zone: 0.0,
                mode,
                reaffirm_ms: None,
                slow_key: None,
                slow_scale: None,
            },
        }
    }

    fn tap_binding(
        name: &str,
        key: &str,
        layer: Option<&str>,
        modifier: Option<&str>,
        coordinate: f64,
    ) -> BindingV2 {
        BindingV2 {
            name: name.to_owned(),
            layer: layer.map(str::to_owned),
            modifier: modifier.map(str::to_owned),
            input: InputV2::Key {
                key: key.to_owned(),
            },
            action: ActionV2::Tap {
                point: NormalizedPoint {
                    x: coordinate,
                    y: coordinate,
                },
                hold: false,
            },
        }
    }

    #[test]
    fn smoke_discovers_only_base_unmodified_bindings() {
        let profile = layered_profile();
        profile.validate().unwrap();

        let movement = find_wasd_joystick(&profile).unwrap();
        let taps = find_key_taps(
            &profile,
            Resolution {
                width: 100,
                height: 100,
            },
        );

        assert_eq!(movement.name, "base movement");
        assert_eq!(movement.center, NormalizedPoint { x: 0.3, y: 0.3 });
        assert_eq!(
            taps.iter()
                .map(|binding| binding.key.as_str())
                .collect::<Vec<_>>(),
            ["2", "3"]
        );
    }
}
