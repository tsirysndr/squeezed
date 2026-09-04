//! Core Audio HAL AudioServerPlugIn for the "Squeezed" virtual audio device.
//!
//! Loaded by `coreaudiod` from `/Library/Audio/Plug-Ins/HAL/SqueezedAudio.driver`.
//! It publishes one virtual device with an output stream and an input stream that
//! share a ring buffer: whatever macOS plays to the output can be captured back
//! from the input by `squeezed --source virtual`, which serves it to
//! Squeezelite players. Modeled on Apple's NullAudio sample / BlackHole.
//!
//! coreaudiod's sandbox forbids network access, which is why the driver only
//! loops audio back and a separate user-space process does the networking.

#![cfg(target_os = "macos")]
#![allow(non_snake_case)]

use std::cell::UnsafeCell;
use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

// ===========================================================================
// FourCC helpers and CoreAudio constants
// ===========================================================================

const fn fcc(b: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*b)
}

// Object IDs published by this driver. The plug-in object must be
// kAudioObjectPlugInObject (1); the rest are ours to assign.
const OBJ_PLUGIN: u32 = 1;
const OBJ_DEVICE: u32 = 2;
const OBJ_STREAM_OUTPUT: u32 = 3;
const OBJ_STREAM_INPUT: u32 = 4;
const OBJ_VOLUME: u32 = 5;
const OBJ_MUTE: u32 = 6;
const OBJ_UNKNOWN: u32 = 0;

// OSStatus results
const NO_ERR: i32 = 0;
const ERR_UNKNOWN_PROPERTY: i32 = fcc(b"who?") as i32;
const ERR_BAD_PROPERTY_SIZE: i32 = fcc(b"!siz") as i32;
const ERR_ILLEGAL_OPERATION: i32 = fcc(b"nope") as i32;
const ERR_BAD_OBJECT: i32 = fcc(b"!obj") as i32;
const ERR_UNSUPPORTED_OPERATION: i32 = fcc(b"unop") as i32;
const ERR_UNSUPPORTED_FORMAT: i32 = fcc(b"!dat") as i32;
const E_NOINTERFACE: i32 = 0x8000_0004u32 as i32;
const E_POINTER: i32 = 0x8000_4003u32 as i32;

// Property selectors
const SEL_BASE_CLASS: u32 = fcc(b"bcls");
const SEL_CLASS: u32 = fcc(b"clas");
const SEL_OWNER: u32 = fcc(b"stdv");
const SEL_NAME: u32 = fcc(b"lnam");
const SEL_MANUFACTURER: u32 = fcc(b"lmak");
const SEL_OWNED_OBJECTS: u32 = fcc(b"ownd");
const SEL_IDENTIFY: u32 = fcc(b"iden");
const SEL_CUSTOM_PROPERTY_INFO_LIST: u32 = fcc(b"cust");

const SEL_PLUGIN_DEVICE_LIST: u32 = fcc(b"dev#");
const SEL_PLUGIN_TRANSLATE_UID_TO_DEVICE: u32 = fcc(b"uidd");
const SEL_PLUGIN_BOX_LIST: u32 = fcc(b"box#");
const SEL_PLUGIN_TRANSLATE_UID_TO_BOX: u32 = fcc(b"uidb");
const SEL_PLUGIN_RESOURCE_BUNDLE: u32 = fcc(b"rsrc");

const SEL_DEVICE_UID: u32 = fcc(b"uid ");
const SEL_DEVICE_MODEL_UID: u32 = fcc(b"muid");
const SEL_DEVICE_TRANSPORT_TYPE: u32 = fcc(b"tran");
const SEL_DEVICE_RELATED_DEVICES: u32 = fcc(b"akin");
const SEL_DEVICE_CLOCK_DOMAIN: u32 = fcc(b"clkd");
const SEL_DEVICE_IS_ALIVE: u32 = fcc(b"livn");
const SEL_DEVICE_IS_RUNNING: u32 = fcc(b"goin");
const SEL_DEVICE_CAN_BE_DEFAULT: u32 = fcc(b"dflt");
const SEL_DEVICE_CAN_BE_SYSTEM_DEFAULT: u32 = fcc(b"sflt");
const SEL_DEVICE_LATENCY: u32 = fcc(b"ltnc");
const SEL_DEVICE_STREAMS: u32 = fcc(b"stm#");
const SEL_DEVICE_STREAM_CONFIGURATION: u32 = fcc(b"slay");
const SEL_CONTROL_LIST: u32 = fcc(b"ctrl");
const SEL_DEVICE_SAFETY_OFFSET: u32 = fcc(b"saft");
const SEL_DEVICE_NOMINAL_SAMPLE_RATE: u32 = fcc(b"nsrt");
const SEL_DEVICE_AVAILABLE_SAMPLE_RATES: u32 = fcc(b"nsr#");
const SEL_DEVICE_IS_HIDDEN: u32 = fcc(b"hidn");
const SEL_DEVICE_PREFERRED_CHANNELS_STEREO: u32 = fcc(b"dch2");
const SEL_DEVICE_ZERO_TIMESTAMP_PERIOD: u32 = fcc(b"ring");

const SEL_STREAM_IS_ACTIVE: u32 = fcc(b"sact");
const SEL_STREAM_DIRECTION: u32 = fcc(b"sdir");
const SEL_STREAM_TERMINAL_TYPE: u32 = fcc(b"term");
const SEL_STREAM_STARTING_CHANNEL: u32 = fcc(b"schn");
const SEL_STREAM_LATENCY: u32 = fcc(b"ltnc");
const SEL_STREAM_VIRTUAL_FORMAT: u32 = fcc(b"sfmt");
const SEL_STREAM_AVAILABLE_VIRTUAL_FORMATS: u32 = fcc(b"sfma");
const SEL_STREAM_PHYSICAL_FORMAT: u32 = fcc(b"pft ");
const SEL_STREAM_AVAILABLE_PHYSICAL_FORMATS: u32 = fcc(b"pfta");

const SEL_CONTROL_SCOPE: u32 = fcc(b"cscp");
const SEL_CONTROL_ELEMENT: u32 = fcc(b"celm");
const SEL_LEVEL_SCALAR_VALUE: u32 = fcc(b"lcsv");
const SEL_LEVEL_DECIBEL_VALUE: u32 = fcc(b"lcdv");
const SEL_LEVEL_DECIBEL_RANGE: u32 = fcc(b"lcdr");
const SEL_LEVEL_SCALAR_TO_DECIBELS: u32 = fcc(b"lcsd");
const SEL_LEVEL_DECIBELS_TO_SCALAR: u32 = fcc(b"lcds");
const SEL_BOOLEAN_VALUE: u32 = fcc(b"bcvl");

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

// One-buffer AudioBufferList describing our interleaved stereo stream, used for
// kAudioDevicePropertyStreamConfiguration. Without this the HAL cannot size the
// output buffer and never starts output IO.
#[repr(C)]
struct AudioBufferList1 {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

// Classes
const CLASS_OBJECT: u32 = fcc(b"aobj");
const CLASS_PLUGIN: u32 = fcc(b"aplg");
const CLASS_DEVICE: u32 = fcc(b"adev");
const CLASS_STREAM: u32 = fcc(b"astr");
const CLASS_LEVEL_CONTROL: u32 = fcc(b"levl");
const CLASS_VOLUME_CONTROL: u32 = fcc(b"vlme");
const CLASS_BOOLEAN_CONTROL: u32 = fcc(b"togl");
const CLASS_MUTE_CONTROL: u32 = fcc(b"mute");

// Scopes
const SCOPE_GLOBAL: u32 = fcc(b"glob");
const SCOPE_INPUT: u32 = fcc(b"inpt");
const SCOPE_OUTPUT: u32 = fcc(b"outp");

// Misc constants
const TRANSPORT_VIRTUAL: u32 = fcc(b"virt");
const TERMINAL_SPEAKER: u32 = fcc(b"spkr");
const TERMINAL_MICROPHONE: u32 = fcc(b"micr");
const FORMAT_LPCM: u32 = fcc(b"lpcm");
const FLAG_IS_FLOAT: u32 = 0x1;
const FLAG_IS_PACKED: u32 = 0x8;

// IO operations. On this device the HAL delivers output audio through
// ProcessOutput ('pout') and 'rite', not WriteMix ('wmix').
const OP_READ_INPUT: u32 = fcc(b"read");
const OP_WRITE_MIX: u32 = fcc(b"wmix");
const OP_PROCESS_OUTPUT: u32 = fcc(b"pout");
const OP_RITE: u32 = fcc(b"rite");

// Device parameters
const CHANNELS: usize = 2;
const RING_FRAMES: usize = 16384;
const SUPPORTED_RATES: [f64; 4] = [44100.0, 48000.0, 88200.0, 96000.0];
const DEFAULT_RATE: f64 = 44100.0;

const DEVICE_NAME: &core::ffi::CStr = c"Squeezed";
const MANUFACTURER: &core::ffi::CStr = c"Tsiry Sandratraina";
const DEVICE_UID: &core::ffi::CStr = c"SqueezedAudioDevice_UID";
const DEVICE_MODEL_UID: &core::ffi::CStr = c"SqueezedAudioDevice_ModelUID";
const STREAM_OUT_NAME: &core::ffi::CStr = c"Squeezed Output";
const STREAM_IN_NAME: &core::ffi::CStr = c"Squeezed Input";

// UUIDs, as raw bytes.
// kAudioServerPlugInTypeUUID: 443ABAB8-E7B3-491A-B985-BEB9187030DB
const TYPE_UUID: [u8; 16] = [
    0x44, 0x3A, 0xBA, 0xB8, 0xE7, 0xB3, 0x49, 0x1A, 0xB9, 0x85, 0xBE, 0xB9, 0x18, 0x70, 0x30, 0xDB,
];
// kAudioServerPlugInDriverInterfaceUUID: EEA5773D-CC43-49F1-8E00-8F96E7D23B17
const DRIVER_INTERFACE_UUID: [u8; 16] = [
    0xEE, 0xA5, 0x77, 0x3D, 0xCC, 0x43, 0x49, 0xF1, 0x8E, 0x00, 0x8F, 0x96, 0xE7, 0xD2, 0x3B, 0x17,
];
// IUnknown: 00000000-0000-0000-C000-000000000046
const IUNKNOWN_UUID: [u8; 16] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

// ===========================================================================
// C ABI types
// ===========================================================================

type OSStatus = i32;
type Boolean = u8;
type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFUUIDRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFDictionaryRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CFUUIDBytes([u8; 16]);

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioValueRange {
    minimum: f64,
    maximum: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioStreamRangedDescription {
    format: AudioStreamBasicDescription,
    sample_rate_range: AudioValueRange,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SMPTETime {
    subframes: i16,
    subframe_divisor: i16,
    counter: u32,
    smpte_type: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioTimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte_time: SMPTETime,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioServerPlugInIOCycleInfo {
    io_cycle_counter: u64,
    nominal_io_buffer_frame_size: u32,
    current_time: AudioTimeStamp,
    input_time: AudioTimeStamp,
    output_time: AudioTimeStamp,
    master_time: AudioTimeStamp,
}

#[repr(C)]
struct AudioServerPlugInHostInterface {
    properties_changed: unsafe extern "C" fn(
        host: AudioServerPlugInHostRef,
        object_id: u32,
        number_addresses: u32,
        addresses: *const AudioObjectPropertyAddress,
    ) -> OSStatus,
    copy_from_storage: unsafe extern "C" fn(
        host: AudioServerPlugInHostRef,
        key: CFStringRef,
        out_data: *mut CFTypeRef,
    ) -> OSStatus,
    write_to_storage: unsafe extern "C" fn(
        host: AudioServerPlugInHostRef,
        key: CFStringRef,
        data: CFTypeRef,
    ) -> OSStatus,
    delete_from_storage:
        unsafe extern "C" fn(host: AudioServerPlugInHostRef, key: CFStringRef) -> OSStatus,
    request_device_configuration_change: unsafe extern "C" fn(
        host: AudioServerPlugInHostRef,
        device_object_id: u32,
        change_action: u64,
        change_info: *mut c_void,
    ) -> OSStatus,
}

type AudioServerPlugInHostRef = *const AudioServerPlugInHostInterface;
type DriverRef = *mut *const AudioServerPlugInDriverInterface;

#[repr(C)]
struct AudioServerPlugInDriverInterface {
    _reserved: usize,
    query_interface: unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> OSStatus,
    add_ref: unsafe extern "C" fn(*mut c_void) -> u32,
    release: unsafe extern "C" fn(*mut c_void) -> u32,
    initialize: unsafe extern "C" fn(DriverRef, AudioServerPlugInHostRef) -> OSStatus,
    create_device:
        unsafe extern "C" fn(DriverRef, CFDictionaryRef, *const c_void, *mut u32) -> OSStatus,
    destroy_device: unsafe extern "C" fn(DriverRef, u32) -> OSStatus,
    add_device_client: unsafe extern "C" fn(DriverRef, u32, *const c_void) -> OSStatus,
    remove_device_client: unsafe extern "C" fn(DriverRef, u32, *const c_void) -> OSStatus,
    perform_device_configuration_change:
        unsafe extern "C" fn(DriverRef, u32, u64, *mut c_void) -> OSStatus,
    abort_device_configuration_change:
        unsafe extern "C" fn(DriverRef, u32, u64, *mut c_void) -> OSStatus,
    has_property:
        unsafe extern "C" fn(DriverRef, u32, c_int, *const AudioObjectPropertyAddress) -> Boolean,
    is_property_settable: unsafe extern "C" fn(
        DriverRef,
        u32,
        c_int,
        *const AudioObjectPropertyAddress,
        *mut Boolean,
    ) -> OSStatus,
    get_property_data_size: unsafe extern "C" fn(
        DriverRef,
        u32,
        c_int,
        *const AudioObjectPropertyAddress,
        u32,
        *const c_void,
        *mut u32,
    ) -> OSStatus,
    get_property_data: unsafe extern "C" fn(
        DriverRef,
        u32,
        c_int,
        *const AudioObjectPropertyAddress,
        u32,
        *const c_void,
        u32,
        *mut u32,
        *mut c_void,
    ) -> OSStatus,
    set_property_data: unsafe extern "C" fn(
        DriverRef,
        u32,
        c_int,
        *const AudioObjectPropertyAddress,
        u32,
        *const c_void,
        u32,
        *const c_void,
    ) -> OSStatus,
    start_io: unsafe extern "C" fn(DriverRef, u32, u32) -> OSStatus,
    stop_io: unsafe extern "C" fn(DriverRef, u32, u32) -> OSStatus,
    get_zero_time_stamp:
        unsafe extern "C" fn(DriverRef, u32, u32, *mut f64, *mut u64, *mut u64) -> OSStatus,
    will_do_io_operation:
        unsafe extern "C" fn(DriverRef, u32, u32, u32, *mut Boolean, *mut Boolean) -> OSStatus,
    begin_io_operation: unsafe extern "C" fn(
        DriverRef,
        u32,
        u32,
        u32,
        u32,
        *const AudioServerPlugInIOCycleInfo,
    ) -> OSStatus,
    do_io_operation: unsafe extern "C" fn(
        DriverRef,
        u32,
        u32,
        u32,
        u32,
        u32,
        *const AudioServerPlugInIOCycleInfo,
        *mut c_void,
        *mut c_void,
    ) -> OSStatus,
    end_io_operation: unsafe extern "C" fn(
        DriverRef,
        u32,
        u32,
        u32,
        u32,
        *const AudioServerPlugInIOCycleInfo,
    ) -> OSStatus,
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringCompare(a: CFStringRef, b: CFStringRef, options: usize) -> isize;
    fn CFRelease(cf: CFTypeRef);
    fn CFUUIDGetUUIDBytes(uuid: CFUUIDRef) -> CFUUIDBytes;
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
    fn syslog(priority: c_int, fmt: *const c_char, ...);
}

fn log_msg(msg: &core::ffi::CStr) {
    // LOG_NOTICE (5) — visible in Console.app under coreaudiod.
    unsafe { syslog(5, c"Squeezed driver: %s".as_ptr(), msg.as_ptr()) };
}

fn host_ticks_per_second() -> f64 {
    static TPS: OnceLock<f64> = OnceLock::new();
    *TPS.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 1, denom: 1 };
        unsafe { mach_timebase_info(&mut info) };
        1_000_000_000.0 * info.denom as f64 / info.numer as f64
    })
}

// ===========================================================================
// Driver state
// ===========================================================================

struct State {
    host: usize, // AudioServerPlugInHostRef, stored as usize so State is Send
    sample_rate: f64,
    io_count: u32,
    anchor_host_time: u64,
    timestamp_seed: u64,
}

static STATE: Mutex<State> = Mutex::new(State {
    host: 0,
    sample_rate: DEFAULT_RATE,
    io_count: 0,
    anchor_host_time: 0,
    timestamp_seed: 1,
});

static REF_COUNT: AtomicU32 = AtomicU32::new(0);

// Output volume/mute, read lock-free by the IO path. VOLUME_BITS holds the
// f32 bit pattern of the 0..1 slider scalar; the applied amplitude is scalar³
// (≈ a perceptual curve), i.e. a -96..0 dB range as advertised below.
static VOLUME_BITS: AtomicU32 = AtomicU32::new(1.0f32.to_bits());
static MUTED: AtomicU32 = AtomicU32::new(0);

const MIN_DB: f64 = -96.0;
const MAX_DB: f64 = 0.0;

fn scalar_to_db(s: f32) -> f32 {
    if s <= 0.0 {
        return MIN_DB as f32;
    }
    (60.0 * (s as f64).log10()).max(MIN_DB) as f32
}

fn db_to_scalar(db: f32) -> f32 {
    let db = (db as f64).clamp(MIN_DB, MAX_DB);
    10f64.powf(db / 60.0).clamp(0.0, 1.0) as f32
}

// Loopback ring buffer. The realtime IO threads read and write it through raw
// pointers without locking (like BlackHole); torn f32 samples on rate changes
// are inaudible and preferable to priority inversion on the IO thread.
struct Ring(UnsafeCell<[f32; RING_FRAMES * CHANNELS]>);
unsafe impl Sync for Ring {}
static RING: Ring = Ring(UnsafeCell::new([0.0; RING_FRAMES * CHANNELS]));

fn ring_base() -> *mut f32 {
    RING.0.get() as *mut f32
}

fn current_asbd(sample_rate: f64) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        sample_rate,
        format_id: FORMAT_LPCM,
        format_flags: FLAG_IS_FLOAT | FLAG_IS_PACKED,
        bytes_per_packet: (CHANNELS * 4) as u32,
        frames_per_packet: 1,
        bytes_per_frame: (CHANNELS * 4) as u32,
        channels_per_frame: CHANNELS as u32,
        bits_per_channel: 32,
        reserved: 0,
    }
}

// ===========================================================================
// COM plumbing + factory
// ===========================================================================

struct InterfaceHolder(AudioServerPlugInDriverInterface);
unsafe impl Sync for InterfaceHolder {}

static INTERFACE: InterfaceHolder = InterfaceHolder(AudioServerPlugInDriverInterface {
    _reserved: 0,
    query_interface,
    add_ref,
    release,
    initialize,
    create_device,
    destroy_device,
    add_device_client,
    remove_device_client,
    perform_device_configuration_change,
    abort_device_configuration_change,
    has_property,
    is_property_settable,
    get_property_data_size,
    get_property_data,
    set_property_data,
    start_io,
    stop_io,
    get_zero_time_stamp,
    will_do_io_operation,
    begin_io_operation,
    do_io_operation,
    end_io_operation,
});

struct DriverRefHolder(UnsafeCell<*const AudioServerPlugInDriverInterface>);
unsafe impl Sync for DriverRefHolder {}
static DRIVER_REF: DriverRefHolder = DriverRefHolder(UnsafeCell::new(&INTERFACE.0));

fn our_ref() -> DriverRef {
    DRIVER_REF.0.get()
}

/// CFPlugIn factory, named in Info.plist's CFPlugInFactories. coreaudiod
/// always passes a valid CFUUIDRef (checked non-null before the deref).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn SqueezedAudioDriver_Create(
    _allocator: CFAllocatorRef,
    requested_type: CFUUIDRef,
) -> *mut c_void {
    if requested_type.is_null() {
        return ptr::null_mut();
    }
    let bytes = unsafe { CFUUIDGetUUIDBytes(requested_type) };
    if bytes.0 == TYPE_UUID {
        REF_COUNT.fetch_add(1, Ordering::SeqCst);
        log_msg(c"factory created driver instance");
        our_ref() as *mut c_void
    } else {
        ptr::null_mut()
    }
}

unsafe extern "C" fn query_interface(
    driver: *mut c_void,
    iid: CFUUIDBytes,
    out_interface: *mut *mut c_void,
) -> OSStatus {
    if out_interface.is_null() {
        return E_POINTER;
    }
    if driver != our_ref() as *mut c_void {
        return ERR_BAD_OBJECT;
    }
    if iid.0 == IUNKNOWN_UUID || iid.0 == DRIVER_INTERFACE_UUID {
        REF_COUNT.fetch_add(1, Ordering::SeqCst);
        unsafe { *out_interface = our_ref() as *mut c_void };
        NO_ERR
    } else {
        unsafe { *out_interface = ptr::null_mut() };
        E_NOINTERFACE
    }
}

unsafe extern "C" fn add_ref(_driver: *mut c_void) -> u32 {
    REF_COUNT.fetch_add(1, Ordering::SeqCst) + 1
}

unsafe extern "C" fn release(_driver: *mut c_void) -> u32 {
    // The interface is static; never freed. Just keep the count non-negative.
    let prev = REF_COUNT.fetch_sub(1, Ordering::SeqCst);
    prev.saturating_sub(1)
}

// ===========================================================================
// Lifecycle
// ===========================================================================

unsafe extern "C" fn initialize(driver: DriverRef, host: AudioServerPlugInHostRef) -> OSStatus {
    if driver != our_ref() {
        return ERR_BAD_OBJECT;
    }
    let mut state = STATE.lock().unwrap();
    state.host = host as usize;
    log_msg(c"initialized");
    NO_ERR
}

unsafe extern "C" fn create_device(
    _driver: DriverRef,
    _description: CFDictionaryRef,
    _client_info: *const c_void,
    _out_device: *mut u32,
) -> OSStatus {
    ERR_UNSUPPORTED_OPERATION
}

unsafe extern "C" fn destroy_device(_driver: DriverRef, _device: u32) -> OSStatus {
    ERR_UNSUPPORTED_OPERATION
}

unsafe extern "C" fn add_device_client(
    _driver: DriverRef,
    device: u32,
    _client_info: *const c_void,
) -> OSStatus {
    if device != OBJ_DEVICE {
        return ERR_BAD_OBJECT;
    }
    NO_ERR
}

unsafe extern "C" fn remove_device_client(
    _driver: DriverRef,
    device: u32,
    _client_info: *const c_void,
) -> OSStatus {
    if device != OBJ_DEVICE {
        return ERR_BAD_OBJECT;
    }
    NO_ERR
}

unsafe extern "C" fn perform_device_configuration_change(
    driver: DriverRef,
    device: u32,
    change_action: u64,
    _change_info: *mut c_void,
) -> OSStatus {
    if driver != our_ref() || device != OBJ_DEVICE {
        return ERR_BAD_OBJECT;
    }
    let new_rate = change_action as f64;
    if !SUPPORTED_RATES.contains(&new_rate) {
        return ERR_ILLEGAL_OPERATION;
    }
    let mut state = STATE.lock().unwrap();
    state.sample_rate = new_rate;
    state.anchor_host_time = unsafe { mach_absolute_time() };
    state.timestamp_seed += 1;
    log_msg(c"sample rate changed");
    NO_ERR
}

unsafe extern "C" fn abort_device_configuration_change(
    _driver: DriverRef,
    device: u32,
    _change_action: u64,
    _change_info: *mut c_void,
) -> OSStatus {
    if device != OBJ_DEVICE {
        return ERR_BAD_OBJECT;
    }
    NO_ERR
}

// ===========================================================================
// Property helpers
// ===========================================================================

unsafe fn out_scalar<T: Copy>(
    value: T,
    in_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    let need = size_of::<T>() as u32;
    if in_size < need {
        return ERR_BAD_PROPERTY_SIZE;
    }
    unsafe {
        ptr::write_unaligned(out_data as *mut T, value);
        *out_size = need;
    }
    NO_ERR
}

/// Write as many array items as fit in the caller's buffer (HAL convention).
unsafe fn out_array<T: Copy>(
    values: &[T],
    in_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    let item = size_of::<T>() as u32;
    let fit = (in_size / item).min(values.len() as u32);
    unsafe {
        for (i, v) in values.iter().take(fit as usize).enumerate() {
            ptr::write_unaligned((out_data as *mut T).add(i), *v);
        }
        *out_size = fit * item;
    }
    NO_ERR
}

unsafe fn out_cfstring(
    s: &core::ffi::CStr,
    in_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    let need = size_of::<CFStringRef>() as u32;
    if in_size < need {
        return ERR_BAD_PROPERTY_SIZE;
    }
    let cf = unsafe { CFStringCreateWithCString(ptr::null(), s.as_ptr(), CF_STRING_ENCODING_UTF8) };
    unsafe {
        // Ownership transfers to the HAL, which releases it.
        ptr::write_unaligned(out_data as *mut CFStringRef, cf);
        *out_size = need;
    }
    NO_ERR
}

fn available_formats(_scope: u32) -> [AudioStreamRangedDescription; SUPPORTED_RATES.len()] {
    let mut out = [AudioStreamRangedDescription {
        format: current_asbd(DEFAULT_RATE),
        sample_rate_range: AudioValueRange {
            minimum: DEFAULT_RATE,
            maximum: DEFAULT_RATE,
        },
    }; SUPPORTED_RATES.len()];
    for (i, rate) in SUPPORTED_RATES.iter().enumerate() {
        out[i].format = current_asbd(*rate);
        out[i].sample_rate_range = AudioValueRange {
            minimum: *rate,
            maximum: *rate,
        };
    }
    out
}

/// Full byte size of a property, or None when the property is unknown.
/// Shared by HasProperty and GetPropertyDataSize.
fn prop_size(object: u32, addr: &AudioObjectPropertyAddress) -> Option<u32> {
    let sel = addr.selector;
    let cf = size_of::<CFStringRef>() as u32;
    match object {
        OBJ_PLUGIN => match sel {
            SEL_BASE_CLASS | SEL_CLASS | SEL_OWNER => Some(4),
            SEL_NAME | SEL_MANUFACTURER | SEL_PLUGIN_RESOURCE_BUNDLE => Some(cf),
            SEL_OWNED_OBJECTS | SEL_PLUGIN_DEVICE_LIST => Some(4),
            SEL_PLUGIN_BOX_LIST => Some(0),
            SEL_PLUGIN_TRANSLATE_UID_TO_DEVICE | SEL_PLUGIN_TRANSLATE_UID_TO_BOX => Some(4),
            SEL_CUSTOM_PROPERTY_INFO_LIST => Some(0),
            _ => None,
        },
        OBJ_DEVICE => match sel {
            SEL_BASE_CLASS | SEL_CLASS | SEL_OWNER => Some(4),
            SEL_NAME | SEL_MANUFACTURER | SEL_DEVICE_UID | SEL_DEVICE_MODEL_UID => Some(cf),
            SEL_OWNED_OBJECTS => Some(16),
            SEL_DEVICE_TRANSPORT_TYPE
            | SEL_DEVICE_CLOCK_DOMAIN
            | SEL_DEVICE_IS_ALIVE
            | SEL_DEVICE_IS_RUNNING
            | SEL_DEVICE_CAN_BE_DEFAULT
            | SEL_DEVICE_CAN_BE_SYSTEM_DEFAULT
            | SEL_DEVICE_LATENCY
            | SEL_DEVICE_SAFETY_OFFSET
            | SEL_DEVICE_IS_HIDDEN
            | SEL_DEVICE_ZERO_TIMESTAMP_PERIOD => Some(4),
            SEL_DEVICE_RELATED_DEVICES => Some(4),
            SEL_DEVICE_STREAMS => match addr.scope {
                SCOPE_INPUT | SCOPE_OUTPUT => Some(4),
                _ => Some(8),
            },
            SEL_DEVICE_STREAM_CONFIGURATION => Some(size_of::<AudioBufferList1>() as u32),
            SEL_CONTROL_LIST => Some(8),
            SEL_DEVICE_NOMINAL_SAMPLE_RATE => Some(8),
            SEL_DEVICE_AVAILABLE_SAMPLE_RATES => {
                Some((SUPPORTED_RATES.len() * size_of::<AudioValueRange>()) as u32)
            }
            SEL_DEVICE_PREFERRED_CHANNELS_STEREO => Some(8),
            SEL_CUSTOM_PROPERTY_INFO_LIST => Some(0),
            SEL_IDENTIFY => Some(4),
            _ => None,
        },
        OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT => match sel {
            SEL_BASE_CLASS | SEL_CLASS | SEL_OWNER => Some(4),
            SEL_NAME => Some(cf),
            SEL_OWNED_OBJECTS => Some(0),
            SEL_STREAM_IS_ACTIVE
            | SEL_STREAM_DIRECTION
            | SEL_STREAM_TERMINAL_TYPE
            | SEL_STREAM_STARTING_CHANNEL
            | SEL_STREAM_LATENCY => Some(4),
            SEL_STREAM_VIRTUAL_FORMAT | SEL_STREAM_PHYSICAL_FORMAT => {
                Some(size_of::<AudioStreamBasicDescription>() as u32)
            }
            SEL_STREAM_AVAILABLE_VIRTUAL_FORMATS | SEL_STREAM_AVAILABLE_PHYSICAL_FORMATS => {
                Some((SUPPORTED_RATES.len() * size_of::<AudioStreamRangedDescription>()) as u32)
            }
            SEL_CUSTOM_PROPERTY_INFO_LIST => Some(0),
            _ => None,
        },
        OBJ_VOLUME => match sel {
            SEL_BASE_CLASS | SEL_CLASS | SEL_OWNER => Some(4),
            SEL_NAME => Some(cf),
            SEL_OWNED_OBJECTS => Some(0),
            SEL_CONTROL_SCOPE | SEL_CONTROL_ELEMENT => Some(4),
            SEL_LEVEL_SCALAR_VALUE
            | SEL_LEVEL_DECIBEL_VALUE
            | SEL_LEVEL_SCALAR_TO_DECIBELS
            | SEL_LEVEL_DECIBELS_TO_SCALAR => Some(4),
            SEL_LEVEL_DECIBEL_RANGE => Some(size_of::<AudioValueRange>() as u32),
            SEL_CUSTOM_PROPERTY_INFO_LIST => Some(0),
            _ => None,
        },
        OBJ_MUTE => match sel {
            SEL_BASE_CLASS | SEL_CLASS | SEL_OWNER => Some(4),
            SEL_NAME => Some(cf),
            SEL_OWNED_OBJECTS => Some(0),
            SEL_CONTROL_SCOPE | SEL_CONTROL_ELEMENT => Some(4),
            SEL_BOOLEAN_VALUE => Some(4),
            SEL_CUSTOM_PROPERTY_INFO_LIST => Some(0),
            _ => None,
        },
        _ => None,
    }
}

/// Best-effort notification to the HAL that control values changed.
fn notify_properties_changed(object: u32, selectors: &[u32]) {
    let host = STATE.lock().unwrap().host;
    if host == 0 {
        return;
    }
    let host = host as AudioServerPlugInHostRef;
    let addrs: Vec<AudioObjectPropertyAddress> = selectors
        .iter()
        .map(|s| AudioObjectPropertyAddress {
            selector: *s,
            scope: SCOPE_GLOBAL,
            element: 0,
        })
        .collect();
    unsafe {
        ((*host).properties_changed)(host, object, addrs.len() as u32, addrs.as_ptr());
    }
}

// ===========================================================================
// Property dispatch
// ===========================================================================

unsafe extern "C" fn has_property(
    driver: DriverRef,
    object: u32,
    _pid: c_int,
    addr: *const AudioObjectPropertyAddress,
) -> Boolean {
    if driver != our_ref() || addr.is_null() {
        return 0;
    }
    let addr = unsafe { &*addr };
    prop_size(object, addr).is_some() as Boolean
}

unsafe extern "C" fn is_property_settable(
    driver: DriverRef,
    object: u32,
    _pid: c_int,
    addr: *const AudioObjectPropertyAddress,
    out_settable: *mut Boolean,
) -> OSStatus {
    if driver != our_ref() {
        return ERR_BAD_OBJECT;
    }
    if addr.is_null() || out_settable.is_null() {
        return E_POINTER;
    }
    let addr = unsafe { &*addr };
    if prop_size(object, addr).is_none() {
        return ERR_UNKNOWN_PROPERTY;
    }
    let settable = matches!(
        (object, addr.selector),
        (OBJ_DEVICE, SEL_DEVICE_NOMINAL_SAMPLE_RATE)
            | (
                OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT,
                SEL_STREAM_IS_ACTIVE | SEL_STREAM_VIRTUAL_FORMAT | SEL_STREAM_PHYSICAL_FORMAT
            )
            | (OBJ_VOLUME, SEL_LEVEL_SCALAR_VALUE | SEL_LEVEL_DECIBEL_VALUE)
            | (OBJ_MUTE, SEL_BOOLEAN_VALUE)
    );
    unsafe { *out_settable = settable as Boolean };
    NO_ERR
}

unsafe extern "C" fn get_property_data_size(
    driver: DriverRef,
    object: u32,
    _pid: c_int,
    addr: *const AudioObjectPropertyAddress,
    _qualifier_size: u32,
    _qualifier: *const c_void,
    out_size: *mut u32,
) -> OSStatus {
    if driver != our_ref() {
        return ERR_BAD_OBJECT;
    }
    if addr.is_null() || out_size.is_null() {
        return E_POINTER;
    }
    let addr = unsafe { &*addr };
    match prop_size(object, addr) {
        Some(size) => {
            unsafe { *out_size = size };
            NO_ERR
        }
        None => ERR_UNKNOWN_PROPERTY,
    }
}

unsafe extern "C" fn get_property_data(
    driver: DriverRef,
    object: u32,
    _pid: c_int,
    addr: *const AudioObjectPropertyAddress,
    qualifier_size: u32,
    qualifier: *const c_void,
    in_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    if driver != our_ref() {
        return ERR_BAD_OBJECT;
    }
    if addr.is_null() || out_size.is_null() || out_data.is_null() {
        return E_POINTER;
    }
    let addr = unsafe { &*addr };
    let sel = addr.selector;

    unsafe {
        match (object, sel) {
            // -------- shared object properties --------
            (OBJ_PLUGIN, SEL_BASE_CLASS) => out_scalar(CLASS_OBJECT, in_size, out_size, out_data),
            (OBJ_PLUGIN, SEL_CLASS) => out_scalar(CLASS_PLUGIN, in_size, out_size, out_data),
            (OBJ_PLUGIN, SEL_OWNER) => out_scalar(OBJ_UNKNOWN, in_size, out_size, out_data),
            (OBJ_DEVICE, SEL_BASE_CLASS) => out_scalar(CLASS_OBJECT, in_size, out_size, out_data),
            (OBJ_DEVICE, SEL_CLASS) => out_scalar(CLASS_DEVICE, in_size, out_size, out_data),
            (OBJ_DEVICE, SEL_OWNER) => out_scalar(OBJ_PLUGIN, in_size, out_size, out_data),
            (OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT, SEL_BASE_CLASS) => {
                out_scalar(CLASS_OBJECT, in_size, out_size, out_data)
            }
            (OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT, SEL_CLASS) => {
                out_scalar(CLASS_STREAM, in_size, out_size, out_data)
            }
            (OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT, SEL_OWNER) => {
                out_scalar(OBJ_DEVICE, in_size, out_size, out_data)
            }
            (_, SEL_CUSTOM_PROPERTY_INFO_LIST) if prop_size(object, addr).is_some() => {
                *out_size = 0;
                NO_ERR
            }

            // -------- plug-in --------
            (OBJ_PLUGIN, SEL_NAME) => out_cfstring(c"Squeezed", in_size, out_size, out_data),
            (OBJ_PLUGIN, SEL_MANUFACTURER) => {
                out_cfstring(MANUFACTURER, in_size, out_size, out_data)
            }
            (OBJ_PLUGIN, SEL_PLUGIN_RESOURCE_BUNDLE) => {
                out_cfstring(c"", in_size, out_size, out_data)
            }
            (OBJ_PLUGIN, SEL_OWNED_OBJECTS | SEL_PLUGIN_DEVICE_LIST) => {
                out_array(&[OBJ_DEVICE], in_size, out_size, out_data)
            }
            (OBJ_PLUGIN, SEL_PLUGIN_BOX_LIST) => {
                *out_size = 0;
                NO_ERR
            }
            (OBJ_PLUGIN, SEL_PLUGIN_TRANSLATE_UID_TO_BOX) => {
                out_scalar(OBJ_UNKNOWN, in_size, out_size, out_data)
            }
            (OBJ_PLUGIN, SEL_PLUGIN_TRANSLATE_UID_TO_DEVICE) => {
                let mut result = OBJ_UNKNOWN;
                if qualifier_size as usize >= size_of::<CFStringRef>() && !qualifier.is_null() {
                    let requested: CFStringRef =
                        ptr::read_unaligned(qualifier as *const CFStringRef);
                    if !requested.is_null() {
                        let ours = CFStringCreateWithCString(
                            ptr::null(),
                            DEVICE_UID.as_ptr(),
                            CF_STRING_ENCODING_UTF8,
                        );
                        if CFStringCompare(requested, ours, 0) == 0 {
                            result = OBJ_DEVICE;
                        }
                        CFRelease(ours);
                    }
                }
                out_scalar(result, in_size, out_size, out_data)
            }

            // -------- device --------
            (OBJ_DEVICE, SEL_NAME) => out_cfstring(DEVICE_NAME, in_size, out_size, out_data),
            (OBJ_DEVICE, SEL_MANUFACTURER) => {
                out_cfstring(MANUFACTURER, in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_DEVICE_UID) => out_cfstring(DEVICE_UID, in_size, out_size, out_data),
            (OBJ_DEVICE, SEL_DEVICE_MODEL_UID) => {
                out_cfstring(DEVICE_MODEL_UID, in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_OWNED_OBJECTS) => out_array(
                &[OBJ_STREAM_OUTPUT, OBJ_STREAM_INPUT, OBJ_VOLUME, OBJ_MUTE],
                in_size,
                out_size,
                out_data,
            ),
            (OBJ_DEVICE, SEL_DEVICE_TRANSPORT_TYPE) => {
                out_scalar(TRANSPORT_VIRTUAL, in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_DEVICE_RELATED_DEVICES) => {
                out_array(&[OBJ_DEVICE], in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_DEVICE_CLOCK_DOMAIN) => out_scalar(0u32, in_size, out_size, out_data),
            (OBJ_DEVICE, SEL_DEVICE_IS_ALIVE) => out_scalar(1u32, in_size, out_size, out_data),
            (OBJ_DEVICE, SEL_DEVICE_IS_RUNNING) => {
                let running = STATE.lock().unwrap().io_count > 0;
                out_scalar(running as u32, in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_DEVICE_CAN_BE_DEFAULT | SEL_DEVICE_CAN_BE_SYSTEM_DEFAULT) => {
                out_scalar(1u32, in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_DEVICE_LATENCY | SEL_DEVICE_SAFETY_OFFSET) => {
                out_scalar(0u32, in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_DEVICE_STREAMS) => match addr.scope {
                SCOPE_INPUT => out_array(&[OBJ_STREAM_INPUT], in_size, out_size, out_data),
                SCOPE_OUTPUT => out_array(&[OBJ_STREAM_OUTPUT], in_size, out_size, out_data),
                _ => out_array(
                    &[OBJ_STREAM_OUTPUT, OBJ_STREAM_INPUT],
                    in_size,
                    out_size,
                    out_data,
                ),
            },
            (OBJ_DEVICE, SEL_DEVICE_STREAM_CONFIGURATION) => {
                // Our device carries 2 channels in one buffer on both the input
                // and output scope; the global scope reports none.
                let channels = match addr.scope {
                    SCOPE_INPUT | SCOPE_OUTPUT => CHANNELS as u32,
                    _ => 0,
                };
                let need = size_of::<AudioBufferList1>() as u32;
                if in_size < need {
                    return ERR_BAD_PROPERTY_SIZE;
                }
                let list = AudioBufferList1 {
                    number_buffers: if channels > 0 { 1 } else { 0 },
                    buffers: [AudioBuffer {
                        number_channels: channels,
                        data_byte_size: 0,
                        data: ptr::null_mut(),
                    }],
                };
                ptr::write_unaligned(out_data as *mut AudioBufferList1, list);
                *out_size = need;
                NO_ERR
            }
            (OBJ_DEVICE, SEL_CONTROL_LIST) => {
                out_array(&[OBJ_VOLUME, OBJ_MUTE], in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_DEVICE_NOMINAL_SAMPLE_RATE) => {
                let rate = STATE.lock().unwrap().sample_rate;
                out_scalar(rate, in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_DEVICE_AVAILABLE_SAMPLE_RATES) => {
                let ranges: Vec<AudioValueRange> = SUPPORTED_RATES
                    .iter()
                    .map(|r| AudioValueRange {
                        minimum: *r,
                        maximum: *r,
                    })
                    .collect();
                out_array(&ranges, in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_DEVICE_IS_HIDDEN) => out_scalar(0u32, in_size, out_size, out_data),
            (OBJ_DEVICE, SEL_DEVICE_PREFERRED_CHANNELS_STEREO) => {
                out_array(&[1u32, 2u32], in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_DEVICE_ZERO_TIMESTAMP_PERIOD) => {
                out_scalar(RING_FRAMES as u32, in_size, out_size, out_data)
            }
            (OBJ_DEVICE, SEL_IDENTIFY) => out_scalar(0u32, in_size, out_size, out_data),

            // -------- streams --------
            (OBJ_STREAM_OUTPUT, SEL_NAME) => {
                out_cfstring(STREAM_OUT_NAME, in_size, out_size, out_data)
            }
            (OBJ_STREAM_INPUT, SEL_NAME) => {
                out_cfstring(STREAM_IN_NAME, in_size, out_size, out_data)
            }
            (OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT, SEL_OWNED_OBJECTS) => {
                *out_size = 0;
                NO_ERR
            }
            (OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT, SEL_STREAM_IS_ACTIVE) => {
                out_scalar(1u32, in_size, out_size, out_data)
            }
            (OBJ_STREAM_OUTPUT, SEL_STREAM_DIRECTION) => {
                out_scalar(0u32, in_size, out_size, out_data)
            }
            (OBJ_STREAM_INPUT, SEL_STREAM_DIRECTION) => {
                out_scalar(1u32, in_size, out_size, out_data)
            }
            (OBJ_STREAM_OUTPUT, SEL_STREAM_TERMINAL_TYPE) => {
                out_scalar(TERMINAL_SPEAKER, in_size, out_size, out_data)
            }
            (OBJ_STREAM_INPUT, SEL_STREAM_TERMINAL_TYPE) => {
                out_scalar(TERMINAL_MICROPHONE, in_size, out_size, out_data)
            }
            (OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT, SEL_STREAM_STARTING_CHANNEL) => {
                out_scalar(1u32, in_size, out_size, out_data)
            }
            (OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT, SEL_STREAM_LATENCY) => {
                out_scalar(0u32, in_size, out_size, out_data)
            }
            (
                OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT,
                SEL_STREAM_VIRTUAL_FORMAT | SEL_STREAM_PHYSICAL_FORMAT,
            ) => {
                let rate = STATE.lock().unwrap().sample_rate;
                out_scalar(current_asbd(rate), in_size, out_size, out_data)
            }
            (
                OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT,
                SEL_STREAM_AVAILABLE_VIRTUAL_FORMATS | SEL_STREAM_AVAILABLE_PHYSICAL_FORMATS,
            ) => {
                let formats = available_formats(addr.scope);
                out_array(&formats, in_size, out_size, out_data)
            }

            // -------- controls --------
            (OBJ_VOLUME | OBJ_MUTE, SEL_BASE_CLASS) => out_scalar(
                if object == OBJ_VOLUME {
                    CLASS_LEVEL_CONTROL
                } else {
                    CLASS_BOOLEAN_CONTROL
                },
                in_size,
                out_size,
                out_data,
            ),
            (OBJ_VOLUME | OBJ_MUTE, SEL_CLASS) => out_scalar(
                if object == OBJ_VOLUME {
                    CLASS_VOLUME_CONTROL
                } else {
                    CLASS_MUTE_CONTROL
                },
                in_size,
                out_size,
                out_data,
            ),
            (OBJ_VOLUME | OBJ_MUTE, SEL_OWNER) => {
                out_scalar(OBJ_DEVICE, in_size, out_size, out_data)
            }
            (OBJ_VOLUME, SEL_NAME) => out_cfstring(c"Volume", in_size, out_size, out_data),
            (OBJ_MUTE, SEL_NAME) => out_cfstring(c"Mute", in_size, out_size, out_data),
            (OBJ_VOLUME | OBJ_MUTE, SEL_OWNED_OBJECTS) => {
                *out_size = 0;
                NO_ERR
            }
            (OBJ_VOLUME | OBJ_MUTE, SEL_CONTROL_SCOPE) => {
                out_scalar(SCOPE_OUTPUT, in_size, out_size, out_data)
            }
            (OBJ_VOLUME | OBJ_MUTE, SEL_CONTROL_ELEMENT) => {
                out_scalar(0u32, in_size, out_size, out_data)
            }
            (OBJ_VOLUME, SEL_LEVEL_SCALAR_VALUE) => {
                let s = f32::from_bits(VOLUME_BITS.load(Ordering::Relaxed));
                out_scalar(s, in_size, out_size, out_data)
            }
            (OBJ_VOLUME, SEL_LEVEL_DECIBEL_VALUE) => {
                let s = f32::from_bits(VOLUME_BITS.load(Ordering::Relaxed));
                out_scalar(scalar_to_db(s), in_size, out_size, out_data)
            }
            (OBJ_VOLUME, SEL_LEVEL_DECIBEL_RANGE) => out_scalar(
                AudioValueRange {
                    minimum: MIN_DB,
                    maximum: MAX_DB,
                },
                in_size,
                out_size,
                out_data,
            ),
            // In/out translations: the HAL passes a Float32 in and expects the
            // converted Float32 back in the same buffer.
            (OBJ_VOLUME, SEL_LEVEL_SCALAR_TO_DECIBELS) => {
                if in_size < 4 {
                    return ERR_BAD_PROPERTY_SIZE;
                }
                let s = ptr::read_unaligned(out_data as *const f32);
                out_scalar(scalar_to_db(s.clamp(0.0, 1.0)), in_size, out_size, out_data)
            }
            (OBJ_VOLUME, SEL_LEVEL_DECIBELS_TO_SCALAR) => {
                if in_size < 4 {
                    return ERR_BAD_PROPERTY_SIZE;
                }
                let db = ptr::read_unaligned(out_data as *const f32);
                out_scalar(db_to_scalar(db), in_size, out_size, out_data)
            }
            (OBJ_MUTE, SEL_BOOLEAN_VALUE) => {
                out_scalar(MUTED.load(Ordering::Relaxed), in_size, out_size, out_data)
            }

            _ => ERR_UNKNOWN_PROPERTY,
        }
    }
}

unsafe extern "C" fn set_property_data(
    driver: DriverRef,
    object: u32,
    _pid: c_int,
    addr: *const AudioObjectPropertyAddress,
    _qualifier_size: u32,
    _qualifier: *const c_void,
    in_size: u32,
    in_data: *const c_void,
) -> OSStatus {
    if driver != our_ref() {
        return ERR_BAD_OBJECT;
    }
    if addr.is_null() || in_data.is_null() {
        return E_POINTER;
    }
    let addr = unsafe { &*addr };

    match (object, addr.selector) {
        (OBJ_DEVICE, SEL_DEVICE_NOMINAL_SAMPLE_RATE) => {
            if in_size < 8 {
                return ERR_BAD_PROPERTY_SIZE;
            }
            let rate = unsafe { ptr::read_unaligned(in_data as *const f64) };
            request_rate_change(rate)
        }
        (
            OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT,
            SEL_STREAM_VIRTUAL_FORMAT | SEL_STREAM_PHYSICAL_FORMAT,
        ) => {
            if (in_size as usize) < size_of::<AudioStreamBasicDescription>() {
                return ERR_BAD_PROPERTY_SIZE;
            }
            let asbd =
                unsafe { ptr::read_unaligned(in_data as *const AudioStreamBasicDescription) };
            if asbd.format_id != FORMAT_LPCM
                || asbd.channels_per_frame != CHANNELS as u32
                || asbd.bits_per_channel != 32
            {
                return ERR_UNSUPPORTED_FORMAT;
            }
            request_rate_change(asbd.sample_rate)
        }
        (OBJ_STREAM_OUTPUT | OBJ_STREAM_INPUT, SEL_STREAM_IS_ACTIVE) => NO_ERR,
        (OBJ_VOLUME, SEL_LEVEL_SCALAR_VALUE) => {
            if in_size < 4 {
                return ERR_BAD_PROPERTY_SIZE;
            }
            let s = unsafe { ptr::read_unaligned(in_data as *const f32) }.clamp(0.0, 1.0);
            VOLUME_BITS.store(s.to_bits(), Ordering::Relaxed);
            notify_properties_changed(
                OBJ_VOLUME,
                &[SEL_LEVEL_SCALAR_VALUE, SEL_LEVEL_DECIBEL_VALUE],
            );
            NO_ERR
        }
        (OBJ_VOLUME, SEL_LEVEL_DECIBEL_VALUE) => {
            if in_size < 4 {
                return ERR_BAD_PROPERTY_SIZE;
            }
            let db = unsafe { ptr::read_unaligned(in_data as *const f32) };
            VOLUME_BITS.store(db_to_scalar(db).to_bits(), Ordering::Relaxed);
            notify_properties_changed(
                OBJ_VOLUME,
                &[SEL_LEVEL_SCALAR_VALUE, SEL_LEVEL_DECIBEL_VALUE],
            );
            NO_ERR
        }
        (OBJ_MUTE, SEL_BOOLEAN_VALUE) => {
            if in_size < 4 {
                return ERR_BAD_PROPERTY_SIZE;
            }
            let v = unsafe { ptr::read_unaligned(in_data as *const u32) };
            MUTED.store((v != 0) as u32, Ordering::Relaxed);
            notify_properties_changed(OBJ_MUTE, &[SEL_BOOLEAN_VALUE]);
            NO_ERR
        }
        _ => {
            if prop_size(object, addr).is_some() {
                ERR_ILLEGAL_OPERATION
            } else {
                ERR_UNKNOWN_PROPERTY
            }
        }
    }
}

/// Ask the host to schedule a sample-rate change; the new rate is delivered
/// back to us in PerformDeviceConfigurationChange as the change action.
fn request_rate_change(rate: f64) -> OSStatus {
    if !SUPPORTED_RATES.contains(&rate) {
        return ERR_UNSUPPORTED_FORMAT;
    }
    let (host, current) = {
        let state = STATE.lock().unwrap();
        (state.host, state.sample_rate)
    };
    if rate == current {
        return NO_ERR;
    }
    if host == 0 {
        return ERR_ILLEGAL_OPERATION;
    }
    let host = host as AudioServerPlugInHostRef;
    unsafe {
        ((*host).request_device_configuration_change)(
            host,
            OBJ_DEVICE,
            rate as u64,
            ptr::null_mut(),
        )
    }
}

// ===========================================================================
// IO
// ===========================================================================

unsafe extern "C" fn start_io(driver: DriverRef, device: u32, _client: u32) -> OSStatus {
    if driver != our_ref() || device != OBJ_DEVICE {
        return ERR_BAD_OBJECT;
    }
    let mut state = STATE.lock().unwrap();
    if state.io_count == 0 {
        unsafe {
            ptr::write_bytes(ring_base(), 0, RING_FRAMES * CHANNELS);
        }
        state.anchor_host_time = unsafe { mach_absolute_time() };
        state.timestamp_seed += 1;
        log_msg(c"IO started");
    }
    state.io_count += 1;
    NO_ERR
}

unsafe extern "C" fn stop_io(driver: DriverRef, device: u32, _client: u32) -> OSStatus {
    if driver != our_ref() || device != OBJ_DEVICE {
        return ERR_BAD_OBJECT;
    }
    let mut state = STATE.lock().unwrap();
    state.io_count = state.io_count.saturating_sub(1);
    if state.io_count == 0 {
        log_msg(c"IO stopped");
    }
    NO_ERR
}

unsafe extern "C" fn get_zero_time_stamp(
    driver: DriverRef,
    device: u32,
    _client: u32,
    out_sample_time: *mut f64,
    out_host_time: *mut u64,
    out_seed: *mut u64,
) -> OSStatus {
    if driver != our_ref() || device != OBJ_DEVICE {
        return ERR_BAD_OBJECT;
    }
    if out_sample_time.is_null() || out_host_time.is_null() || out_seed.is_null() {
        return E_POINTER;
    }
    let state = STATE.lock().unwrap();
    let ticks_per_period = host_ticks_per_second() / state.sample_rate * RING_FRAMES as f64;
    let now = unsafe { mach_absolute_time() };
    let elapsed = now.saturating_sub(state.anchor_host_time) as f64;
    let periods = (elapsed / ticks_per_period).floor();
    unsafe {
        *out_sample_time = periods * RING_FRAMES as f64;
        *out_host_time = state.anchor_host_time + (periods * ticks_per_period) as u64;
        *out_seed = state.timestamp_seed;
    }
    NO_ERR
}

unsafe extern "C" fn will_do_io_operation(
    driver: DriverRef,
    device: u32,
    _client: u32,
    operation: u32,
    out_will_do: *mut Boolean,
    out_will_do_in_place: *mut Boolean,
) -> OSStatus {
    if driver != our_ref() || device != OBJ_DEVICE {
        return ERR_BAD_OBJECT;
    }
    // Opt into the input read and every output-delivery operation. macOS starts
    // the output IOProc only if we accept its output pipeline ops (ProcessOutput
    // / 'rite'), not just WriteMix — declining them leaves output IO unstarted.
    let will_do = matches!(
        operation,
        OP_READ_INPUT | OP_WRITE_MIX | OP_PROCESS_OUTPUT | OP_RITE
    );
    unsafe {
        if !out_will_do.is_null() {
            *out_will_do = will_do as Boolean;
        }
        if !out_will_do_in_place.is_null() {
            *out_will_do_in_place = 1;
        }
    }
    NO_ERR
}

unsafe extern "C" fn begin_io_operation(
    _driver: DriverRef,
    _device: u32,
    _client: u32,
    _operation: u32,
    _frames: u32,
    _cycle_info: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    NO_ERR
}

unsafe extern "C" fn do_io_operation(
    driver: DriverRef,
    device: u32,
    _stream: u32,
    _client: u32,
    operation: u32,
    frames: u32,
    cycle_info: *const AudioServerPlugInIOCycleInfo,
    main_buffer: *mut c_void,
    _secondary_buffer: *mut c_void,
) -> OSStatus {
    if driver != our_ref() || device != OBJ_DEVICE {
        return ERR_BAD_OBJECT;
    }
    if cycle_info.is_null() || main_buffer.is_null() {
        return E_POINTER;
    }
    let cycle = unsafe { &*cycle_info };
    let buffer = main_buffer as *mut f32;
    let frames = frames as usize;
    let ring = ring_base();

    match operation {
        // Post-mix system audio destined for "hardware" — apply volume/mute and
        // copy into the ring. This macOS delivers it via 'rite'; other configs
        // use WriteMix. Both carry the full mix of every app.
        OP_WRITE_MIX | OP_RITE => {
            let gain = if MUTED.load(Ordering::Relaxed) != 0 {
                0.0
            } else {
                let s = f32::from_bits(VOLUME_BITS.load(Ordering::Relaxed));
                s * s * s
            };
            let start =
                (cycle.output_time.sample_time as i64).rem_euclid(RING_FRAMES as i64) as usize;
            for f in 0..frames {
                let rf = ((start + f) % RING_FRAMES) * CHANNELS;
                unsafe {
                    for c in 0..CHANNELS {
                        *ring.add(rf + c) = *buffer.add(f * CHANNELS + c) * gain;
                    }
                }
            }
            NO_ERR
        }
        // Per-stream pre-mix pass. We must accept it for the HAL to run output
        // IO, but we don't write it to the ring — that would drop other apps'
        // audio when several play at once, since the mix arrives via WriteMix/'rite'.
        OP_PROCESS_OUTPUT => NO_ERR,
        // Capture side — hand back what the output wrote, then clear it so a
        // stopped output yields silence instead of a stale loop.
        OP_READ_INPUT => {
            let start =
                (cycle.input_time.sample_time as i64).rem_euclid(RING_FRAMES as i64) as usize;
            for f in 0..frames {
                let rf = ((start + f) % RING_FRAMES) * CHANNELS;
                unsafe {
                    for c in 0..CHANNELS {
                        *buffer.add(f * CHANNELS + c) = *ring.add(rf + c);
                        *ring.add(rf + c) = 0.0;
                    }
                }
            }
            NO_ERR
        }
        _ => NO_ERR,
    }
}

unsafe extern "C" fn end_io_operation(
    _driver: DriverRef,
    _device: u32,
    _client: u32,
    _operation: u32,
    _frames: u32,
    _cycle_info: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    NO_ERR
}
