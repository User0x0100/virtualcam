//! Real Frame Server benchmark. Run elevated to read the service's process CPU time.
//! `mf_delivery_bench <nv12|yuy2|idle> [seconds=15] [width=1080] [height=1920] [fps=30]`
//! Latency is the producer's pre-write pixel timestamp to readable client sample,
//! including host copy, Frame Server and Source Reader; warm-up is excluded.
//! The producer writes exactly the mode the client selects — the source never
//! converts.

#[cfg(windows)]
mod windows_bench {
    use std::error::Error;
    use std::os::windows::process::CommandExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use virtualcam::{AppId, FrameRate, ManagedCameraId, ManagedCameraManager, MediaFoundationRegistration, PixelFormat, VideoFormat, WriteStatus};
    use windows::Win32::Media::MediaFoundation::{
        IMFActivate, IMFAttributes, IMFMediaSource, IMFSample, IMFSourceReader, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_MT_SUBTYPE,
        MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MFCreateAttributes, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFSTARTUP_FULL, MFShutdown, MFStartup,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize};
    use windows_core::{GUID, PWSTR};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    type Result<T> = std::result::Result<T, Box<dyn Error>>;

    struct Mf;
    impl Drop for Mf {
        fn drop(&mut self) {
            // SAFETY: Balances the successful initialization in `run`.
            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
        }
    }

    struct Camera(ManagedCameraManager, ManagedCameraId);
    impl Drop for Camera {
        fn drop(&mut self) {
            let _ = self.0.unregister(self.1);
        }
    }

    struct Producer {
        stop: Arc<AtomicBool>,
        written: Arc<AtomicU64>,
        thread: Option<JoinHandle<std::result::Result<(), virtualcam::Error>>>,
    }
    impl Drop for Producer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    struct Process(HANDLE);
    impl Process {
        fn open(pid: u32) -> Result<Self> {
            // SAFETY: Read-only process query; the resulting handle is owned.
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self(handle))
        }
        fn cpu(&self) -> Result<u64> {
            let mut times = [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 4];
            let [creation, exit, kernel, user] = &mut times;
            // SAFETY: Live query handle and four writable FILETIMEs.
            if unsafe { GetProcessTimes(self.0, creation, exit, kernel, user) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let value = |time: FILETIME| u64::from(time.dwLowDateTime) | u64::from(time.dwHighDateTime) << 32;
            Ok(value(times[2]) + value(times[3]))
        }
    }
    impl Drop for Process {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn stamp(frame: &mut [u8], width: usize, step: usize, value: u32) {
        for bit in 0..32 {
            let luma = if value >> bit & 1 == 1 { 235 } else { 16 };
            for row in 0..16 {
                for x in bit * 16..bit * 16 + 16 {
                    frame[(row * width + x) * step] = luma;
                }
            }
        }
    }

    fn decode_stamp(bytes: &[u8], width: usize, step: usize) -> Option<u32> {
        if bytes.len() < width * 16 * step {
            return None;
        }
        Some((0..32).fold(0_u32, |value, bit| value | (u32::from(bytes[(8 * width + bit * 16 + 8) * step] > 128) << bit)))
    }

    #[test]
    fn pixel_timestamp_round_trips_in_every_produced_layout() {
        for (format, step) in [(PixelFormat::Nv12, 1_usize), (PixelFormat::Yuy2, 2)] {
            let layout = format.layout(640, 16).expect("layout");
            for value in [0, 0xA5A5_1234, u32::MAX] {
                let mut frame = vec![128; layout.size()];
                stamp(&mut frame, 640, step, value);
                assert_eq!(decode_stamp(&frame, 640, step), Some(value), "{format:?}");
            }
        }
        assert_eq!(decode_stamp(&[], 640, 1), None);
    }

    fn read_stamp(reader: &IMFSourceReader, width: usize, step: usize) -> Result<u32> {
        let sample = loop {
            let mut sample: Option<IMFSample> = None;
            let mut flags = 0;
            // SAFETY: Synchronous reader and valid output storage.
            unsafe { reader.ReadSample(0xFFFF_FFFC, 0, None, Some(&mut flags), None, Some(&mut sample)) }?;
            if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 | MF_SOURCE_READERF_ERROR.0) as u32 != 0 {
                return Err(format!("Source Reader ended or failed: flags={flags:#x}").into());
            }
            if let Some(sample) = sample {
                break sample;
            }
        };
        // SAFETY: The sample owns its contiguous buffer and the lock is balanced.
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let (mut data, mut length) = (core::ptr::null_mut(), 0);
            buffer.Lock(&mut data, None, Some(&mut length))?;
            let value = if !data.is_null() { decode_stamp(core::slice::from_raw_parts(data, length as usize), width, step) } else { None };
            buffer.Unlock()?;
            value.ok_or_else(|| "sample buffer is too short".into())
        }
    }

    fn device(name: &str) -> Result<IMFActivate> {
        let mut attributes: Option<IMFAttributes> = None;
        let (mut devices, mut count) = (core::ptr::null_mut::<Option<IMFActivate>>(), 0);
        // SAFETY: Standard device enumeration; every interface and allocation is released.
        unsafe {
            MFCreateAttributes(&mut attributes, 1)?;
            let attributes = attributes.ok_or("attributes missing")?;
            attributes.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)?;
            MFEnumDeviceSources(&attributes, &mut devices, &mut count)?;
            let mut found = None;
            for index in 0..count as usize {
                if let Some(device) = (*devices.add(index)).take() {
                    let (mut text, mut length) = (PWSTR::null(), 0);
                    if device.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut text, &mut length).is_ok() {
                        let actual = text.to_string();
                        CoTaskMemFree(Some(text.0.cast()));
                        if actual.is_ok_and(|actual| actual.strip_prefix(name).is_some_and(|rest| rest.is_empty() || rest.starts_with(" ("))) {
                            found = Some(device);
                        }
                    }
                }
            }
            CoTaskMemFree(Some(devices.cast()));
            found.ok_or_else(|| "benchmark camera was not enumerated".into())
        }
    }

    pub fn run() -> Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mode = args.first().map_or("nv12", String::as_str);
        if !["nv12", "yuy2", "idle"].contains(&mode) {
            return Err("mode must be nv12, yuy2 or idle".into());
        }
        let number = |index: usize, default: u32| -> Result<u32> { Ok(args.get(index).map(|value| value.parse()).transpose()?.unwrap_or(default)) };
        let (seconds, width, height, fps) = (number(1, 15)?, number(2, 1080)?, number(3, 1920)?, number(4, 30)?);
        if seconds == 0 || width < 512 || height < 16 {
            return Err("seconds must be positive; stamp needs width >= 512, height >= 16".into());
        }
        // SAFETY: Fresh benchmark thread enters the MTA and initializes MF.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        }
        let _mf = Mf;
        let native = VideoFormat::new(PixelFormat::Nv12, width, height, FrameRate::fps(fps));
        native.validate().map_err(virtualcam::Error::from)?;
        // The producer writes exactly the mode the client selects.
        let selected = if mode == "yuy2" { PixelFormat::Yuy2 } else { PixelFormat::Nv12 };
        let producing = VideoFormat { pixel_format: selected, ..native };
        let manager = ManagedCameraManager::for_app(AppId::new("dev.virtualcam.mf-bench")?);
        let name = format!("VCRs MF Bench {}", std::process::id());
        let info = manager.register(MediaFoundationRegistration::new(&name, native, [PixelFormat::Yuy2, PixelFormat::Nv12]))?;
        let _camera = Camera(manager.clone(), info.id);
        let base = Instant::now();
        let mut producer = Producer {
            stop: Arc::new(AtomicBool::new(false)),
            written: Arc::new(AtomicU64::new(0)),
            thread: None,
        };
        if mode != "idle" {
            let mut camera = manager.open(info.id, producing)?;
            let (stop, written) = (Arc::clone(&producer.stop), Arc::clone(&producer.written));
            producer.thread = Some(std::thread::spawn(move || {
                let mut frame = vec![128; camera.frame_size()];
                let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
                let mut next = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    stamp(&mut frame, width as usize, if producing.pixel_format == PixelFormat::Yuy2 { 2 } else { 1 }, base.elapsed().as_micros() as u32);
                    if camera.write_host(&frame)? == WriteStatus::Written {
                        written.fetch_add(1, Ordering::Relaxed);
                    }
                    next += interval;
                    let now = Instant::now();
                    if next < now {
                        next = now + interval;
                    }
                    std::thread::sleep(next.saturating_duration_since(now));
                }
                camera.close()
            }));
        }
        let device = device(&name)?;
        // SAFETY: Activates the system-enumerated device, never the linked in-process source.
        let source: IMFMediaSource = unsafe { device.ActivateObject() }?;
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, None) }?;
        let subtype = GUID::from_u128(selected.mf_subtype().ok_or("subtype missing")?);
        let media_type = (0..)
            .map_while(|index| unsafe { reader.GetNativeMediaType(0, index) }.ok())
            .find(|media_type| unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.is_ok_and(|value| value == subtype))
            .ok_or("selected mode missing")?;
        unsafe { reader.SetCurrentMediaType(0, None, &media_type) }?;
        let step = if selected == PixelFormat::Yuy2 { 2 } else { 1 };
        let _ = read_stamp(&reader, width as usize, step)?;
        // SCM lookup occurs outside the measured window. No per-frame process queries.
        let lookup = std::process::Command::new("pwsh")
            .args(["-NoProfile", "-Command", "(Get-CimInstance Win32_Service -Filter \"Name='FrameServer'\").ProcessId"])
            .creation_flags(0x0800_0000)
            .output()?;
        if !lookup.status.success() {
            return Err("FrameServer PID lookup failed".into());
        }
        let server_pid: u32 = String::from_utf8(lookup.stdout)?.trim().parse()?;
        let server = Process::open(server_pid)?;
        let client = Process::open(unsafe { GetCurrentProcessId() })?;
        let warmup = Instant::now();
        while warmup.elapsed() < Duration::from_secs(3) {
            let _ = read_stamp(&reader, width as usize, step)?;
        }
        if mode != "idle" && producer.written.load(Ordering::Relaxed) == 0 {
            return Err("producer never attached to Frame Server".into());
        }
        let (server_before, client_before) = (server.cpu()?, client.cpu()?);
        let written_before = producer.written.load(Ordering::Relaxed);
        let started = Instant::now();
        let mut latencies = Vec::with_capacity(seconds as usize * fps as usize + 60);
        let (mut frames, mut repeats, mut previous) = (0, 0, None);
        while started.elapsed() < Duration::from_secs(u64::from(seconds)) {
            let stamp = read_stamp(&reader, width as usize, step)?;
            let now = base.elapsed().as_micros() as u32;
            if mode != "idle" && stamp != 0 {
                if previous == Some(stamp) {
                    repeats += 1;
                }
                previous = Some(stamp);
                latencies.push(f64::from(now.wrapping_sub(stamp)) / 1000.0);
            }
            frames += 1;
        }
        let wall = started.elapsed().as_secs_f64();
        let (server_cpu, client_cpu) = (server.cpu()? - server_before, client.cpu()? - client_before);
        let written = producer.written.load(Ordering::Relaxed) - written_before;
        producer.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = producer.thread.take() {
            thread.join().map_err(|_| "producer panicked")??;
        }
        unsafe { source.Shutdown() }?;
        assert!(frames > 0, "no frames during measurement");
        if mode != "idle" {
            assert!(!latencies.is_empty() && written > 0, "no producer frame timestamps");
        }
        latencies.sort_by(f64::total_cmp);
        let percentile = |q: f64| {
            if latencies.is_empty() {
                "null".to_owned()
            } else {
                format!("{:.3}", latencies[((latencies.len() - 1) as f64 * q) as usize])
            }
        };
        let core_percent = |cpu: u64| cpu as f64 / 10_000_000.0 / wall * 100.0;
        println!(
            "{{\"mode\":\"{mode}\",\"width\":{width},\"height\":{height},\"fps\":{fps},\"warmup_seconds\":3,\"wall_seconds\":{wall:.6},\"frames\":{frames},\"written\":{written},\"repeats\":{repeats},\"logical_processors\":{},\"frameserver_pid\":{server_pid},\"client_pid\":{},\"frameserver_cpu_core_percent\":{:.6},\"client_cpu_core_percent\":{:.6},\"delivery_fps\":{:.3},\"latency_ms_p50\":{},\"latency_ms_p90\":{},\"latency_ms_p99\":{},\"latency_ms_max\":{}}}",
            std::thread::available_parallelism()?.get(),
            std::process::id(),
            core_percent(server_cpu),
            core_percent(client_cpu),
            frames as f64 / wall,
            percentile(0.5),
            percentile(0.9),
            percentile(0.99),
            percentile(1.0)
        );
        Ok(())
    }
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows_bench::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This benchmark requires Windows Frame Server.");
}
