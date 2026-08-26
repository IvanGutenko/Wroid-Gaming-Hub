use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;

use wroid_core::profile_v2::{
    materialize_axis, materialize_dead_zone, materialize_radius, ActionV2, BindingV2, ProfileV2,
};
use wroid_core::{Point, Resolution};

fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = Options::parse(env::args().skip(1))? else {
        print_usage();
        return Ok(());
    };

    let profile = ProfileV2::load_from_path(&options.path)?;
    if let Err(error) = profile.validate() {
        eprintln!("Profile v2 validation failed:");
        for item in &error.errors {
            eprintln!("  - {item}");
        }
        return Err(Box::new(error));
    }

    println!(
        "Profile v{} OK: {} ({}) orientation={:?} with {} binding(s)",
        profile.schema_version,
        profile.name,
        profile.package_name,
        profile.orientation,
        profile.bindings.len()
    );
    if let Some(resolution) = options.materialize {
        print_materialized_bindings(&profile, resolution);
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    path: PathBuf,
    materialize: Option<Resolution>,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Self>, io::Error> {
        let mut args = args.into_iter();
        let Some(first) = args.next() else {
            return Ok(None);
        };
        if matches!(first.as_str(), "-h" | "--help") {
            return Ok(None);
        }

        let mut materialize = None;
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--materialize" => {
                    let width = parse_dimension(args.next(), "width")?;
                    let height = parse_dimension(args.next(), "height")?;
                    materialize = Some(Resolution { width, height });
                }
                value => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument: {value}"),
                    ))
                }
            }
        }

        Ok(Some(Self {
            path: PathBuf::from(first),
            materialize,
        }))
    }
}

fn parse_dimension(value: Option<String>, label: &str) -> Result<u32, io::Error> {
    let value = value.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("missing {label} after --materialize"),
        )
    })?;
    let parsed = value.parse::<u32>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {label} '{value}': {error}"),
        )
    })?;
    if parsed == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} must be greater than zero"),
        ));
    }
    Ok(parsed)
}

fn print_materialized_bindings(profile: &ProfileV2, resolution: Resolution) {
    println!("Materialized coordinates for {resolution}:");
    for binding in &profile.bindings {
        print_materialized_action(
            &format!("{} [{}]", binding.name, binding_context(binding)),
            &binding.action,
            resolution,
            1,
        );
    }
}

fn binding_context(binding: &BindingV2) -> String {
    format!(
        "layer={} modifier={}",
        binding.layer.as_deref().unwrap_or("base"),
        binding.modifier.as_deref().unwrap_or("none")
    )
}

fn print_materialized_action(label: &str, action: &ActionV2, resolution: Resolution, depth: usize) {
    let indent = "  ".repeat(depth);
    match action {
        ActionV2::Tap { point, .. } => {
            println!("{indent}{label}: tap {}", point.materialize(resolution));
        }
        ActionV2::Hold { point } => {
            println!("{indent}{label}: hold {}", point.materialize(resolution));
        }
        ActionV2::VirtualJoystick {
            center,
            radius,
            dead_zone,
            mode,
            reaffirm_ms,
            slow_key,
            slow_scale,
        } => {
            let center = center.materialize(resolution);
            let radius_px = materialize_radius(*radius, resolution);
            let dead_zone_px = materialize_dead_zone(*dead_zone, radius_px, resolution);
            println!(
                "{indent}{label}: virtual_joystick center={center} radius={radius_px}px dead_zone={dead_zone_px}px mode={mode:?} reaffirm_ms={reaffirm_ms:?} slow_key={slow_key:?} slow_scale={slow_scale:?}"
            );
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
            let origin = Point {
                x: materialize_axis(region.x + region.w / 2.0, resolution.width),
                y: materialize_axis(region.y + region.h / 2.0, resolution.height),
            };
            let size = Point {
                x: materialize_axis(region.w, resolution.width),
                y: materialize_axis(region.h, resolution.height),
            };
            println!(
                "{indent}{label}: mouse_aim center={origin} size={},{} sensitivity={sensitivity} toggle_key={toggle_key:?} recenter_threshold={recenter_threshold} recenter_gap_ms={recenter_gap_ms} ads_multiplier={ads_multiplier:?} reaffirm_ms={reaffirm_ms:?}",
                size.x, size.y
            );
        }
        ActionV2::RadialSelect {
            center,
            distance,
            direction,
        } => {
            println!(
                "{indent}{label}: radial_select center={} distance={distance} direction={direction:?}",
                center.materialize(resolution)
            );
        }
        ActionV2::Macro { steps } => {
            println!("{indent}{label}: macro with {} step(s)", steps.len());
            for (index, step) in steps.iter().enumerate() {
                print_materialized_action(&format!("step[{index}]"), step, resolution, depth + 1);
            }
        }
    }
}

fn print_usage() {
    println!("Usage: wroid-profile-v2-validate <profile.json> [--materialize <width> <height>]");
}

#[cfg(test)]
mod tests {
    use super::*;
    use wroid_core::profile_v2::{BindingV2, InputV2, NormalizedPoint};

    #[test]
    fn parses_materialize_dimensions() {
        let options = Options::parse([
            "profile.json".to_owned(),
            "--materialize".to_owned(),
            "1920".to_owned(),
            "1080".to_owned(),
        ])
        .unwrap()
        .unwrap();

        assert_eq!(options.path, PathBuf::from("profile.json"));
        assert_eq!(
            options.materialize,
            Some(Resolution {
                width: 1920,
                height: 1080
            })
        );
    }

    #[test]
    fn rejects_zero_materialize_dimension() {
        let error = Options::parse([
            "profile.json".to_owned(),
            "--materialize".to_owned(),
            "0".to_owned(),
            "1080".to_owned(),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn materialized_binding_context_names_layer_and_modifier() {
        let binding = BindingV2 {
            name: "frag".to_owned(),
            layer: Some("grenades".to_owned()),
            modifier: Some("shift".to_owned()),
            input: InputV2::Key {
                key: "1".to_owned(),
            },
            action: ActionV2::Tap {
                point: NormalizedPoint { x: 0.7, y: 0.3 },
                hold: false,
            },
        };

        assert_eq!(binding_context(&binding), "layer=grenades modifier=shift");

        let mut base = binding;
        base.layer = None;
        base.modifier = None;
        assert_eq!(binding_context(&base), "layer=base modifier=none");
    }
}
