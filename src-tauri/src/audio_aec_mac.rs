/// macOS AUVoiceIO AEC (Acoustic Echo Cancellation) capture
///
/// Uses `kAudioUnitSubType_VoiceProcessingIO` instead of the normal
/// `RemoteIO` AudioUnit. The OS automatically applies AEC, AGC, and
/// noise suppression before delivering samples to the input callback.
/// Frames are resampled to 8 kHz mono and pushed through the same
/// `UnboundedSender<Vec<i16>>` used by the cpal path.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use coreaudio_sys::{
    kAudioUnitManufacturer_Apple,
    kAudioUnitSubType_VoiceProcessingIO,
    kAudioUnitType_Output,
    AudioBufferList, AudioComponent, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentInstanceNew, AudioStreamBasicDescription,
    AudioTimeStamp, AudioUnit, AudioUnitElement,
    AudioUnitGetProperty, AudioUnitInitialize, AudioUnitRenderActionFlags,
    AudioUnitSetProperty, OSStatus,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
    kAudioOutputUnitProperty_EnableIO,
    kAudioUnitProperty_StreamFormat,
    kAudioUnitScope_Global, kAudioUnitScope_Input, kAudioUnitScope_Output,
};

use rubato::{Fft, FixedSync, Resampler};
use audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::audioadapter::AdapterIterators;

const TARGET_RATE: u32 = 8_000;
const VOICE_FRAME: usize = 160;

// Bus indices for VoiceProcessingIO
const BUS_OUTPUT: AudioUnitElement = 0; // speaker output
const BUS_INPUT: AudioUnitElement = 1;  // microphone input

// ── public API ───────────────────────────────────────────────────────────────

pub struct AecCapture {
    /// The AudioUnit — must stay alive for the duration of capture
    audio_unit: AudioUnit,
    /// Kept alive so the raw pointer passed to the C callback remains valid
    _state: Arc<Mutex<CallbackState>>,
    /// Raw pointer handed to the C callback via `Arc::into_raw`; reclaimed in Drop
    callback_state_raw: *const Mutex<CallbackState>,
    pub device_rate: u32,
    pub device_name: String,
}

// AudioUnit is a *mut c_void — safe to Send because we control its lifetime
// via the Mutex in AudioInner.
unsafe impl Send for AecCapture {}

impl AecCapture {
    pub fn start(
        sender: UnboundedSender<Vec<i16>>,
        transmitting: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        unsafe { start_voice_io(sender, transmitting) }
    }
}

impl Drop for AecCapture {
    fn drop(&mut self) {
        unsafe {
            // 先停掉 AudioUnit，确保回调不会再访问 raw 指针
            coreaudio_sys::AudioOutputUnitStop(self.audio_unit);
            coreaudio_sys::AudioUnitUninitialize(self.audio_unit);
            coreaudio_sys::AudioComponentInstanceDispose(self.audio_unit);
            // 回收 Arc::into_raw 泄出的强引用，让 CallbackState 正常释放
            if !self.callback_state_raw.is_null() {
                drop(Arc::from_raw(self.callback_state_raw));
            }
        }
    }
}

// ── callback state shared with the C callback ────────────────────────────────

struct CallbackState {
    sender: UnboundedSender<Vec<i16>>,
    transmitting: Arc<AtomicBool>,
    resampler: Option<Fft<f32>>,
    in_ring: VecDeque<f32>,
    frame: Vec<i16>,
    capture_channels: usize,
    /// AudioUnit handle needed inside the callback to pull input data
    audio_unit: AudioUnit,
}

// ── unsafe impl ──────────────────────────────────────────────────────────────

unsafe fn start_voice_io(
    sender: UnboundedSender<Vec<i16>>,
    transmitting: Arc<AtomicBool>,
) -> Result<AecCapture, String> {
    // Find VoiceProcessingIO AudioComponent
    let desc = AudioComponentDescription {
        componentType: kAudioUnitType_Output,
        componentSubType: kAudioUnitSubType_VoiceProcessingIO,
        componentManufacturer: kAudioUnitManufacturer_Apple,
        componentFlags: 0,
        componentFlagsMask: 0,
    };

    let component: AudioComponent = AudioComponentFindNext(std::ptr::null_mut(), &desc);
    if component.is_null() {
        return Err("VoiceProcessingIO AudioComponent not found".into());
    }

    let mut audio_unit: AudioUnit = std::ptr::null_mut();
    check_os(
        AudioComponentInstanceNew(component, &mut audio_unit),
        "AudioComponentInstanceNew",
    )?;

    // VoiceProcessingIO 是全双工单元，Apple 官方示例总是让 input 和 output 两条 bus
    // 都保持 enable。如果禁用 output bus，输入 render 回调会不被触发，或 AudioUnitRender
    // 持续返回静音 —— AEC/AGC/NS 流水线需要 output 侧时钟驱动。
    let enable: u32 = 1;
    check_os(
        AudioUnitSetProperty(
            audio_unit,
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Input,
            BUS_INPUT,
            &enable as *const _ as *const _,
            std::mem::size_of::<u32>() as u32,
        ),
        "EnableIO input",
    )?;
    check_os(
        AudioUnitSetProperty(
            audio_unit,
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Output,
            BUS_OUTPUT,
            &enable as *const _ as *const _,
            std::mem::size_of::<u32>() as u32,
        ),
        "EnableIO output",
    )?;

    // Query the hardware input format
    let mut hw_fmt: AudioStreamBasicDescription = std::mem::zeroed();
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    check_os(
        AudioUnitGetProperty(
            audio_unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output, // scope Output of bus Input = what mic delivers
            BUS_INPUT,
            &mut hw_fmt as *mut _ as *mut _,
            &mut size,
        ),
        "GetProperty StreamFormat",
    )?;

    let capture_rate = hw_fmt.mSampleRate as u32;
    let capture_channels = hw_fmt.mChannelsPerFrame as usize;

    eprintln!(
        "[AEC-Mac] VoiceProcessingIO: {}Hz, {} ch",
        capture_rate, capture_channels
    );

    // Set the format we want the callback to receive: native float32 interleaved
    let fmt = AudioStreamBasicDescription {
        mSampleRate: hw_fmt.mSampleRate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBitsPerChannel: 32,
        mChannelsPerFrame: capture_channels as u32,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4 * capture_channels as u32,
        mBytesPerPacket: 4 * capture_channels as u32,
        mReserved: 0,
    };
    check_os(
        AudioUnitSetProperty(
            audio_unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            BUS_INPUT,
            &fmt as *const _ as *const _,
            std::mem::size_of::<AudioStreamBasicDescription>() as u32,
        ),
        "SetProperty StreamFormat",
    )?;

    // Build resampler
    let resampler = if capture_rate != TARGET_RATE {
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

    // Allocate shared callback state
    let state = Arc::new(Mutex::new(CallbackState {
        sender,
        transmitting,
        resampler,
        in_ring: VecDeque::with_capacity(VOICE_FRAME * 8),
        frame: Vec::with_capacity(VOICE_FRAME),
        capture_channels,
        audio_unit,
    }));

    // Register the input render callback
    let state_ptr = Arc::into_raw(Arc::clone(&state)) as *mut std::ffi::c_void;
    let callback = coreaudio_sys::AURenderCallbackStruct {
        inputProc: Some(input_render_callback),
        inputProcRefCon: state_ptr,
    };
    check_os(
        AudioUnitSetProperty(
            audio_unit,
            coreaudio_sys::kAudioOutputUnitProperty_SetInputCallback,
            kAudioUnitScope_Global,
            BUS_INPUT,
            &callback as *const _ as *const _,
            std::mem::size_of::<coreaudio_sys::AURenderCallbackStruct>() as u32,
        ),
        "SetInputCallback",
    )?;

    check_os(AudioUnitInitialize(audio_unit), "AudioUnitInitialize")?;
    check_os(
        coreaudio_sys::AudioOutputUnitStart(audio_unit),
        "AudioOutputUnitStart",
    )?;

    Ok(AecCapture {
        audio_unit,
        _state: state,
        callback_state_raw: state_ptr as *const Mutex<CallbackState>,
        device_rate: capture_rate,
        device_name: "Built-in Microphone (AEC)".into(),
    })
}

// ── C-compatible render callback ─────────────────────────────────────────────

unsafe extern "C" fn input_render_callback(
    in_ref_con: *mut std::ffi::c_void,
    _io_action_flags: *mut AudioUnitRenderActionFlags,
    in_time_stamp: *const AudioTimeStamp,
    in_bus_number: AudioUnitElement,
    in_number_frames: u32,
    _io_data: *mut AudioBufferList,
) -> OSStatus {
    // Reconstruct Arc without consuming it (we'll see it again next callback)
    let state_arc = Arc::from_raw(in_ref_con as *const Mutex<CallbackState>);
    let result = process_input(&state_arc, _io_action_flags, in_time_stamp, in_bus_number, in_number_frames);
    // Keep the Arc alive — put it back
    std::mem::forget(state_arc);
    result
}

unsafe fn process_input(
    state_arc: &Arc<Mutex<CallbackState>>,
    _io_action_flags: *mut AudioUnitRenderActionFlags,
    in_time_stamp: *const AudioTimeStamp,
    in_bus_number: AudioUnitElement,
    in_number_frames: u32,
) -> OSStatus {
    let mut guard = match state_arc.lock() {
        Ok(g) => g,
        Err(_) => return -1,
    };

    // Pull input samples from the AudioUnit into our buffer
    let channels = guard.capture_channels.max(1);
    let total_floats = in_number_frames as usize * channels;
    let mut buf: Vec<f32> = vec![0.0f32; total_floats];

    let mut abl = AudioBufferList {
        mNumberBuffers: 1,
        mBuffers: [coreaudio_sys::AudioBuffer {
            mNumberChannels: channels as u32,
            mDataByteSize: (total_floats * 4) as u32,
            mData: buf.as_mut_ptr() as *mut _,
        }],
    };

    let audio_unit = guard.audio_unit;
    let status = coreaudio_sys::AudioUnitRender(
        audio_unit,
        _io_action_flags,
        in_time_stamp,
        in_bus_number,
        in_number_frames,
        &mut abl,
    );
    if status != 0 {
        return status;
    }

    if !guard.transmitting.load(Ordering::Relaxed) {
        guard.in_ring.clear();
        guard.frame.clear();
        if let Some(ref mut r) = guard.resampler {
            r.reset();
        }
        return 0;
    }

    // Mix channels to mono and push to in_ring
    for frame in buf.chunks(channels) {
        let sum: f32 = frame.iter().copied().sum();
        guard.in_ring.push_back(sum / channels as f32);
    }

    // Resample / passthrough → 160-sample frames
    // Destructure to hold independent mutable refs and avoid borrow conflicts
    let CallbackState {
        ref mut resampler,
        ref mut in_ring,
        ref mut frame,
        ref sender,
        ..
    } = *guard;

    if let Some(ref mut rs) = resampler {
        let chunk_size = rs.input_frames_next();
        while in_ring.len() >= chunk_size {
            let chunk: Vec<f32> = in_ring.drain(..chunk_size).collect();
            let input_data = vec![chunk];
            if let Ok(adapter) = SequentialSliceOfVecs::new(&input_data, 1, chunk_size) {
                if let Ok(out) = rs.process(&adapter, 0, None) {
                    if let Some(iter) = out.iter_channel(0) {
                        for s in iter {
                            let pcm = (s * 32768.0).clamp(-32768.0, 32767.0) as i16;
                            frame.push(pcm);
                            if frame.len() == VOICE_FRAME {
                                let out = std::mem::replace(
                                    frame,
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
        while let Some(s) = in_ring.pop_front() {
            let pcm = (s * 32768.0).clamp(-32768.0, 32767.0) as i16;
            frame.push(pcm);
            if frame.len() == VOICE_FRAME {
                let out = std::mem::replace(
                    frame,
                    Vec::with_capacity(VOICE_FRAME),
                );
                let _ = sender.send(out);
            }
        }
    }

    0
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn check_os(status: OSStatus, label: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{label} failed: OSStatus {status}"))
    }
}
