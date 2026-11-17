//! The audio one process renders, captured before it reaches a device.
//!
//! Windows can hand out the stream of a single process (and its children)
//! through the process-loopback virtual device. That stream is taken after
//! the process's own session volume, so muting the process in the mixer
//! silences the capture too; the process has to render to a device nobody
//! listens to instead.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{HRESULT, IUnknown, Interface, Ref, w};
use windows_core::implement;

/// The rate and layout every capture is converted to.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const VT_BLOB: u16 = 65;
const BUFFER_FLAG_SILENT: u32 = 2;
/// 200 ms, in the 100 ns units WASAPI counts in.
const BUFFER_DURATION: i64 = 2_000_000;

/// A running capture. Dropping it stops the capture thread.
pub struct ProcessCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ProcessCapture {
    /// Start capturing `pid` and its child processes. `on_samples` receives
    /// interleaved stereo `f32` at [`SAMPLE_RATE`] on the capture thread.
    pub fn start(
        pid: u32,
        on_samples: impl FnMut(&[f32]) + Send + 'static,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("process-capture".into())
            .spawn({
                let stop = stop.clone();
                move || run(pid, stop, ready_tx, on_samples)
            })
            .map_err(|error| error.to_string())?;
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("process capture did not start in time".into())
            }
        }
    }

    /// Whether the capture thread has ended on its own.
    pub fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for ProcessCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _ = self.0.try_send(());
        Ok(())
    }
}

/// A `PROPVARIANT` holding a `VT_BLOB`, laid out as the real one is.
#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *mut u8,
}

struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Opened {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
}

fn run(
    pid: u32,
    stop: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<(), String>>,
    mut on_samples: impl FnMut(&[f32]),
) {
    // SAFETY: balanced by CoUninitialize at the end of this thread.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let opened = open(pid).map_err(|error| format!("process capture: {error}"));
    let opened = match opened {
        Ok(opened) => {
            let _ = ready.send(Ok(()));
            opened
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            if com.is_ok() {
                // SAFETY: matches the successful CoInitializeEx above.
                unsafe { CoUninitialize() };
            }
            return;
        }
    };
    if let Err(error) = pump(&opened, &stop, &mut on_samples) {
        tracing::warn!(%error, pid, "process capture stopped");
    }
    // SAFETY: the client was started by `open`.
    unsafe {
        let _ = opened.client.Stop();
    }
    drop(opened);
    if com.is_ok() {
        // SAFETY: matches the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

fn open(pid: u32) -> windows::core::Result<Opened> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let variant = BlobVariant {
        vt: VT_BLOB,
        reserved: [0; 3],
        size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        data: (&raw mut params).cast(),
    };
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(done_tx).into();

    // SAFETY: `variant` and `params` outlive the activation, which is
    // awaited below before either goes out of scope.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            w!("VAD\\Process_Loopback"),
            &IAudioClient::IID,
            Some((&raw const variant).cast::<PROPVARIANT>()),
            &handler,
        )?
    };
    if done_rx.recv_timeout(Duration::from_secs(5)).is_err() {
        return Err(windows::core::Error::new(
            HRESULT(0x8000_000Eu32 as i32),
            "audio activation timed out",
        ));
    }
    let mut result = HRESULT(0);
    let mut activated: Option<IUnknown> = None;
    // SAFETY: both out-pointers are valid locals.
    unsafe { operation.GetActivateResult(&mut result, &mut activated)? };
    result.ok()?;
    let client: IAudioClient = activated
        .ok_or_else(|| windows::core::Error::new(HRESULT(0x8000_4005u32 as i32), "no client"))?
        .cast()?;

    let block_align = (CHANNELS * std::mem::size_of::<f32>()) as u16;
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    // SAFETY: plain WASAPI calls on a freshly activated client.
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            BUFFER_DURATION,
            0,
            &format,
            None,
        )?;
        let event = Event(CreateEventW(None, false, false, None)?);
        client.SetEventHandle(event.0)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok(Opened {
            client,
            capture,
            event,
        })
    }
}

fn pump(
    opened: &Opened,
    stop: &AtomicBool,
    on_samples: &mut impl FnMut(&[f32]),
) -> windows::core::Result<()> {
    let mut silence = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        // SAFETY: the event handle stays open for the life of `opened`.
        let woke = unsafe { WaitForSingleObject(opened.event.0, 100) };
        if woke != WAIT_OBJECT_0 {
            continue;
        }
        loop {
            // SAFETY: GetBuffer/ReleaseBuffer are paired within this block,
            // and the slice is dropped before ReleaseBuffer.
            unsafe {
                if opened.capture.GetNextPacketSize()? == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                opened
                    .capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let count = frames as usize * CHANNELS;
                if flags & BUFFER_FLAG_SILENT != 0 || data.is_null() {
                    silence.clear();
                    silence.resize(count, 0.0);
                    on_samples(&silence);
                } else {
                    let samples = std::slice::from_raw_parts(data.cast::<f32>(), count);
                    on_samples(samples);
                }
                opened.capture.ReleaseBuffer(frames)?;
            }
        }
    }
    Ok(())
}
