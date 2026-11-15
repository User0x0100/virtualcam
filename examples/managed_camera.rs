//! Registers, lists, streams to, and removes managed virtual cameras.
//!
//! ```text
//! cargo run --example managed_camera -- register "Studio Cam" [--app com.example.studio] [--backend dshow|mf]
//!                                         [--scope user|machine] [--format nv12] [--formats nv12,yuy2]
//!                                         [--size 1280x720] [--fps 30]
//! cargo run --example managed_camera -- list [--app com.example.studio]
//! cargo run --example managed_camera -- stream <ID> [--format yuy2] [--size 1280x720] [--fps 30] [--seconds 30]
//! cargo run --example managed_camera -- unregister <ID> [--app com.example.studio]
//! cargo run --example managed_camera -- install-mf <PATH>       # elevated, once per machine
//! cargo run --example managed_camera -- uninstall-mf [--other-users-cleaned]  # elevated
//! ```
//!
//! `--formats` is the Media Foundation declared format set (default: only
//! `--format`); `--format` is its default format and the DirectShow idle
//! format. When a consumer selects another declared format, `stream` follows
//! it by reopening the producer in that format.
//!
//! DirectShow cameras need `virtualcam_dshow.dll`; pass `--dshow-source <path>`
//! or place it next to the example executable. Media Foundation cameras need
//! the MF source installed once by an administrator.

use std::error::Error;
use std::time::{Duration, Instant};

use virtualcam::{AppId, DirectShowRegistration, FrameRate, ManagedBackend, ManagedCameraId, ManagedCameraManager, MediaFoundationRegistration, PixelFormat, RegistrationScope, VideoFormat, VirtualCamera, WriteStatus};

struct Args {
    positional: Vec<String>,
    app: Option<String>,
    backend: String,
    scope: String,
    format: String,
    formats: Option<String>,
    size: (u32, u32),
    fps: u32,
    seconds: f64,
    dshow_source: Option<String>,
    other_users_cleaned: bool,
}

fn parse_args() -> Result<Args, Box<dyn Error>> {
    let mut args = Args {
        positional: Vec::new(),
        app: None,
        backend: "dshow".into(),
        scope: "user".into(),
        format: "nv12".into(),
        formats: None,
        size: (1280, 720),
        fps: 30,
        seconds: 30.0,
        dshow_source: None,
        other_users_cleaned: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--app" => args.app = Some(value()?),
            "--backend" => args.backend = value()?,
            "--scope" => args.scope = value()?,
            "--format" => args.format = value()?,
            "--formats" => args.formats = Some(value()?),
            "--fps" => args.fps = value()?.parse()?,
            "--seconds" => args.seconds = value()?.parse()?,
            "--dshow-source" => args.dshow_source = Some(value()?),
            "--other-users-cleaned" => args.other_users_cleaned = true,
            "--size" => {
                let text = value()?;
                let (w, h) = text.split_once('x').ok_or("size must look like 1280x720")?;
                args.size = (w.parse()?, h.parse()?);
            }
            _ => args.positional.push(arg),
        }
    }
    Ok(args)
}

fn pixel_format(name: &str) -> Result<PixelFormat, Box<dyn Error>> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "nv12" => PixelFormat::Nv12,
        "i420" | "iyuv" => PixelFormat::I420,
        "yuy2" => PixelFormat::Yuy2,
        "uyvy" => PixelFormat::Uyvy,
        "p010" => PixelFormat::P010,
        "bgra" | "bgra32" | "rgb32" => PixelFormat::Bgra32,
        "rgb24" | "bgr24" => PixelFormat::Rgb24,
        other => return Err(format!("unknown pixel format {other}").into()),
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
    let mut manager = match &args.app {
        Some(app) => ManagedCameraManager::for_app(AppId::new(app.clone())?),
        None => ManagedCameraManager::new(),
    };
    if let Some(path) = &args.dshow_source {
        manager = manager.with_dshow_source(path);
    }
    let format = VideoFormat::new(pixel_format(&args.format)?, args.size.0, args.size.1, FrameRate::fps(args.fps));

    match args.positional.first().map(String::as_str) {
        Some("register") => {
            let name = args.positional.get(1).ok_or("register needs a camera name")?.clone();
            let info = match args.backend.as_str() {
                "dshow" => manager.register(DirectShowRegistration {
                    name,
                    scope: if args.scope == "machine" { RegistrationScope::AllUsers } else { RegistrationScope::CurrentUser },
                    default_format: Some(format),
                    pixel_formats: PixelFormat::MANAGED.to_vec(),
                    frame_rates: [15, 24, 25, 30, 50, 60].map(FrameRate::fps).to_vec(),
                })?,
                "mf" => {
                    let formats = match &args.formats {
                        Some(list) => list.split(',').map(|name| pixel_format(name.trim())).collect::<Result<Vec<_>, Box<dyn Error>>>()?,
                        None => vec![format.pixel_format],
                    };
                    manager.register(MediaFoundationRegistration::new(name, format, formats))?
                }
                other => return Err(format!("unknown backend {other}").into()),
            };
            println!("{}", info.id);
        }
        Some("list") => {
            for info in manager.list()? {
                let format = info.format.map(|f| f.to_string()).unwrap_or_else(|| "-".into());
                let app = info.app_id.map(|a| a.to_string()).unwrap_or_else(|| "-".into());
                println!("{}  {:?}  {:?}  {:?}  app={}  format={}  {:?}", info.id, info.name, info.backend, info.scope, app, format, info.health);
            }
        }
        Some("unregister") => {
            let id: ManagedCameraId = args.positional.get(1).ok_or("unregister needs an id")?.parse().map_err(|_| "invalid id")?;
            manager.unregister(id)?;
        }
        Some("stream") => {
            let id: ManagedCameraId = args.positional.get(1).ok_or("stream needs an id")?.parse().map_err(|_| "invalid id")?;
            let info = manager.get(id)?;
            let camera = if info.backend == ManagedBackend::MediaFoundation {
                manager.open_registered(id)?
            } else {
                manager.open(id, format)?
            };
            stream(&manager, id, camera, args.seconds)?;
        }
        Some("install-mf") => {
            let path = args.positional.get(1).ok_or("install-mf needs the virtualcam_mf.dll path")?;
            ManagedCameraManager::install_media_foundation_source(path)?;
            println!("installed {:?}", ManagedCameraManager::media_foundation_source());
        }
        Some("uninstall-mf") => ManagedCameraManager::uninstall_media_foundation_source(args.other_users_cleaned)?,
        _ => return Err("usage: managed_camera <register|list|stream|unregister|install-mf|uninstall-mf> ...".into()),
    }
    Ok(())
}

/// Streams a moving test pattern, following every mode consumers select by
/// reopening the producer in it.
fn stream(manager: &ManagedCameraManager, id: ManagedCameraId, mut camera: VirtualCamera, seconds: f64) -> Result<(), Box<dyn Error>> {
    let mut format = camera.video_format().ok_or("camera has no video format")?;
    let interval = Duration::from_nanos(format.frame_rate.interval_100ns().unwrap_or(333_333) * 100);
    let mut frame = vec![0_u8; camera.frame_size()];
    let started = Instant::now();
    let (mut written, mut inactive, mut switches) = (0_u64, 0_u64, 0_u64);
    let mut index = 0_u64;
    while started.elapsed().as_secs_f64() < seconds {
        if let Some(wanted) = camera.requested_format() {
            // The consumer selected another mode: frames in the current one
            // would be shown as black, so reopen in the requested mode.
            camera = manager.open(id, wanted)?;
            format = wanted;
            frame.resize(camera.frame_size(), 0);
            switches += 1;
        }
        draw(&mut frame, &format, index);
        match camera.write_host(&frame)? {
            WriteStatus::Written => written += 1,
            _ => inactive += 1,
        }
        index += 1;
        let next = started + interval * index as u32;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    camera.close()?;
    println!("{format}: {written} frames written, {inactive} while no consumer was attached, {switches} mode switches");
    Ok(())
}

/// Moving vertical bar over a horizontal luma ramp, in any managed format.
fn draw(frame: &mut [u8], format: &VideoFormat, index: u64) {
    let (w, h) = (format.width as usize, format.height as usize);
    let bar = (index as usize * 8) % w;
    let luma = |x: usize| if x.abs_diff(bar) < w / 40 + 1 { 235_u8 } else { 16 + (x * 200 / w) as u8 };
    match format.pixel_format {
        PixelFormat::Nv12 | PixelFormat::I420 => {
            for y in 0..h {
                for x in 0..w {
                    frame[y * w + x] = luma(x);
                }
            }
            frame[w * h..].fill(128);
        }
        PixelFormat::P010 => {
            for y in 0..h {
                for x in 0..w {
                    let value = (u16::from(luma(x)) << 2) << 6;
                    frame[(y * w + x) * 2..][..2].copy_from_slice(&value.to_le_bytes());
                }
            }
            for chunk in frame[w * h * 2..].chunks_exact_mut(2) {
                chunk.copy_from_slice(&(512_u16 << 6).to_le_bytes());
            }
        }
        PixelFormat::Yuy2 | PixelFormat::Uyvy => {
            let yuy2 = format.pixel_format == PixelFormat::Yuy2;
            for y in 0..h {
                for x in (0..w).step_by(2) {
                    let o = (y * w + x) * 2;
                    let (y0, y1) = (luma(x), luma(x + 1));
                    let bytes = if yuy2 { [y0, 128, y1, 128] } else { [128, y0, 128, y1] };
                    frame[o..o + 4].copy_from_slice(&bytes);
                }
            }
        }
        PixelFormat::Bgra32 | PixelFormat::Rgb24 => {
            let bpp = if format.pixel_format == PixelFormat::Bgra32 { 4 } else { 3 };
            for y in 0..h {
                for x in 0..w {
                    let o = (y * w + x) * bpp;
                    let v = luma(x);
                    frame[o..o + 3].copy_from_slice(&[v, (y * 255 / h) as u8, v]);
                    if bpp == 4 {
                        frame[o + 3] = 255;
                    }
                }
            }
        }
        _ => {}
    }
}
