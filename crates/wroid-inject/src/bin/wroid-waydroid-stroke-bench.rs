//! Measure one-way stroke latency from uinput injection to Android's evdev
//! view.
//!
//! Mouse aim is a continuous stream of relative moves, not taps: at a 1000 Hz
//! gaming mouse the container receives four to sixteen times more reports
//! than a physical touchscreen produces. This benchmark injects the same
//! on-screen stroke at several report rates — including a mid-stroke direction
//! reversal — and pairs every injected frame with the Android-side `getevent`
//! timestamp of the matching sync record. The container shares the host
//! kernel, so both sides read the same `CLOCK_MONOTONIC` and the difference
//! is the one-way pipeline latency.
//!
//! Run with sudo; it starts a temporary Waydroid session and removes the
//! temporary LXC input bridge afterwards.

use std::error::Error;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use wroid_core::Point;
use wroid_inject::{
    ensure_container_stopped, ensure_root, remove_default_bridge, wait_for_android_input_device,
    DesktopUser, DesktopWaydroidSession, DeviceConfig, InputDeviceNode, InstalledWaydroidBridge,
    UinputTouchInjector, WaydroidBridgeLease, WROID_TOUCHSCREEN_NAME,
};
use wroid_runtime::{ContactId, TouchEngine};

const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;
const CAPTURE_STARTUP_MS: u64 = 1_000;
const PHASE_QUIET_MS: u64 = 500;
const GETEVENT_TIMEOUT_GRACE_SECONDS: u64 = 10;
const STROKE_HALF_EVENTS: usize = 200;
const ORIGIN_X: u32 = 960;
const ORIGIN_Y: u32 = 540;

struct StrokeSpec {
    label: &'static str,
    events_per_half: usize,
    step: i32,
    interval: Duration,
}

struct PhaseSample {
    label: &'static str,
    /// `CLOCK_MONOTONIC` seconds for every submitted frame; the container
    /// shares the host kernel, so these align with `getevent -t` stamps.
    frames: Vec<f64>,
}

fn main() -> Result<(), Box<dyn Error>> {
    ensure_root("Waydroid stroke benchmark")?;
    let _bridge_lease = WaydroidBridgeLease::acquire_default("Waydroid stroke benchmark")?;

    ensure_container_stopped()?;
    remove_default_bridge()?;

    let desktop_user = DesktopUser::from_sudo_environment()?;
    let config = DeviceConfig::new(DEFAULT_WIDTH, DEFAULT_HEIGHT)?;
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
    let input_node = InputDeviceNode::from_path(&event_node)?;

    println!(
        "Created {WROID_TOUCHSCREEN_NAME} at {}",
        event_node.display()
    );
    let bridge = InstalledWaydroidBridge::install_default(&input_node)?;
    println!("Installed a temporary, reversible Waydroid LXC input bridge.");

    let mut session = DesktopWaydroidSession::start(desktop_user)?;
    let benchmark_result = (|| -> Result<BenchmarkReport, Box<dyn Error>> {
        wait_for_android_input_device(WROID_TOUCHSCREEN_NAME)?;
        println!("Android detected {WROID_TOUCHSCREEN_NAME}.");

        let specs = vec![
            StrokeSpec {
                label: "125hz",
                events_per_half: 50,
                step: 4,
                interval: Duration::from_millis(8),
            },
            StrokeSpec {
                label: "250hz",
                events_per_half: 100,
                step: 2,
                interval: Duration::from_millis(4),
            },
            StrokeSpec {
                label: "1000hz",
                events_per_half: STROKE_HALF_EVENTS,
                step: 1,
                interval: Duration::from_millis(1),
            },
        ];
        let expected_syncs: usize = specs.iter().map(|spec| spec.events_per_half * 2 + 2).sum();

        let capture = spawn_getevent_capture(&event_node, expected_syncs)?;
        println!("Waiting {CAPTURE_STARTUP_MS}ms for Android getevent capture to warm up.");
        sleep(Duration::from_millis(CAPTURE_STARTUP_MS));

        let mut engine = TouchEngine::new(injector);
        let mut phases = Vec::new();
        for spec in &specs {
            phases.push(run_stroke(spec, &mut engine)?);
            sleep(Duration::from_millis(PHASE_QUIET_MS));
        }

        let captured = combined_output(capture.wait_with_output()?);
        Ok(BenchmarkReport::new(phases, captured))
    })();

    let stop_result = session.stop();
    let cleanup_result = bridge.cleanup();

    let report = benchmark_result?;
    stop_result?;
    cleanup_result?;

    report.print();
    println!("Waydroid stopped and the temporary LXC bridge was removed.");
    Ok(())
}

/// One stroke: right for half the events, then immediately back left — the
/// sharp reversal the aim feel complaints are about. Every frame records the
/// host instant just before submission.
fn run_stroke(
    spec: &StrokeSpec,
    engine: &mut TouchEngine<UinputTouchInjector>,
) -> Result<PhaseSample, Box<dyn Error>> {
    let contact = ContactId::new(1);
    let origin = Point {
        x: ORIGIN_X,
        y: ORIGIN_Y,
    };
    let mut frames = Vec::with_capacity(spec.events_per_half * 2 + 2);
    let started = Instant::now();

    engine.begin_contact(contact, origin)?;
    frames.push(monotonic_seconds());

    for index in 0..spec.events_per_half * 2 {
        let direction = if index < spec.events_per_half { 1 } else { -1 };
        let target = Point {
            x: offset_axis(origin.x, i64::from(spec.step) * direction * index as i64),
            y: origin.y,
        };
        let due =
            started + Duration::from_millis((index as u64 + 1) * spec.interval.as_millis() as u64);
        pace(due);
        engine.move_contact(contact, target)?;
        frames.push(monotonic_seconds());
    }
    pace(
        started
            + Duration::from_millis(
                (spec.events_per_half as u64 + 1) * spec.interval.as_millis() as u64,
            ),
    );
    engine.end_contact(contact)?;
    frames.push(monotonic_seconds());

    println!(
        "{}: injected {} frames ({}px out, reversal, {}px back)",
        spec.label,
        frames.len(),
        spec.step * spec.events_per_half as i32,
        spec.step * spec.events_per_half as i32,
    );
    Ok(PhaseSample {
        label: spec.label,
        frames,
    })
}

/// Sleep until `due`, spinning the final stretch so a 1 ms cadence stays
/// honest against timer slack.
fn pace(due: Instant) {
    loop {
        let now = Instant::now();
        if now >= due {
            return;
        }
        let remaining = due - now;
        if remaining > Duration::from_millis(2) {
            sleep(remaining - Duration::from_millis(1));
        }
    }
}

fn monotonic_seconds() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: plain clock read into a local timespec.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

fn offset_axis(value: u32, delta: i64) -> u32 {
    let value = i64::from(value) + delta;
    value.clamp(0, i64::from(u32::MAX)) as u32
}

fn spawn_getevent_capture(event_node: &Path, expected_syncs: usize) -> io::Result<Child> {
    let event_path = event_node.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "event node path is not UTF-8")
    })?;
    let capture_count = expected_syncs.saturating_mul(8).max(1);
    let timeout_seconds = 30_u64.max(GETEVENT_TIMEOUT_GRACE_SECONDS);

    println!(
        "Capturing up to {capture_count} Android getevent record(s), timeout={timeout_seconds}s."
    );

    Command::new("timeout")
        .args([
            format!("{timeout_seconds}s"),
            "waydroid".to_owned(),
            "shell".to_owned(),
            "--".to_owned(),
            "getevent".to_owned(),
            "-t".to_owned(),
            "-c".to_owned(),
            capture_count.to_string(),
            event_path.to_owned(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn combined_output(output: Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    format!("{stdout}{stderr}")
}

#[derive(Debug)]
struct BenchmarkReport {
    phases: Vec<PhaseStats>,
    android_syncs: usize,
    android_downs: usize,
    android_ups: usize,
    unparsed_syncs: usize,
}

#[derive(Debug)]
struct PhaseStats {
    label: &'static str,
    injected: usize,
    received: usize,
    latency_p50_ms: f64,
    latency_p95_ms: f64,
    latency_max_ms: f64,
}

impl BenchmarkReport {
    fn new(phases: Vec<PhaseSample>, captured: String) -> Self {
        let mut sync_timestamps = Vec::new();
        let mut android_syncs = 0;
        let android_downs = count_any(
            &captured,
            &["0001 014a 00000001", "BTN_TOUCH             DOWN"],
        );
        let android_ups = count_any(
            &captured,
            &["0001 014a 00000000", "BTN_TOUCH             UP"],
        );
        let mut unparsed_syncs = 0;

        for line in captured.lines() {
            let is_sync = line.contains("0000 0000 00000000") || line.contains("SYN_REPORT");
            if !is_sync {
                continue;
            }
            android_syncs += 1;
            match line_timestamp_seconds(line) {
                Some(seconds) => sync_timestamps.push(seconds),
                None => unparsed_syncs += 1,
            }
        }

        let mut stats = Vec::new();
        let mut cursor = 0_usize;
        for phase in &phases {
            let mut latencies = Vec::with_capacity(phase.frames.len());
            for frame in &phase.frames {
                if cursor >= sync_timestamps.len() {
                    break;
                }
                let android_seconds = sync_timestamps[cursor];
                cursor += 1;
                let latency = android_seconds - frame;
                if latency.is_finite() && latency >= 0.0 {
                    latencies.push(latency * 1_000.0);
                }
            }
            latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let percentile = |fraction: f64| -> f64 {
                latencies
                    .get(((latencies.len() as f64) * fraction).min(1.0) as usize - 1)
                    .copied()
                    .unwrap_or(f64::NAN)
            };
            stats.push(PhaseStats {
                label: phase.label,
                injected: phase.frames.len(),
                received: latencies.len(),
                latency_p50_ms: percentile(0.50),
                latency_p95_ms: percentile(0.95),
                latency_max_ms: latencies.last().copied().unwrap_or(f64::NAN),
            });
        }

        // The down/up counts span all phases; strokes are one contact each.

        Self {
            phases: stats,
            android_syncs,
            android_downs,
            android_ups,
            unparsed_syncs,
        }
    }

    fn print(&self) {
        println!("\nStroke latency report (uinput -> Android evdev):");
        println!(
            "  {:<8} {:>9} {:>9} {:>9} {:>9} {:>9}",
            "phase", "injected", "received", "p50 ms", "p95 ms", "max ms"
        );
        for phase in &self.phases {
            println!(
                "  {:<8} {:>9} {:>9} {:>9.2} {:>9.2} {:>9.2}",
                phase.label,
                phase.injected,
                phase.received,
                phase.latency_p50_ms,
                phase.latency_p95_ms,
                phase.latency_max_ms
            );
        }
        println!(
            "  android syncs: {} (unparsed: {}), BTN_TOUCH down: {}, up: {}",
            self.android_syncs, self.unparsed_syncs, self.android_downs, self.android_ups
        );
    }
}

fn line_timestamp_seconds(line: &str) -> Option<f64> {
    let start = line.find('[')?;
    let end = line[start..].find(']')? + start;
    line[start + 1..end].trim().parse::<f64>().ok()
}

fn count_any(haystack: &str, needles: &[&str]) -> usize {
    needles
        .iter()
        .map(|needle| haystack.matches(needle).count())
        .max()
        .unwrap_or(0)
}
