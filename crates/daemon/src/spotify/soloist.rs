//! Windows Spotify playback through Spotify Soloist, heard through Kopuz.
//!
//! Soloist is Spotify's own headless Connect device. Kopuz starts it with no
//! window, ties its lifetime to this process, and drives it over the Web API
//! like any other Connect device. Its audio is captured per process and
//! played again through Kopuz's equalizer and volume, so the equalizer
//! touches only the player.
//!
//! Windows applies a process's mixer mute before the capture, so Soloist has
//! to render to an output nobody listens to (set once in Windows' volume
//! mixer); the captured stream then plays on the default output.

use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::time::{Duration, Instant};

use api::{ApiError, ErrorCode};
use player::engine::{AudioSink, CpalSink, SinkConfig, SinkEvent, convert_channels, resample};
use player::loopback::{self, ProcessCapture};
use tokio::sync::{Mutex as AsyncMutex, watch};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::core::PCWSTR;

use crate::session::SessionHandle;

/// The Connect device name Soloist announces, and how Kopuz finds it again.
pub(super) const DEVICE_NAME: &str = "Kopuz";

/// How long a fresh Soloist gets to show up in the account's device list.
const APPEAR_TIMEOUT: Duration = Duration::from_secs(25);

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Soloist's exit code for a build past its 90-day lifetime.
const EXIT_EXPIRED: i32 = 10;

pub(super) struct Soloist {
    running: AsyncMutex<Option<Running>>,
    volume: Arc<AtomicU32>,
}

struct Running {
    child: Child,
    // Declared after `child` so the process is killed before the job closes.
    _job: Job,
    capture: ProcessCapture,
    device: Option<String>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Soloist {
    pub(super) fn new(volume: f32) -> Self {
        Self {
            running: AsyncMutex::new(None),
            volume: Arc::new(AtomicU32::new(volume.clamp(0.0, 1.0).to_bits())),
        }
    }

    pub(super) fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// The Connect device id of a running, paired Soloist, starting it first
    /// when it is not running.
    pub(super) async fn ready(
        &self,
        app: &SessionHandle,
        access: &str,
    ) -> Result<String, ApiError> {
        let mut running = self.running.lock().await;
        if let Some(current) = running.as_mut() {
            match current.child.try_wait() {
                Ok(None) if !current.capture.finished() => {
                    if let Some(device) = current.device.clone() {
                        return Ok(device);
                    }
                }
                Ok(Some(status)) if status.code() == Some(EXIT_EXPIRED) => {
                    *running = None;
                    return Err(ApiError::internal(
                        "This Spotify Soloist build has expired. Download the current one \
                         and replace soloist.exe.",
                    ));
                }
                _ => *running = None,
            }
        }
        if running.is_none() {
            *running = Some(self.start(app)?);
        }
        let current = running.as_mut().expect("just started");
        let device = find_device(access, &mut current.child).await?;
        current.device = Some(device.clone());
        Ok(device)
    }

    fn start(&self, app: &SessionHandle) -> Result<Running, ApiError> {
        let home = home()?;
        let exe = std::env::var_os("KOPUZ_SOLOIST_EXE")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("soloist.exe"));
        if !exe.is_file() {
            return Err(ApiError::internal(format!(
                "Spotify Soloist is not installed. Put soloist.exe in {}",
                home.display()
            )));
        }
        let key = api_key(&home)?;
        let data = home.join("data");
        let cache = home.join("cache");
        for dir in [&data, &cache] {
            std::fs::create_dir_all(dir).map_err(|error| {
                ApiError::internal(format!("Could not create {}: {error}", dir.display()))
            })?;
        }
        stop_stale(&data);
        let log = File::create(home.join("soloist.log")).map_err(|error| {
            ApiError::internal(format!("Could not open the Soloist log: {error}"))
        })?;
        let log_err = log
            .try_clone()
            .map_err(|error| ApiError::internal(error.to_string()))?;

        let job = Job::kill_on_close().map_err(|error| {
            ApiError::internal(format!("Could not create a job object: {error}"))
        })?;
        let child = Command::new(&exe)
            .arg("--device-name")
            .arg(DEVICE_NAME)
            .arg("--api-key")
            .arg(key)
            .arg("--data-dir")
            .arg(&data)
            .arg("--cache-dir")
            .arg(&cache)
            .arg("--initial-volume")
            .arg("100")
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|error| ApiError::internal(format!("Could not start Soloist: {error}")))?;
        if let Err(error) = job.assign(&child) {
            tracing::warn!(%error, "Soloist is not tied to Kopuz's lifetime");
        }
        let pid = child.id();
        tracing::info!(pid, exe = %exe.display(), "started Spotify Soloist");

        let settings = app.settings_watch();
        let volume = self.volume.clone();
        let mut output: Option<EqOutput> = None;
        let mut settings = Some(settings);
        let silent_output = silent_output(&home);
        let mut routed = false;
        let mut next_route = Instant::now();
        let capture = ProcessCapture::start(pid, move |samples| {
            // Windows takes Soloist's output preference only once it has
            // opened a stream, so it is set when its first audio arrives.
            if !routed
                && Instant::now() >= next_route
                && samples.iter().any(|sample| *sample != 0.0)
            {
                match loopback::route_output(pid, &silent_output) {
                    Ok(device) => {
                        tracing::info!(%device, "Soloist plays to an unheard output");
                        routed = true;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not move Soloist off the speakers");
                        next_route = Instant::now() + Duration::from_secs(10);
                    }
                }
            }
            let output = output.get_or_insert_with(|| {
                EqOutput::new(
                    settings.take().expect("output is built once"),
                    volume.clone(),
                )
            });
            output.write(samples);
        });
        let capture = match capture {
            Ok(capture) => capture,
            Err(error) => {
                let mut child = child;
                let _ = child.kill();
                return Err(ApiError::internal(format!(
                    "Could not capture Soloist's audio: {error}"
                )));
            }
        };
        Ok(Running {
            child,
            _job: job,
            capture,
            device: None,
        })
    }
}

/// Soloist allows one process per data directory. A Soloist left over from a
/// crash or started by hand would hold it, and the fresh one would exit while
/// Kopuz drove the old one without hearing it, so the old one is stopped.
fn stop_stale(data: &std::path::Path) {
    let Ok(pid) = std::fs::read_to_string(data.join("soloist.pid")) else {
        return;
    };
    let Ok(pid) = pid.trim().parse::<u32>() else {
        return;
    };
    let listed = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let is_soloist = listed.is_ok_and(|output| {
        String::from_utf8_lossy(&output.stdout)
            .to_ascii_lowercase()
            .contains("\"soloist.exe\"")
    });
    if !is_soloist {
        return;
    }
    tracing::info!(pid, "stopping a Soloist left running on Kopuz's data folder");
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    // Give the process a moment to release its lock file.
    std::thread::sleep(Duration::from_millis(500));
}

/// `%LOCALAPPDATA%\kopuz\kopuz\data\soloist`, where soloist.exe, its key and
/// its state live.
fn home() -> Result<PathBuf, ApiError> {
    let dirs = directories::ProjectDirs::from("moe", "kopuz", "kopuz")
        .ok_or_else(|| ApiError::internal("Could not find the Kopuz data folder"))?;
    let home = dirs.data_local_dir().join("soloist");
    std::fs::create_dir_all(&home).map_err(|error| {
        ApiError::internal(format!("Could not create {}: {error}", home.display()))
    })?;
    Ok(home)
}

/// The output Soloist renders to so that only Kopuz's copy is heard: the
/// name in `output-device.txt`, else "Digital Output".
fn silent_output(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join("output-device.txt"))
        .ok()
        .map(|name| name.trim().trim_start_matches('\u{feff}').trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Digital Output".to_string())
}

/// The Soloist API key: `SOLOIST_API_KEY`, or `api-key.txt` next to the exe.
fn api_key(home: &std::path::Path) -> Result<String, ApiError> {
    if let Ok(key) = std::env::var("SOLOIST_API_KEY")
        && !key.trim().is_empty()
    {
        return Ok(key.trim().to_string());
    }
    let path = home.join("api-key.txt");
    let key = std::fs::read_to_string(&path).unwrap_or_default();
    let key = key.trim().trim_start_matches('\u{feff}').trim();
    if key.is_empty() {
        return Err(ApiError::new(
            ErrorCode::SourceAuthExpired,
            format!(
                "Spotify Soloist needs an API key. Save it in {}",
                path.display()
            ),
        ));
    }
    Ok(key.to_string())
}

/// Wait for Soloist to appear in the account's devices. An unpaired Soloist
/// never does until it is picked once in a Spotify app on the same network.
async fn find_device(access: &str, child: &mut Child) -> Result<String, ApiError> {
    let deadline = Instant::now() + APPEAR_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(ApiError::internal(if status.code() == Some(EXIT_EXPIRED) {
                "This Spotify Soloist build has expired. Download the current one and \
                 replace soloist.exe."
                    .to_string()
            } else {
                format!("Spotify Soloist exited ({status}); see soloist.log")
            }));
        }
        match server::spotify::api::devices(access).await {
            Ok(devices) => {
                if let Some(device) = devices
                    .into_iter()
                    .find(|device| device.name == DEVICE_NAME)
                {
                    // Spotify may still list a device that another process
                    // owns; only one this Kopuz started and still runs counts.
                    if child.try_wait().is_ok_and(|status| status.is_none()) {
                        return Ok(device.id);
                    }
                }
            }
            Err(error) => tracing::debug!(%error, "spotify devices poll failed"),
        }
        if Instant::now() >= deadline {
            return Err(ApiError::new(
                ErrorCode::SourceAuthExpired,
                format!(
                    "Soloist is running but not signed in. Pick \"{DEVICE_NAME}\" once in the \
                     Spotify app on a phone on the same network, then press play again."
                ),
            ));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// A job object that kills its processes when Kopuz goes away, even when
/// Kopuz itself is killed.
struct Job(HANDLE);

impl Job {
    fn kill_on_close() -> windows::core::Result<Self> {
        // SAFETY: plain Win32 calls; the handle is owned by the returned Job.
        unsafe {
            let job = Self(CreateJobObjectW(None, PCWSTR::null())?);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
            Ok(job)
        }
    }

    fn assign(&self, child: &Child) -> windows::core::Result<()> {
        // SAFETY: the child's handle is valid while `child` is alive.
        unsafe { AssignProcessToJobObject(self.0, HANDLE(child.as_raw_handle())) }
    }
}

// SAFETY: a job handle may be used and closed from any thread.
unsafe impl Send for Job {}
// SAFETY: as above; the handle is never mutated.
unsafe impl Sync for Job {}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// The captured stream, equalized and played on the default output.
struct EqOutput {
    active: Option<Active>,
    retry_at: Option<Instant>,
    equalizer: player::eq::Equalizer,
    settings: watch::Receiver<config::Settings>,
    volume: Arc<AtomicU32>,
    scratch: Vec<f32>,
    remixed: Vec<f32>,
    resampled: Vec<f32>,
}

struct Active {
    sink: CpalSink,
    config: SinkConfig,
    producer: rtrb::Producer<f32>,
    lost: Arc<AtomicBool>,
}

impl Drop for Active {
    fn drop(&mut self) {
        self.sink.close();
    }
}

impl EqOutput {
    fn new(mut settings: watch::Receiver<config::Settings>, volume: Arc<AtomicU32>) -> Self {
        let mut equalizer = player::eq::Equalizer::new(loopback::SAMPLE_RATE, loopback::CHANNELS);
        equalizer.set_settings(settings.borrow_and_update().playback.equalizer.clone());
        Self {
            active: None,
            retry_at: None,
            equalizer,
            settings,
            volume,
            scratch: Vec::new(),
            remixed: Vec::new(),
            resampled: Vec::new(),
        }
    }

    fn open() -> Result<Active, String> {
        let lost = Arc::new(AtomicBool::new(false));
        let flag = lost.clone();
        let mut sink = CpalSink::try_new(move |event| {
            if matches!(
                event,
                SinkEvent::DeviceLost | SinkEvent::StreamStalled | SinkEvent::DefaultDeviceChanged
            ) {
                flag.store(true, Ordering::Relaxed);
            }
        })
        .map_err(|error| error.to_string())?;
        let (producer_tx, producer_rx) = std::sync::mpsc::sync_channel(1);
        let config = sink.open(
            Some(loopback::SAMPLE_RATE),
            Box::new(move |config| {
                // About 120 ms of slack between the capture and output clocks.
                let capacity = config.channels * (config.sample_rate as usize / 8);
                let (producer, mut consumer) = rtrb::RingBuffer::new(capacity);
                let _ = producer_tx.send(producer);
                // The capture arrives in uneven packets; playing only once
                // about 50 ms is queued keeps a late packet from becoming a
                // gap, and an underrun waits for that cushion again.
                let cushion = config.channels * (config.sample_rate as usize / 20);
                let mut primed = false;
                Box::new(move |output| {
                    output.fill(0.0);
                    if !primed {
                        if consumer.slots() < cushion {
                            return;
                        }
                        primed = true;
                    }
                    for frame in output.chunks_exact_mut(config.channels) {
                        if consumer.slots() < frame.len() {
                            primed = false;
                            break;
                        }
                        for sample in frame {
                            *sample = consumer.pop().unwrap_or(0.0);
                        }
                    }
                })
            }),
        )?;
        let producer = producer_rx.try_recv().map_err(|error| error.to_string())?;
        sink.play()?;
        Ok(Active {
            sink,
            config,
            producer,
            lost,
        })
    }

    fn write(&mut self, samples: &[f32]) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.lost.load(Ordering::Relaxed))
        {
            self.active = None;
        }
        if self.active.is_none() {
            if self.retry_at.is_some_and(|at| Instant::now() < at) {
                return;
            }
            match Self::open() {
                Ok(active) => {
                    self.active = Some(active);
                    self.retry_at = None;
                }
                Err(error) => {
                    tracing::warn!(%error, "could not open output for Soloist audio");
                    self.retry_at = Some(Instant::now() + Duration::from_secs(2));
                    return;
                }
            }
        }
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if self.settings.has_changed().unwrap_or(false) {
            self.equalizer
                .set_settings(self.settings.borrow_and_update().playback.equalizer.clone());
        }
        let volume = f32::from_bits(self.volume.load(Ordering::Relaxed));
        self.scratch.clear();
        self.scratch.extend_from_slice(samples);
        self.equalizer.process_in_place(&mut self.scratch);
        for sample in &mut self.scratch {
            *sample *= volume;
        }
        let samples = if active.config.channels == loopback::CHANNELS {
            &self.scratch
        } else {
            convert_channels(
                &self.scratch,
                loopback::CHANNELS,
                active.config.channels,
                &mut self.remixed,
            );
            &self.remixed
        };
        let samples = if active.config.sample_rate == loopback::SAMPLE_RATE {
            samples
        } else {
            resample(
                samples,
                active.config.channels,
                loopback::SAMPLE_RATE,
                active.config.sample_rate,
                &mut self.resampled,
            );
            &self.resampled
        };
        // The capture runs on its own clock: when the output falls behind,
        // the newest audio is dropped rather than letting latency grow.
        for frame in samples.chunks_exact(active.config.channels) {
            if active.producer.slots() < frame.len() {
                break;
            }
            for &sample in frame {
                let _ = active.producer.push(sample);
            }
        }
    }
}
