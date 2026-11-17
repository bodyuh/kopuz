//! Windows Spotify playback in the application process.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use api::{ApiError, ErrorCode};
use librespot_core::{
    Session, SessionConfig, SpotifyUri, authentication::Credentials, cache::Cache,
};
use librespot_playback::{
    NUM_CHANNELS, SAMPLE_RATE,
    audio_backend::{Sink, SinkError, SinkResult},
    config::PlayerConfig,
    convert::Converter,
    decoder::AudioPacket,
    mixer::NoOpVolume,
    player::{Player, PlayerEvent},
};
use player::engine::{AudioSink, CpalSink, SinkConfig, SinkEvent, convert_channels, resample};
use reader::Track;
use tokio::sync::{Mutex as AsyncMutex, watch};

use crate::{external::ExternalReport, session::SessionHandle};

pub(super) struct NativePlayback {
    engine: AsyncMutex<Option<Engine>>,
    request: AtomicU64,
    cancelled: watch::Sender<()>,
    volume: Arc<AtomicU32>,
}

struct Engine {
    connection: Session,
    player: Arc<Player>,
    requested: Arc<Mutex<Option<RequestedTrack>>>,
    events: tokio::task::JoinHandle<()>,
    account: String,
}

struct RequestedTrack {
    uri: SpotifyUri,
    track: Track,
    artwork: Option<String>,
    event_id: Option<u64>,
    position_ms: u64,
    playing: bool,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.events.abort();
        self.player.stop();
        self.connection.shutdown();
    }
}

impl NativePlayback {
    pub(super) fn new(volume: f32) -> Self {
        Self {
            engine: AsyncMutex::new(None),
            request: AtomicU64::new(0),
            cancelled: watch::channel(()).0,
            volume: Arc::new(AtomicU32::new(volume.to_bits())),
        }
    }

    pub(super) async fn load(
        &self,
        session: &SessionHandle,
        track: &Track,
        artwork: Option<String>,
    ) -> Result<(), ApiError> {
        let mut cancelled = self.cancelled.subscribe();
        let request = self.request.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let sources = session.sources();
        let account = sources
            .server
            .as_ref()
            .filter(|source| source.service == config::MusicService::Spotify)
            .and_then(|source| source.user_id.clone())
            .filter(|account| !account.is_empty())
            .ok_or_else(|| {
                ApiError::new(
                    ErrorCode::SourceAuthExpired,
                    "Sign in to Spotify in Media servers first.",
                )
            })?;
        let uri = SpotifyUri::from_uri(&format!("spotify:track:{}", track.id.key()))
            .map_err(|error| ApiError::internal(error.to_string()))?;

        tokio::select! {
            biased;
            _ = cancelled.changed() => Ok(()),
            result = async {
                let mut engine = self.engine.lock().await;
                if self.request.load(Ordering::Relaxed) != request {
                    return Ok(());
                }
                if engine.as_ref().is_none_or(|engine| {
                    engine.connection.is_invalid() || engine.player.is_invalid() || engine.account != account
                }) {
                    *engine = None;
                    *engine = Some(connect(session, account, self.volume.clone()).await?);
                }
                if self.request.load(Ordering::Relaxed) != request {
                    return Ok(());
                }
                if let Some(engine) = engine.as_ref() {
                    *engine.requested.lock().unwrap_or_else(|error| error.into_inner()) = Some(RequestedTrack {
                        uri: uri.clone(),
                        track: track.clone(),
                        artwork,
                        position_ms: 0,
                        playing: false,
                        event_id: None,
                    });
                    engine.player.load(uri, true, 0);
                }
                Ok(())
            } => result,
        }
    }

    pub(super) fn play(&self) {
        self.with_player(|player| player.play());
    }

    pub(super) fn pause(&self) {
        self.with_player(|player| player.pause());
    }

    pub(super) fn seek(&self, milliseconds: u64) {
        self.with_player(|player| player.seek(milliseconds.min(u64::from(u32::MAX)) as u32));
    }

    pub(super) fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub(super) async fn stop(&self) {
        self.request.fetch_add(1, Ordering::Relaxed);
        self.cancelled.send_modify(|_| {});
        if let Some(engine) = self.engine.lock().await.as_ref() {
            engine
                .requested
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            engine.player.stop();
        }
    }

    fn with_player(&self, operation: impl FnOnce(&Player)) {
        if let Ok(engine) = self.engine.try_lock()
            && let Some(engine) = engine.as_ref()
        {
            operation(&engine.player);
        }
    }
}

async fn connect(
    app: &SessionHandle,
    account: String,
    volume: Arc<AtomicU32>,
) -> Result<Engine, ApiError> {
    let dirs = directories::ProjectDirs::from("moe", "kopuz", "kopuz")
        .ok_or_else(|| ApiError::internal("Could not find the Kopuz configuration folder"))?;
    let cache = Cache::new(
        Some(dirs.config_dir().join("spotify-native")),
        None,
        None,
        None,
    )
    .map_err(|error| {
        ApiError::internal(format!("Could not open Spotify sign-in storage: {error}"))
    })?;
    let client = SessionConfig::default();
    let mut connection = Session::new(client.clone(), Some(cache.clone()));
    let mut authorized = false;
    if let Some(credentials) = cache.credentials()
        && credentials.username.as_deref() == Some(account.as_str())
    {
        match tokio::time::timeout(
            Duration::from_secs(20),
            connection.connect(credentials, false),
        )
        .await
        {
            Ok(Ok(())) => authorized = true,
            Ok(Err(error))
                if matches!(
                    error.kind,
                    librespot_core::error::ErrorKind::PermissionDenied
                        | librespot_core::error::ErrorKind::Unauthenticated
                ) =>
            {
                connection.shutdown();
                connection = Session::new(client.clone(), Some(cache.clone()));
            }
            Ok(Err(error)) => {
                return Err(ApiError::internal(format!(
                    "Spotify connection failed: {error}"
                )));
            }
            Err(_) => return Err(ApiError::internal("Spotify connection timed out")),
        }
    }
    if !authorized {
        let token = server::spotify::auth::authorize_native_playback(client.client_id)
            .await
            .map_err(|error| ApiError::new(ErrorCode::SourceAuthExpired, error))?;
        tokio::time::timeout(
            Duration::from_secs(20),
            connection.connect(Credentials::with_access_token(token), true),
        )
        .await
        .map_err(|_| ApiError::internal("Spotify connection timed out"))?
        .map_err(|error| ApiError::internal(format!("Spotify connection failed: {error}")))?;
    }
    if connection.username() != account {
        connection.shutdown();
        return Err(ApiError::new(
            ErrorCode::SourceAuthExpired,
            "Use the same Spotify account for library access and playback.",
        ));
    }

    let settings = app.settings_watch();
    let player = Player::new(
        PlayerConfig {
            position_update_interval: Some(Duration::from_secs(1)),
            ..PlayerConfig::default()
        },
        connection.clone(),
        Box::new(NoOpVolume),
        move || {
            let mut settings = settings;
            let mut equalizer = player::eq::Equalizer::new(SAMPLE_RATE, usize::from(NUM_CHANNELS));
            equalizer.set_settings(settings.borrow_and_update().playback.equalizer.clone());
            Box::new(Output {
                active: None,
                equalizer,
                settings,
                volume,
                scratch: Vec::new(),
                remixed: Vec::new(),
                resampled: Vec::new(),
            })
        },
    );
    let mut channel = player.get_player_event_channel();
    let requested = Arc::new(Mutex::new(None));
    let app = app.clone();
    let events = tokio::spawn({
        let requested = requested.clone();
        async move {
            while let Some(event) = channel.recv().await {
                report(&app, &requested, event);
            }
        }
    });
    Ok(Engine {
        connection,
        player,
        requested,
        events,
        account,
    })
}

fn report(app: &SessionHandle, request: &Mutex<Option<RequestedTrack>>, event: PlayerEvent) {
    let (uri, id, position, playing, completed, failure) = match event {
        PlayerEvent::Playing {
            track_id,
            play_request_id,
            position_ms,
        } => (
            track_id,
            play_request_id,
            Some(position_ms),
            Some(true),
            false,
            false,
        ),
        PlayerEvent::Paused {
            track_id,
            play_request_id,
            position_ms,
        } => (
            track_id,
            play_request_id,
            Some(position_ms),
            Some(false),
            false,
            false,
        ),
        PlayerEvent::PositionCorrection {
            track_id,
            play_request_id,
            position_ms,
        }
        | PlayerEvent::PositionChanged {
            track_id,
            play_request_id,
            position_ms,
        }
        | PlayerEvent::Seeked {
            track_id,
            play_request_id,
            position_ms,
        } => (
            track_id,
            play_request_id,
            Some(position_ms),
            None,
            false,
            false,
        ),
        PlayerEvent::EndOfTrack {
            track_id,
            play_request_id,
        } => (track_id, play_request_id, None, Some(false), true, false),
        PlayerEvent::Unavailable {
            track_id,
            play_request_id,
        }
        | PlayerEvent::AudioKeyUnavailable {
            track_id,
            play_request_id,
        } => (track_id, play_request_id, None, Some(false), false, true),
        _ => return,
    };
    let mut requested = request.lock().unwrap_or_else(|error| error.into_inner());
    let Some(track) = requested
        .as_mut()
        .filter(|track| track.uri == uri && track.event_id.is_none_or(|previous| previous == id))
    else {
        return;
    };
    track.event_id = Some(id);
    if let Some(position) = position {
        track.position_ms = u64::from(position);
    }
    if let Some(playing) = playing {
        track.playing = playing;
    }
    if failure {
        app.emit_event(api::ApiEvent::Notice {
            level: api::NoticeLevel::Error,
            code: "spotify".into(),
            message: Some("Spotify couldn't play this track in Kopuz.".into()),
        });
    }
    app.report_external(ExternalReport {
        track: Some(track.track.clone()),
        position_ms: track.position_ms,
        playing: track.playing,
        completed,
        device: None,
        cover_url: track.artwork.clone(),
    });
}

struct Output {
    active: Option<ActiveOutput>,
    equalizer: player::eq::Equalizer,
    settings: watch::Receiver<config::Settings>,
    volume: Arc<AtomicU32>,
    scratch: Vec<f32>,
    remixed: Vec<f32>,
    resampled: Vec<f32>,
}

struct ActiveOutput {
    sink: CpalSink,
    config: SinkConfig,
    producer: rtrb::Producer<f32>,
    capacity: usize,
    lost: Arc<AtomicBool>,
}

impl Sink for Output {
    fn start(&mut self) -> SinkResult<()> {
        let lost = Arc::new(AtomicBool::new(false));
        let failure_flag = lost.clone();
        let mut sink = CpalSink::try_new(move |event| {
            if matches!(
                event,
                SinkEvent::DeviceLost | SinkEvent::StreamStalled | SinkEvent::DefaultDeviceChanged
            ) {
                failure_flag.store(true, Ordering::Relaxed);
            }
        })
        .map_err(|error| SinkError::ConnectionRefused(error.to_string()))?;
        // The callback factory runs synchronously while opening the stream.
        let (producer_tx, producer_rx) = std::sync::mpsc::sync_channel(1);
        let config = sink
            .open(
                Some(SAMPLE_RATE),
                Box::new(move |config| {
                    let capacity = config.channels * (config.sample_rate as usize / 12);
                    let (producer, mut consumer) = rtrb::RingBuffer::new(capacity);
                    let _ = producer_tx.send((producer, capacity));
                    Box::new(move |output| {
                        output.fill(0.0);
                        for frame in output.chunks_exact_mut(config.channels) {
                            if consumer.slots() < frame.len() {
                                break;
                            }
                            for sample in frame {
                                *sample = consumer.pop().unwrap_or(0.0);
                            }
                        }
                    })
                }),
            )
            .map_err(SinkError::ConnectionRefused)?;
        let (producer, capacity) = producer_rx
            .try_recv()
            .map_err(|error| SinkError::ConnectionRefused(error.to_string()))?;
        sink.play().map_err(SinkError::StateChange)?;
        self.active = Some(ActiveOutput {
            sink,
            config,
            producer,
            capacity,
            lost,
        });
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        if let Some(mut active) = self.active.take() {
            let deadline = Instant::now() + Duration::from_millis(120);
            while active.producer.slots() < active.capacity
                && !active.lost.load(Ordering::Relaxed)
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            active.sink.close();
        }
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, _: &mut Converter) -> SinkResult<()> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.lost.load(Ordering::Relaxed))
        {
            self.stop()?;
            self.start()?;
        }
        let active = self
            .active
            .as_mut()
            .ok_or_else(|| SinkError::NotConnected("Windows output is closed".into()))?;
        let samples = packet
            .samples()
            .map_err(|error| SinkError::OnWrite(error.to_string()))?;
        if self.settings.has_changed().unwrap_or(false) {
            self.equalizer
                .set_settings(self.settings.borrow_and_update().playback.equalizer.clone());
        }
        let source_channels = usize::from(NUM_CHANNELS);
        let volume = f32::from_bits(self.volume.load(Ordering::Relaxed));
        self.scratch.clear();
        self.scratch
            .extend(samples.iter().map(|sample| *sample as f32));
        self.equalizer.process_in_place(&mut self.scratch);
        for sample in &mut self.scratch {
            *sample *= volume;
        }
        let samples = if active.config.channels == source_channels {
            &self.scratch
        } else {
            convert_channels(
                &self.scratch,
                source_channels,
                active.config.channels,
                &mut self.remixed,
            );
            &self.remixed
        };
        let samples = if active.config.sample_rate == SAMPLE_RATE {
            samples
        } else {
            resample(
                samples,
                active.config.channels,
                SAMPLE_RATE,
                active.config.sample_rate,
                &mut self.resampled,
            );
            &self.resampled
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        for frame in samples.chunks_exact(active.config.channels) {
            while active.producer.slots() < frame.len() {
                if active.lost.load(Ordering::Relaxed) || Instant::now() >= deadline {
                    return Err(SinkError::OnWrite("Windows audio output stopped".into()));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            for &sample in frame {
                // Only this thread writes while the callback frees slots.
                let _ = active.producer.push(sample);
            }
        }
        Ok(())
    }
}
