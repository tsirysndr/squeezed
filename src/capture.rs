//! macOS-only: capture PCM from the "Squeezed" virtual audio device.
//!
//! The HAL driver (driver/src/lib.rs, installed via `squeezed driver
//! install`) loops everything the system plays on the device back to its
//! input stream as interleaved stereo Float32. This module finds that device
//! by UID, pins its nominal sample rate to the configured rate, and runs a
//! CoreAudio IOProc that converts each callback's samples to the configured
//! integer PCM format and pushes them into the broadcast buffer.
//!
//! CoreAudio is called through hand-rolled FFI rather than a bindings crate —
//! the handful of calls needed doesn't justify a dependency tree.

#![cfg(target_os = "macos")]

use crate::audio::AudioFormat;
use crate::broadcast::BroadcastBuffer;
use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

/// Device UID published by the driver — must match DEVICE_UID in
/// driver/src/lib.rs.
pub const DEVICE_UID: &str = "SqueezedAudioDevice_UID";

/// Sample rates the driver advertises (SUPPORTED_RATES in driver/src/lib.rs).
pub const SUPPORTED_RATES: [u32; 4] = [44100, 48000, 88200, 96000];

// --- CoreAudio / CoreFoundation FFI ----------------------------------------

type OSStatus = i32;
type AudioObjectID = u32;
type CFStringRef = *const c_void;

const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
const K_AUDIO_OBJECT_UNKNOWN: AudioObjectID = 0;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const K_SELECTOR_TRANSLATE_UID_TO_DEVICE: u32 = fourcc(b"uidd");
const K_SELECTOR_NOMINAL_SAMPLE_RATE: u32 = fourcc(b"nsrt");
const K_SCOPE_GLOBAL: u32 = fourcc(b"glob");

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

impl AudioObjectPropertyAddress {
    const fn global(selector: u32) -> Self {
        AudioObjectPropertyAddress {
            selector,
            scope: K_SCOPE_GLOBAL,
            element: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1], // variable-length in reality
}

/// Only ever handled by pointer; the layout doesn't matter to us.
#[repr(C)]
struct AudioTimeStamp {
    _opaque: [u8; 64],
}

type AudioDeviceIOProc = unsafe extern "C" fn(
    device: AudioObjectID,
    now: *const AudioTimeStamp,
    input_data: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    output_data: *mut AudioBufferList,
    output_time: *const AudioTimeStamp,
    client_data: *mut c_void,
) -> OSStatus;

type AudioDeviceIOProcID = Option<AudioDeviceIOProc>;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        object: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier_data: *const c_void,
        io_data_size: *mut u32,
        out_data: *mut c_void,
    ) -> OSStatus;
    fn AudioObjectSetPropertyData(
        object: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier_data: *const c_void,
        data_size: u32,
        data: *const c_void,
    ) -> OSStatus;
    fn AudioDeviceCreateIOProcID(
        device: AudioObjectID,
        io_proc: AudioDeviceIOProc,
        client_data: *mut c_void,
        out_proc_id: *mut AudioDeviceIOProcID,
    ) -> OSStatus;
    fn AudioDeviceDestroyIOProcID(device: AudioObjectID, proc_id: AudioDeviceIOProcID) -> OSStatus;
    fn AudioDeviceStart(device: AudioObjectID, proc_id: AudioDeviceIOProcID) -> OSStatus;
    #[allow(dead_code)]
    fn AudioDeviceStop(device: AudioObjectID, proc_id: AudioDeviceIOProcID) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithBytes(
        alloc: *const c_void,
        bytes: *const u8,
        num_bytes: isize,
        encoding: u32,
        is_external: u8,
    ) -> CFStringRef;
    fn CFRelease(cf: *const c_void);
}

// --- Device lookup ---------------------------------------------------------

/// Ask the HAL to translate the driver's device UID to a live AudioObjectID.
pub fn find_device() -> anyhow::Result<Option<AudioObjectID>> {
    let uid = unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            DEVICE_UID.as_ptr(),
            DEVICE_UID.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
            0,
        )
    };
    anyhow::ensure!(
        !uid.is_null(),
        "capture: creating CFString for device UID failed"
    );

    let addr = AudioObjectPropertyAddress::global(K_SELECTOR_TRANSLATE_UID_TO_DEVICE);
    let mut device: AudioObjectID = K_AUDIO_OBJECT_UNKNOWN;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &addr,
            std::mem::size_of::<CFStringRef>() as u32,
            &uid as *const CFStringRef as *const c_void,
            &mut size,
            &mut device as *mut AudioObjectID as *mut c_void,
        )
    };
    unsafe { CFRelease(uid) };
    anyhow::ensure!(
        status == 0,
        "capture: device UID lookup failed (OSStatus {status})"
    );
    Ok((device != K_AUDIO_OBJECT_UNKNOWN).then_some(device))
}

fn nominal_sample_rate(device: AudioObjectID) -> anyhow::Result<f64> {
    let addr = AudioObjectPropertyAddress::global(K_SELECTOR_NOMINAL_SAMPLE_RATE);
    let mut rate: f64 = 0.0;
    let mut size = std::mem::size_of::<f64>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut rate as *mut f64 as *mut c_void,
        )
    };
    anyhow::ensure!(
        status == 0,
        "capture: reading sample rate failed (OSStatus {status})"
    );
    Ok(rate)
}

/// Pin the device to `rate`. The HAL applies rate changes asynchronously, so
/// poll until it lands before starting IO.
fn set_nominal_sample_rate(device: AudioObjectID, rate: u32) -> anyhow::Result<()> {
    if nominal_sample_rate(device)? == rate as f64 {
        return Ok(());
    }
    let addr = AudioObjectPropertyAddress::global(K_SELECTOR_NOMINAL_SAMPLE_RATE);
    let value = rate as f64;
    let status = unsafe {
        AudioObjectSetPropertyData(
            device,
            &addr,
            0,
            std::ptr::null(),
            std::mem::size_of::<f64>() as u32,
            &value as *const f64 as *const c_void,
        )
    };
    anyhow::ensure!(
        status == 0,
        "capture: setting device sample rate to {rate} Hz failed (OSStatus {status})"
    );
    for _ in 0..50 {
        if nominal_sample_rate(device)? == rate as f64 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    anyhow::bail!("capture: device did not switch to {rate} Hz in time");
}

// --- IOProc ----------------------------------------------------------------

struct CaptureCtx {
    buf: Arc<BroadcastBuffer>,
    channels: u8,
    bits: u8,
    scratch: Vec<u8>,
}

/// Append one output sample (clamped [-1, 1] float) as little-endian PCM.
#[inline]
fn write_sample(out: &mut Vec<u8>, bits: u8, v: f32) {
    let v = v.clamp(-1.0, 1.0) as f64;
    match bits {
        8 => out.push((v * i8::MAX as f64) as i8 as u8),
        16 => out.extend_from_slice(&((v * i16::MAX as f64) as i16).to_le_bytes()),
        24 => out.extend_from_slice(&((v * 8_388_607.0) as i32).to_le_bytes()[..3]),
        _ => out.extend_from_slice(&((v * i32::MAX as f64) as i32).to_le_bytes()),
    }
}

/// Runs on the HAL's IO thread each cycle: convert the driver's Float32 frames
/// to the configured format and hand them to the broadcast buffer. The push
/// takes a short mutex, which is tolerable at our buffer sizes.
unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input_data: *const AudioBufferList,
    _input_time: *const AudioTimeStamp,
    _output_data: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client_data: *mut c_void,
) -> OSStatus {
    let ctx = &mut *(client_data as *mut CaptureCtx);
    if input_data.is_null() {
        return 0;
    }
    let list = &*input_data;
    ctx.scratch.clear();
    let buffers = std::slice::from_raw_parts(list.buffers.as_ptr(), list.number_buffers as usize);
    for buffer in buffers {
        if buffer.data.is_null() {
            continue;
        }
        let in_channels = buffer.number_channels.max(1) as usize;
        let samples = std::slice::from_raw_parts(
            buffer.data as *const f32,
            buffer.data_byte_size as usize / std::mem::size_of::<f32>(),
        );
        for frame in samples.chunks_exact(in_channels) {
            match ctx.channels {
                1 => {
                    let avg = frame.iter().sum::<f32>() / in_channels as f32;
                    write_sample(&mut ctx.scratch, ctx.bits, avg);
                }
                _ => {
                    write_sample(&mut ctx.scratch, ctx.bits, frame[0]);
                    write_sample(&mut ctx.scratch, ctx.bits, frame[in_channels.min(2) - 1]);
                }
            }
        }
    }
    ctx.buf.push(&ctx.scratch);
    0
}

// --- Entry point -----------------------------------------------------------

/// Start capturing from the virtual device and never return (the device keeps
/// producing frames — silence when nothing is routed to it — for as long as
/// squeezed runs).
pub fn run(format: AudioFormat, buf: Arc<BroadcastBuffer>) -> anyhow::Result<()> {
    anyhow::ensure!(
        SUPPORTED_RATES.contains(&format.sample_rate),
        "input source 'virtual' supports sample rates {SUPPORTED_RATES:?}, not {} Hz",
        format.sample_rate
    );

    let device = find_device()?.ok_or_else(|| {
        anyhow::anyhow!(
            "the Squeezed virtual audio device is not present — install it with \
             `sudo squeezed driver install` (then re-run squeezed)"
        )
    })?;
    set_nominal_sample_rate(device, format.sample_rate)?;

    // The context lives for the rest of the process (capture never stops).
    let byte_rate = format.byte_rate();
    let ctx = Box::into_raw(Box::new(CaptureCtx {
        buf,
        channels: format.channels,
        bits: format.bits,
        scratch: Vec::with_capacity(byte_rate / 4),
    }));

    let mut proc_id: AudioDeviceIOProcID = None;
    let status =
        unsafe { AudioDeviceCreateIOProcID(device, io_proc, ctx as *mut c_void, &mut proc_id) };
    if status != 0 || proc_id.is_none() {
        drop(unsafe { Box::from_raw(ctx) });
        anyhow::bail!("capture: creating the CoreAudio IO proc failed (OSStatus {status})");
    }
    let status = unsafe { AudioDeviceStart(device, proc_id) };
    if status != 0 {
        unsafe { AudioDeviceDestroyIOProcID(device, proc_id) };
        drop(unsafe { Box::from_raw(ctx) });
        anyhow::bail!("capture: starting the CoreAudio device failed (OSStatus {status})");
    }

    tracing::info!(
        "input: capturing from the 'Squeezed' virtual output device — select it in \
         System Settings → Sound → Output"
    );
    loop {
        std::thread::park();
    }
}
