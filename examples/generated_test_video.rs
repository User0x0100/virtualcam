//! Generates a moving test pattern and publishes it to installed virtual cameras.
//!
//! By default both OBS Virtual Camera and Unity Capture receive the same
//! 1280x720, 30 FPS pattern for 30 seconds. No external video file is used.

use std::{
    env,
    error::Error,
    io::{Error as IoError, ErrorKind},
    thread,
    time::{Duration, Instant},
};

use virtualcam::{CameraConfig, CameraType, PixelFormat, VirtualCamera, WriteStatus};

const USAGE: &str = "\
Generated virtual-camera test video

Usage:
  cargo run --release --example generated_test_video -- [options]

Options:
  --camera <both|obs|unity>  Cameras to exercise (default: both)
  --duration <seconds>      Wall-clock duration (default: 30)
  --width <pixels>          Frame width (default: 1280)
  --height <pixels>         Frame height (default: 720)
  --fps <frames/second>     Nominal frame rate (default: 30)
  -h, --help                Show this help

When Unity Capture has no active consumer, the example keeps polling and starts
publishing automatically once device #1 is opened by a consumer application.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CameraSelection {
    Both,
    Obs,
    Unity,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Settings {
    selection: CameraSelection,
    duration_seconds: f64,
    width: u32,
    height: u32,
    fps: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            selection: CameraSelection::Both,
            duration_seconds: 30.0,
            width: 1280,
            height: 720,
            fps: 30.0,
        }
    }
}

#[derive(Default)]
struct StatusCounts {
    written: u64,
    skipped_by_consumer: u64,
    consumer_inactive: u64,
}

struct CameraTarget {
    name: &'static str,
    camera: VirtualCamera,
    frame: Vec<u8>,
    counts: StatusCounts,
    consumer_was_inactive: bool,
}

#[derive(Clone, Copy)]
struct PatternState {
    stripe_x: usize,
    stripe_width: usize,
    scan_y: usize,
    square_x: usize,
    square_y: usize,
    square_size: usize,
    checker_phase: usize,
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments
        .iter()
        .any(|argument| argument == "-h" || argument == "--help")
    {
        print!("{USAGE}");
        return Ok(());
    }

    let settings = parse_args(arguments).map_err(invalid_input)?;
    run(settings)
}

fn run(settings: Settings) -> Result<(), Box<dyn Error>> {
    let config = CameraConfig {
        width: settings.width,
        height: settings.height,
        fps: settings.fps,
    };
    let mut targets = Vec::new();
    for camera_type in selected_camera_types(settings.selection) {
        let camera = VirtualCamera::open(camera_type, config.clone()).map_err(|error| {
            IoError::other(format!(
                "failed to open {}: {error}",
                camera_name(camera_type)
            ))
        })?;
        println!(
            "opened {}: {}x{} at {:.3} FPS, {:?}, {} bytes/frame",
            camera_name(camera_type),
            settings.width,
            settings.height,
            settings.fps,
            camera.native_format(),
            camera.frame_size()
        );
        targets.push(CameraTarget {
            name: camera_name(camera_type),
            frame: vec![0; camera.frame_size()],
            camera,
            counts: StatusCounts::default(),
            consumer_was_inactive: false,
        });
    }

    println!(
        "generating the pattern for {:.3} seconds; press Ctrl+C to stop the process",
        settings.duration_seconds
    );
    let duration = Duration::from_secs_f64(settings.duration_seconds);
    let frame_interval = Duration::from_secs_f64(1.0 / settings.fps);
    let scheduled_frames = (settings.duration_seconds * settings.fps).ceil() as u64;
    let start = Instant::now();
    let end = start + duration;
    let mut scheduler_skips = 0_u64;
    let mut last_report = start;

    for frame_index in 0..scheduled_frames {
        let target_time = start + Duration::from_secs_f64(frame_index as f64 / settings.fps);
        let now = Instant::now();
        if now < target_time {
            thread::sleep(target_time - now);
        }

        let now = Instant::now();
        if now >= end {
            break;
        }
        if now.saturating_duration_since(target_time) >= frame_interval {
            scheduler_skips += 1;
            continue;
        }

        for target in &mut targets {
            fill_pattern(
                target.camera.native_format(),
                &mut target.frame,
                settings.width,
                settings.height,
                frame_index,
                settings.fps,
            );
            let status = target.camera.write_host(&target.frame).map_err(|error| {
                IoError::other(format!("{} frame publication failed: {error}", target.name))
            })?;
            record_status(target, status);
        }

        let now = Instant::now();
        if now.saturating_duration_since(last_report) >= Duration::from_secs(1) {
            println!(
                "elapsed {:>6.2}/{:.2} seconds, scheduler skips: {}",
                now.saturating_duration_since(start).as_secs_f64(),
                settings.duration_seconds,
                scheduler_skips
            );
            last_report = now;
        }
    }

    let now = Instant::now();
    if now < end {
        thread::sleep(end - now);
    }

    for target in &mut targets {
        target
            .camera
            .close()
            .map_err(|error| IoError::other(format!("failed to close {}: {error}", target.name)))?;
        println!(
            "{} summary: written={}, skipped-by-consumer={}, consumer-inactive={}",
            target.name,
            target.counts.written,
            target.counts.skipped_by_consumer,
            target.counts.consumer_inactive
        );
    }
    println!("scheduler skips: {scheduler_skips}");
    Ok(())
}

fn record_status(target: &mut CameraTarget, status: WriteStatus) {
    match status {
        WriteStatus::Written => target.counts.written += 1,
        WriteStatus::FrameSkippedByConsumer => target.counts.skipped_by_consumer += 1,
        WriteStatus::ConsumerInactive => target.counts.consumer_inactive += 1,
    }

    let inactive = status == WriteStatus::ConsumerInactive;
    if inactive && !target.consumer_was_inactive {
        println!(
            "{} consumer is inactive; continuing to poll for it",
            target.name
        );
    } else if !inactive && target.consumer_was_inactive {
        println!("{} consumer became active", target.name);
    }
    target.consumer_was_inactive = inactive;
}

fn selected_camera_types(selection: CameraSelection) -> Vec<CameraType> {
    match selection {
        CameraSelection::Both => vec![CameraType::Obs, CameraType::UnityCapture],
        CameraSelection::Obs => vec![CameraType::Obs],
        CameraSelection::Unity => vec![CameraType::UnityCapture],
    }
}

const fn camera_name(camera_type: CameraType) -> &'static str {
    match camera_type {
        CameraType::Obs => "OBS Virtual Camera",
        CameraType::UnityCapture => "Unity Capture",
    }
}

fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<Settings, String> {
    let mut settings = Settings::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        let value = match argument.as_str() {
            "--camera" | "--duration" | "--width" | "--height" | "--fps" => arguments
                .next()
                .ok_or_else(|| format!("{argument} requires a value"))?,
            _ => return Err(format!("unknown argument: {argument}\n\n{USAGE}")),
        };

        match argument.as_str() {
            "--camera" => {
                settings.selection = match value.to_ascii_lowercase().as_str() {
                    "both" => CameraSelection::Both,
                    "obs" => CameraSelection::Obs,
                    "unity" => CameraSelection::Unity,
                    _ => return Err("--camera must be one of: both, obs, unity".to_owned()),
                };
            }
            "--duration" => settings.duration_seconds = parse_number(&argument, &value)?,
            "--width" => settings.width = parse_number(&argument, &value)?,
            "--height" => settings.height = parse_number(&argument, &value)?,
            "--fps" => settings.fps = parse_number(&argument, &value)?,
            _ => unreachable!("known argument matched above"),
        }
    }

    validate_settings(settings)?;
    Ok(settings)
}

fn parse_number<T>(argument: &str, value: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .parse()
        .map_err(|_| format!("{argument} has an invalid value: {value}"))
}

fn validate_settings(settings: Settings) -> Result<(), String> {
    if settings.width == 0 || settings.height == 0 {
        return Err("--width and --height must be greater than zero".to_owned());
    }
    if !settings.duration_seconds.is_finite()
        || settings.duration_seconds <= 0.0
        || settings.duration_seconds > 86_400.0
    {
        return Err("--duration must be finite and in the range (0, 86400]".to_owned());
    }
    if !settings.fps.is_finite() || settings.fps <= 0.0 || settings.fps > 240.0 {
        return Err("--fps must be finite and in the range (0, 240]".to_owned());
    }
    if matches!(
        settings.selection,
        CameraSelection::Both | CameraSelection::Obs
    ) && (!settings.width.is_multiple_of(2) || !settings.height.is_multiple_of(2))
    {
        return Err("OBS NV12 output requires even --width and --height".to_owned());
    }
    Ok(())
}

fn invalid_input(message: String) -> IoError {
    IoError::new(ErrorKind::InvalidInput, message)
}

fn fill_pattern(
    format: PixelFormat,
    frame: &mut [u8],
    width: u32,
    height: u32,
    frame_index: u64,
    fps: f64,
) {
    let width = width as usize;
    let height = height as usize;
    let state = pattern_state(width, height, frame_index, fps);
    match format {
        PixelFormat::Rgba8 => fill_rgba(frame, width, height, state),
        PixelFormat::Nv12 => fill_nv12(frame, width, height, state),
    }
}

fn pattern_state(width: usize, height: usize, frame_index: u64, fps: f64) -> PatternState {
    let elapsed = frame_index as f64 / fps;
    let square_size = (width.min(height) / 7).max(1);
    PatternState {
        stripe_x: ((elapsed * width as f64 / 4.0) as usize) % width,
        stripe_width: (width / 80).max(1),
        scan_y: ((elapsed * height as f64 / 2.0) as usize) % height,
        square_x: ping_pong(
            elapsed,
            width.saturating_sub(square_size),
            width as f64 / 2.0,
        ),
        square_y: ping_pong(
            elapsed,
            height.saturating_sub(square_size),
            height as f64 / 3.0,
        ),
        square_size,
        checker_phase: (elapsed * 4.0) as usize,
    }
}

fn ping_pong(elapsed: f64, span: usize, speed: f64) -> usize {
    if span == 0 {
        return 0;
    }
    let span = span as f64;
    let position = (elapsed * speed) % (span * 2.0);
    if position <= span {
        position as usize
    } else {
        (span * 2.0 - position) as usize
    }
}

fn fill_rgba(frame: &mut [u8], width: usize, height: usize, state: PatternState) {
    debug_assert_eq!(frame.len(), width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let (r, g, b) = pattern_rgb(x, y, width, height, state);
            let offset = (y * width + x) * 4;
            frame[offset..offset + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
}

fn fill_nv12(frame: &mut [u8], width: usize, height: usize, state: PatternState) {
    let y_plane_size = width * height;
    debug_assert_eq!(frame.len(), y_plane_size * 3 / 2);
    for y in 0..height {
        for x in 0..width {
            let (r, g, b) = pattern_rgb(x, y, width, height, state);
            frame[y * width + x] = rgb_to_limited_y(r, g, b);
        }
    }

    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let (r, g, b) = pattern_rgb(x, y, width, height, state);
            let (u, v) = rgb_to_limited_uv(r, g, b);
            let offset = y_plane_size + (y / 2) * width + x;
            frame[offset] = u;
            frame[offset + 1] = v;
        }
    }
}

fn pattern_rgb(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    state: PatternState,
) -> (u8, u8, u8) {
    const BARS: [(u8, u8, u8); 8] = [
        (191, 191, 191),
        (191, 191, 0),
        (0, 191, 191),
        (0, 191, 0),
        (191, 0, 191),
        (191, 0, 0),
        (0, 0, 191),
        (24, 24, 24),
    ];

    let stripe_distance = (x + width - state.stripe_x) % width;
    if stripe_distance < state.stripe_width {
        return (255, 80, 80);
    }
    if y.abs_diff(state.scan_y) <= 1 {
        return (255, 255, 255);
    }
    if x >= state.square_x
        && x < state.square_x + state.square_size
        && y >= state.square_y
        && y < state.square_y + state.square_size
    {
        let local_x = x - state.square_x;
        let local_y = y - state.square_y;
        let border = (state.square_size / 12).max(1);
        if local_x < border
            || local_y < border
            || local_x + border >= state.square_size
            || local_y + border >= state.square_size
        {
            return (255, 255, 255);
        }
        return (8, 8, 8);
    }
    if y >= height * 3 / 4 {
        let cell = (width.min(height) / 20).max(1);
        return if (x / cell + y / cell + state.checker_phase).is_multiple_of(2) {
            (48, 48, 48)
        } else {
            (208, 208, 208)
        };
    }
    BARS[(x * BARS.len() / width).min(BARS.len() - 1)]
}

fn rgb_to_limited_y(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8
}

fn rgb_to_limited_uv(r: u8, g: u8, b: u8) -> (u8, u8) {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    let u = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
    let v = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
    (u, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_exercise_both_cameras_for_thirty_seconds() {
        assert_eq!(
            parse_args([]).expect("default arguments"),
            Settings::default()
        );
        assert_eq!(Settings::default().selection, CameraSelection::Both);
        assert_eq!(Settings::default().duration_seconds, 30.0);
    }

    #[test]
    fn parses_all_adjustable_options() {
        let settings = parse_args(
            [
                "--camera",
                "unity",
                "--duration",
                "2.5",
                "--width",
                "641",
                "--height",
                "479",
                "--fps",
                "24",
            ]
            .map(str::to_owned),
        )
        .expect("valid options");
        assert_eq!(settings.selection, CameraSelection::Unity);
        assert_eq!(settings.duration_seconds, 2.5);
        assert_eq!((settings.width, settings.height), (641, 479));
        assert_eq!(settings.fps, 24.0);
    }

    #[test]
    fn obs_selection_rejects_odd_nv12_dimensions() {
        let error =
            parse_args(["--camera", "obs", "--width", "641", "--height", "480"].map(str::to_owned))
                .expect_err("odd OBS width must fail");
        assert!(error.contains("even"));
    }

    #[test]
    fn generated_rgba_and_nv12_frames_have_exact_sizes_and_motion() {
        let (width, height) = (64, 48);
        let mut rgba = vec![
            0;
            PixelFormat::Rgba8
                .frame_size(width, height)
                .expect("RGBA size")
        ];
        fill_pattern(PixelFormat::Rgba8, &mut rgba, width, height, 0, 30.0);
        let first_rgba = rgba.clone();
        fill_pattern(PixelFormat::Rgba8, &mut rgba, width, height, 30, 30.0);
        assert_ne!(rgba, first_rgba);
        assert!(rgba.chunks_exact(4).all(|pixel| pixel[3] == 255));

        let mut nv12 = vec![
            0;
            PixelFormat::Nv12
                .frame_size(width, height)
                .expect("NV12 size")
        ];
        fill_pattern(PixelFormat::Nv12, &mut nv12, width, height, 0, 30.0);
        let first_nv12 = nv12.clone();
        fill_pattern(PixelFormat::Nv12, &mut nv12, width, height, 30, 30.0);
        assert_ne!(nv12, first_nv12);
        assert_eq!(nv12.len(), width as usize * height as usize * 3 / 2);
    }
}
