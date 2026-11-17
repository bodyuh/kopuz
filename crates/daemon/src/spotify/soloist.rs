//! Windows Spotify playback through Spotify Soloist, heard through Kopuz.
//!
//! Soloist is Spotify's own headless Connect device. Kopuz starts it with no
//! window, ties its lifetime to this process, and drives it over Soloist's
//! local WebSocket API, which also reports play state and position as they
//! change, without the Web API's token or delay. Its audio is captured per
//! process and played again through Kopuz's equalizer and volume, so the
//! equalizer touches only the player.
//!
//! Windows applies a process's mixer mute before the capture, so Soloist is
//! moved to an output nobody listens to; the captured stream then plays on
//! the default output.

use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use api::{ApiError, ErrorCode};
use futures_util::{SinkExt, StreamExt};
use player::engine::{AudioSink, CpalSink, SinkConfig, SinkEvent, convert_channels, resample};
use player::loopback::{self, ProcessCapture};
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::core::PCWSTR;

use crate::session::SessionHandle;

/// The Connect device name Soloist announces.
pub(super) const DEVICE_NAME: &str = "Kopuz";

/// How long a fresh Soloist gets to restore its sign-in.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a fresh Soloist gets to open its WebSocket.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Soloist's exit code for a build past its 90-day lifetime.
const EXIT_EXPIRED: i32 = 10;
const EXPIRED: &str =
    "This Spotify Soloist build has expired. Download the current one and replace soloist.exe.";

/// What Soloist reports about itself over its WebSocket.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct SoloistState {
    pub logged_in: bool,
    pub uri: Option<String>,
    /// `idle`, `playing`, `paused` or `buffering`.
    pub status: String,
    pub duration_ms: u64,
    position_ms: u64,
    /// Unix milliseconds at which `position_ms` held.
    timestamp_ms: u64,
    speed: f64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

impl SoloistState {
    pub fn playing(&self) -> bool {
        matches!(self.status.as_str(), "playing" | "buffering")
    }

    /// The position now, advanced from the last anchor while playing.
    pub fn position_now(&self) -> u64 {
        if self.status != "playing" {
            return self.position_ms;
        }
        let elapsed = now_ms().saturating_sub(self.timestamp_ms) as f64 * self.speed.max(0.0);
        self.position_ms.saturating_add(elapsed as u64)
    }

    fn apply(&mut self, event: &Value) {
        match event["type"].as_str() {
            Some("auth_state") => self.logged_in = event["logged_in"].as_bool().unwrap_or(false),
            Some("playback_state") => {
                if let Some(status) = event["status"].as_str() {
                    self.status = status.to_string();
                }
                self.set_item(&event["item"]);
                self.set_position(&event["position"]);
            }
            Some("position_sync") => self.set_position(&event["position"]),
            Some("track_changed") => self.set_item(&event["item"]),
            Some("playback_changed") => {
                if let Some(status) = event["status"].as_str() {
                    // Freeze the position where it was when playback stopped.
                    self.position_ms = self.position_now();
                    self.timestamp_ms = now_ms();
                    self.status = status.to_string();
                }
            }
            _ => {}
        }
    }

    fn set_item(&mut self, item: &Value) {
        let uri = item["uri"].as_str().map(str::to_string);
        let duration = item["decorations"]["playback"]["duration_ms"].as_u64();
        if uri != self.uri {
            self.uri = uri;
            self.duration_ms = duration.unwrap_or(0);
        } else if let Some(duration) = duration {
            self.duration_ms = duration;
        }
    }

    fn set_position(&mut self, position: &Value) {
        if let Some(ms) = position["position_ms"].as_u64() {
            self.position_ms = ms;
            self.timestamp_ms = position["timestamp_ms"].as_u64().unwrap_or_else(now_ms);
            self.speed = position["speed"].as_f64().unwrap_or(0.0);
        }
    }
}

pub(super) struct Soloist {
    running: AsyncMutex<Option<Running>>,
    volume: Arc<AtomicU32>,
    state: Arc<watch::Sender<SoloistState>>,
    commands: Mutex<Option<mpsc::UnboundedSender<String>>>,
}

struct Running {
    child: Child,
    // Declared after `child` so the process is killed before the job closes.
    _job: Job,
    capture: ProcessCapture,
    control: tokio::task::JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.control.abort();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Soloist {
    pub(super) fn new(volume: f32) -> Self {
        Self {
            running: AsyncMutex::new(None),
            volume: Arc::new(AtomicU32::new(volume.clamp(0.0, 1.0).to_bits())),
            state: Arc::new(watch::Sender::new(SoloistState::default())),
            commands: Mutex::new(None),
        }
    }

    pub(super) fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<SoloistState> {
        self.state.subscribe()
    }

    /// Send one WebSocket command, such as `{"command": "pause"}`.
    pub(super) fn send(&self, mut command: Value) -> Result<(), ApiError> {
        command["type"] = Value::from("command");
        let sender = self
            .commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        sender
            .and_then(|sender| sender.send(command.to_string()).ok())
            .ok_or_else(|| ApiError::internal("Spotify Soloist is not running"))
    }

    /// Make sure Soloist runs, answers on its socket and is signed in.
    pub(super) async fn ready(&self, app: &SessionHandle) -> Result<(), ApiError> {
        {
            let mut running = self.running.lock().await;
            if let Some(current) = running.as_mut() {
                match current.child.try_wait() {
                    Ok(None) if !current.capture.finished() && !current.control.is_finished() => {}
                    Ok(Some(status)) if status.code() == Some(EXIT_EXPIRED) => {
                        *running = None;
                        return Err(ApiError::internal(EXPIRED));
                    }
                    _ => *running = None,
                }
            }
            if running.is_none() {
                *running = Some(self.start(app).await?);
            }
        }
        let mut state = self.state.subscribe();
        let signed_in = tokio::time::timeout(SIGN_IN_TIMEOUT, async move {
            state.wait_for(|it| it.logged_in).await.map(|_| ())
        });
        match signed_in.await {
            Ok(Ok(_)) => Ok(()),
            _ => Err(ApiError::new(
                ErrorCode::SourceAuthExpired,
                format!(
                    "Soloist is not signed in. Pick \"{DEVICE_NAME}\" once in a Spotify app on \
                     this network, then press play again."
                ),
            )),
        }
    }

    async fn start(&self, app: &SessionHandle) -> Result<Running, ApiError> {
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
        let _ = std::fs::remove_file(data.join("ws.port"));
        let log = File::create(home.join("soloist.log")).map_err(|error| {
            ApiError::internal(format!("Could not open the Soloist log: {error}"))
        })?;
        let log_err = log
            .try_clone()
            .map_err(|error| ApiError::internal(error.to_string()))?;

        let job = Job::kill_on_close().map_err(|error| {
            ApiError::internal(format!("Could not create a job object: {error}"))
        })?;
        let mut child = Command::new(&exe)
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
            .arg("--ws")
            .arg("127.0.0.1:0")
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

        let port = match socket_port(&data, &mut child).await {
            Ok(port) => port,
            Err(error) => {
                let _ = child.kill();
                return Err(error);
            }
        };
        let (commands, receiver) = mpsc::unbounded_channel();
        *self
            .commands
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(commands);
        self.state.send_replace(SoloistState::default());
        let control = tokio::spawn(control(port, self.state.clone(), receiver));

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
                control.abort();
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
            control,
        })
    }
}

/// The port Soloist's WebSocket picked, from the file it writes once
/// listening.
async fn socket_port(data: &Path, child: &mut Child) -> Result<u16, ApiError> {
    let deadline = Instant::now() + SOCKET_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(ApiError::internal(if status.code() == Some(EXIT_EXPIRED) {
                EXPIRED.to_string()
            } else {
                format!("Spotify Soloist exited ({status}); see soloist.log")
            }));
        }
        if let Ok(text) = std::fs::read_to_string(data.join("ws.port"))
            && let Ok(port) = text.trim().parse::<u16>()
            && port != 0
        {
            return Ok(port);
        }
        if Instant::now() >= deadline {
            return Err(ApiError::internal(
                "Spotify Soloist did not open its control socket; see soloist.log",
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Keep the WebSocket to Soloist: forward commands, fold events into state.
async fn control(
    port: u16,
    state: Arc<watch::Sender<SoloistState>>,
    mut commands: mpsc::UnboundedReceiver<String>,
) {
    let socket = match tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}")).await {
        Ok((socket, _)) => socket,
        Err(error) => {
            tracing::warn!(%error, "could not connect to Soloist's control socket");
            return;
        }
    };
    let (mut write, mut read) = socket.split();
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                if let Err(error) = write.send(Message::Text(command.into())).await {
                    tracing::warn!(%error, "Soloist control socket write failed");
                    break;
                }
            }
            message = read.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let Ok(event) = serde_json::from_str::<Value>(text.as_str()) else {
                        continue;
                    };
                    if event["type"] == "error" {
                        tracing::warn!(reason = %event["message"], "Soloist refused a command");
                    }
                    state.send_if_modified(|current| {
                        let before = current.clone();
                        current.apply(&event);
                        *current != before
                    });
                }
                Some(Ok(_)) => {}
                _ => break,
            }
        }
    }
    tracing::info!("Soloist control socket closed");
    state.send_replace(SoloistState::default());
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
