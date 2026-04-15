/// Windows WASAPI AEC (Acoustic Echo Cancellation) capture
///
/// Opens the default Communications capture endpoint, casts the client
/// to `IAudioClient2`, and marks the stream as `AudioCategory_Communications`
/// via `SetClientProperties`. That tells Windows to route the stream
/// through the built-in voice communications APO chain (AEC / AGC / NS),
/// provided the microphone driver exposes those effects.
///
/// The resulting 8 kHz mono frames go through the same
/// `UnboundedSender<Vec<i16>>` used by the cpal fallback path, so upper
/// layers see no difference.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;
use windows::{
    core::{Interface, GUID, PCWSTR},
    Win32::{
        Foundation::{CloseHandle, FALSE, WAIT_OBJECT_0},
        Media::Audio::{
            eCapture, eCommunications, eRender,
            AudioCategory_Communications,
            AudioClientProperties,
            IAudioCaptureClient, IAudioClient, IAudioClient2,
            IMMDeviceEnumerator, MMDeviceEnumerator,
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            AUDCLNT_STREAMOPTIONS_NONE, WAVEFORMATEX,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree,
            CLSCTX_ALL, COINIT_MULTITHREADED,
        },
        System::Threading::{CreateEventW, WaitForSingleObject},
    },
};
use rubato::{Fft, FixedSync, Resampler};
use audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::audioadapter::AdapterIterators;

const TARGET_RATE: u32 = 8_000;
const VOICE_FRAME: usize = 160;

// ── public result type ──────────────────────────────────────────────────────

pub struct AecCapture {
    /// Background thread handle — joined on drop
    thread: Option<std::thread::JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    /// Actual capture sample rate reported by WASAPI
    pub device_rate: u32,
    /// Friendly name shown in the UI
    pub device_name: String,
}

impl AecCapture {
    /// Start AEC capture and push 8 kHz mono frames into `sender`.
    /// Returns `Err(String)` if the Windows API calls fail.
    pub fn start(
        sender: UnboundedSender<Vec<i16>>,
        transmitting: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        // COM must be initialised on the thread that calls WASAPI.
        // We do it on the worker thread below; here we just probe the
        // default communications capture device for its name / rate.
        let (info_tx, info_rx) = std::sync::mpsc::channel::<Result<(String, u32), String>>();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = stop_flag.clone();

        let thread = std::thread::Builder::new()
            .name("aec-capture".into())
            .spawn(move || {
                let result = run_aec_thread(sender, transmitting, stop_flag_clone, &info_tx);
                if let Err(e) = result {
                    eprintln!("[AEC] thread exited with error: {e}");
                }
            })
            .map_err(|e| format!("failed to spawn AEC thread: {e}"))?;

        // Wait for the thread to initialise WASAPI and report back.
        match info_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok((name, rate))) => Ok(AecCapture {
                thread: Some(thread),
                stop_flag,
                device_rate: rate,
                device_name: name,
            }),
            Ok(Err(e)) => {
                Err(e)
            }
            Err(_) => Err("AEC init timed out".into()),
        }
    }
}

impl Drop for AecCapture {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ── worker thread ───────────────────────────────────────────────────────────

fn run_aec_thread(
    sender: UnboundedSender<Vec<i16>>,
    transmitting: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    info_tx: &std::sync::mpsc::Sender<Result<(String, u32), String>>,
) -> Result<(), String> {
    unsafe {
        // Initialise COM for this thread
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| format!("CoInitializeEx failed: {e}"))?;

        // Enumerate the default Communications endpoints. We use the capture
        // endpoint for the mic; the render endpoint is referenced implicitly
        // by the OS when it wires up the AEC reference signal for any stream
        // tagged as AudioCategory_Communications.
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("CoCreateInstance MMDeviceEnumerator: {e}"))?;

        let _render_device = enumerator
            .GetDefaultAudioEndpoint(eRender, eCommunications)
            .map_err(|e| format!("GetDefaultAudioEndpoint render: {e}"))?;

        let cap_device = enumerator
            .GetDefaultAudioEndpoint(eCapture, eCommunications)
            .map_err(|e| format!("GetDefaultAudioEndpoint capture: {e}"))?;

        let device_name = get_device_friendly_name(&cap_device)
            .unwrap_or_else(|_| "Communications Microphone (AEC)".into());

        // Plain Activate — no pActivationParams. The previous code passed an
        // AUDIOCLIENT_ACTIVATION_PARAMS { ActivationType: PROCESS_LOOPBACK },
        // which is for cross-process loopback capture and has nothing to do
        // with AEC; it got silently ignored and we ended up with raw mic
        // audio bypassing every effect.
        let audio_client: IAudioClient = cap_device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("Activate IAudioClient: {e}"))?;

        // Tag the stream as Communications so the OS inserts the voice
        // comms APO chain (AEC / AGC / NS). This requires IAudioClient2 and
        // must happen *before* Initialize.
        let client2: IAudioClient2 = audio_client
            .cast()
            .map_err(|e| format!("cast IAudioClient2: {e}"))?;
        let props = AudioClientProperties {
            cbSize: std::mem::size_of::<AudioClientProperties>() as u32,
            bIsOffload: FALSE,
            eCategory: AudioCategory_Communications,
            // AUDCLNT_STREAMOPTIONS_RAW would bypass every effect and give us
            // the unprocessed mic — exactly the bug we're fixing. Use NONE so
            // the APO chain stays in.
            Options: AUDCLNT_STREAMOPTIONS_NONE,
        };
        client2
            .SetClientProperties(&props)
            .map_err(|e| format!("SetClientProperties(Communications): {e}"))?;

        // Query the mix format after SetClientProperties so we get the format
        // the effects chain will actually deliver.
        let mix_fmt_ptr = audio_client
            .GetMixFormat()
            .map_err(|e| format!("GetMixFormat: {e}"))?;
        let mix_fmt: &WAVEFORMATEX = &*mix_fmt_ptr;
        let capture_rate = mix_fmt.nSamplesPerSec;
        let capture_channels = mix_fmt.nChannels as usize;
        let bits_per_sample = mix_fmt.wBitsPerSample;

        eprintln!(
            "[AEC] WASAPI mix format: {}Hz, {} ch, {} bits",
            capture_rate, capture_channels, bits_per_sample
        );

        // 100 ms buffer
        let buffer_duration: i64 = 10_000_000; // 100 ms in 100-ns units
        audio_client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                buffer_duration,
                0,
                mix_fmt_ptr,
                None,
            )
            .map_err(|e| format!("IAudioClient::Initialize: {e}"))?;

        CoTaskMemFree(Some(mix_fmt_ptr as *const _ as *const _));

        // Set up event-driven capture
        let event = CreateEventW(None, false, false, PCWSTR::null())
            .map_err(|e| format!("CreateEventW: {e}"))?;
        audio_client
            .SetEventHandle(event)
            .map_err(|e| format!("SetEventHandle: {e}"))?;

        let capture_client: IAudioCaptureClient = audio_client
            .GetService()
            .map_err(|e| format!("GetService IAudioCaptureClient: {e}"))?;

        audio_client
            .Start()
            .map_err(|e| format!("IAudioClient::Start: {e}"))?;

        // Report success back to the spawning thread
        let _ = info_tx.send(Ok((device_name, capture_rate)));

        // Build resampler if needed
        let mut resampler = if capture_rate != TARGET_RATE {
            Some(
                Fft::<f32>::new(
                    capture_rate as usize,
                    TARGET_RATE as usize,
                    VOICE_FRAME,
                    1,
                    1,
                    FixedSync::Both,
                )
                .map_err(|e| format!("create AEC resampler: {e}"))?,
            )
        } else {
            None
        };

        let mut in_ring: VecDeque<f32> = VecDeque::with_capacity(VOICE_FRAME * 8);
        let mut frame: Vec<i16> = Vec::with_capacity(VOICE_FRAME);

        // Capture loop
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let wait = WaitForSingleObject(event, 200);
            if wait != WAIT_OBJECT_0 {
                continue;
            }

            if !transmitting.load(Ordering::Relaxed) {
                // drain the buffer without sending
                loop {
                    let mut data_ptr = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    match capture_client.GetBuffer(
                        &mut data_ptr,
                        &mut frames,
                        &mut flags,
                        None,
                        None,
                    ) {
                        Ok(()) if frames > 0 => {
                            let _ = capture_client.ReleaseBuffer(frames);
                        }
                        _ => break,
                    }
                }
                frame.clear();
                in_ring.clear();
                if let Some(ref mut r) = resampler {
                    r.reset();
                }
                continue;
            }

            // Read all available packets
            loop {
                let mut data_ptr: *mut u8 = std::ptr::null_mut();
                let mut frames_available = 0u32;
                let mut flags = 0u32;

                match capture_client.GetBuffer(
                    &mut data_ptr,
                    &mut frames_available,
                    &mut flags,
                    None,
                    None,
                ) {
                    Ok(()) if frames_available > 0 => {}
                    _ => break,
                }

                // Convert interleaved float32 to mono f32
                let total_samples = frames_available as usize * capture_channels;
                let float_slice =
                    std::slice::from_raw_parts(data_ptr as *const f32, total_samples);

                for frame_chunk in float_slice.chunks(capture_channels.max(1)) {
                    let sum: f32 = frame_chunk.iter().copied().sum();
                    let mono = sum / frame_chunk.len() as f32;
                    in_ring.push_back(mono);
                }

                let _ = capture_client.ReleaseBuffer(frames_available);

                // Resample / passthrough into 160-sample frames
                if let Some(ref mut rs) = resampler {
                    let chunk_size = rs.input_frames_next();
                    while in_ring.len() >= chunk_size {
                        let chunk: Vec<f32> = in_ring.drain(..chunk_size).collect();
                        let input_data = vec![chunk];
                        if let Ok(adapter) =
                            SequentialSliceOfVecs::new(&input_data, 1, chunk_size)
                        {
                            if let Ok(out) = rs.process(&adapter, 0, None) {
                                if let Some(iter) = out.iter_channel(0) {
                                    for s in iter {
                                        let pcm =
                                            (s * 32768.0).clamp(-32768.0, 32767.0) as i16;
                                        frame.push(pcm);
                                        if frame.len() == VOICE_FRAME {
                                            let out = std::mem::replace(
                                                &mut frame,
                                                Vec::with_capacity(VOICE_FRAME),
                                            );
                                            let _ = sender.send(out);
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // passthrough — device is already 8 kHz
                    while let Some(s) = in_ring.pop_front() {
                        let pcm = (s * 32768.0).clamp(-32768.0, 32767.0) as i16;
                        frame.push(pcm);
                        if frame.len() == VOICE_FRAME {
                            let out = std::mem::replace(
                                &mut frame,
                                Vec::with_capacity(VOICE_FRAME),
                            );
                            let _ = sender.send(out);
                        }
                    }
                }
            }
        }

        audio_client.Stop().ok();
        CloseHandle(event).ok();
        Ok(())
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

unsafe fn get_device_friendly_name(
    device: &windows::Win32::Media::Audio::IMMDevice,
) -> Result<String, String> {
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::System::Com::STGM_READ;
    use windows::core::PROPVARIANT;

    // PKEY_Device_FriendlyName
    let pkey = windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY {
        fmtid: GUID::from_values(
            0xa45c254e, 0xdf1c, 0x4efd,
            [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
        ),
        pid: 14,
    };

    let store: IPropertyStore = device
        .OpenPropertyStore(STGM_READ)
        .map_err(|e| e.to_string())?;
    let pv: PROPVARIANT = store.GetValue(&pkey).map_err(|e| e.to_string())?;

    // PROPVARIANT with VT_LPWSTR (value type 31)
    let vt = { pv.as_raw().Anonymous.Anonymous.vt };
    if vt == 31 {
        let pwstr = pv.as_raw().Anonymous.Anonymous.Anonymous.pwszVal;
        if !pwstr.is_null() {
            let len = (0..).take_while(|&i| *pwstr.add(i) != 0).count();
            let slice = std::slice::from_raw_parts(pwstr, len);
            return Ok(String::from_utf16_lossy(slice));
        }
    }
    Err("no friendly name".into())
}
