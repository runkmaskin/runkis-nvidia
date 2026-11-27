//! NVIDIA DLSS Super Resolution through NVIDIA Streamline
//! (`VICTORY_DLSS=1`, off by default). See `docs/dlss.md`.
//!
//! None of NVIDIA's files are in this repository. `scripts\get-dlss.ps1`
//! fetches the Streamline release this file was written against (2.14.16,
//! DLSS 310.9) from NVIDIA's public GitHub into `%LOCALAPPDATA%\victory\dlss`
//! (or `VICTORY_DLSS_DIR`). The structures below are this layer's own
//! declarations of Streamline's C ABI (its headers are MIT-licensed); the
//! tests check their sizes and offsets against the headers' layout.
//!
//! Streamline is used with "manual hooking": the host device is created as
//! before and handed over with `slSetD3DDevice`, so nothing of D3D12 goes
//! through Streamline's proxies. That is enough for DLSS Super Resolution,
//! which is one `slEvaluateFeature` recorded into a command list. Frame
//! generation needs more (the swap chain made through Streamline's proxy
//! factory, its present; see `docs/dlss.md`).

use std::ffi::{c_char, c_void};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Whether DLSS is asked for (`VICTORY_DLSS=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_DLSS"))
}

/// Where Streamline's files are: `VICTORY_DLSS_DIR`, else
/// `%LOCALAPPDATA%\victory\dlss`.
pub(crate) fn dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("VICTORY_DLSS_DIR") {
        return Some(d.into());
    }
    std::env::var_os("LOCALAPPDATA").map(|l| std::path::Path::new(&l).join("victory").join("dlss"))
}

/// The model asked for with `VICTORY_DLSS_PRESET` ([`preset`]), read once.
pub(crate) fn preset_set() -> u32 {
    static SET: OnceLock<u32> = OnceLock::new();
    *SET.get_or_init(|| preset(std::env::var("VICTORY_DLSS_PRESET").ok().as_deref()))
}

/// Where the title's pass that DLSS takes its inputs from keeps them: root
/// parameter and place in its table, in dwords (as `VICTORY_TAAU_REPORT`
/// prints them), and the constant buffer word that holds the jitter.
pub(crate) struct Map {
    /// The pass's shader, by the hash the log names it with.
    pub(crate) hash: &'static str,
    pub(crate) colour: (u8, u32),
    pub(crate) depth: (u8, u32),
    pub(crate) motion: (u8, u32),
    pub(crate) output: (u8, u32),
    /// Constant buffer (root parameter, place in its table) and the index of
    /// the jitter's first float in it.
    pub(crate) jitter: ((u8, u32), usize),
    /// Render pixels per unit of the jitter words, per axis ([`jitter_pixels`]).
    pub(crate) jitter_scale: [f32; 2],
    /// What turns the motion vectors into a fraction of the screen, per
    /// render pixel (0: they are a fraction already).
    pub(crate) motion_per_pixel: bool,
    pub(crate) depth_inverted: bool,
}

/// The title's TAA (`TAA2:CS_TAA2_Resolve`), found with `VICTORY_TAAU_REPORT`
/// (docs/dlss.md, step 1). It runs at render size; its result (table place
/// 160) is what the title's post-processing reads, and a separate whole-screen
/// draw scales the finished picture to the output size afterwards. DLSS in
/// its place runs as DLAA (render size in and out) until the title is made to
/// use its real temporal upscaler.
/// - colour: the lit scene, RGBA16F, HDR (written by `CS_ApplyCombinedDeferred`)
/// - depth: D32S8, reversed (the projection's far plane is at 0)
/// - motion: R16G16F, in render pixels (captured while the camera moved:
///   values of -1.8 to 2.2 a frame, which as a fraction of the screen would be
///   two screens; -10000 marks texels without a vector)
/// - the jitter: words 24 and 25 of the frame's constant buffer (table place
///   168) are the projection's third row, x and y: the jitter in clip space,
///   so a pixel's offset is -x times half the width and y times half the
///   height (clip y points up, pixels down; the sign is checked in play).
pub(crate) const PASSES: [Map; 2] = [
    Map {
        hash: "df11dcd012cd7382",
        colour: (1, 56),
        depth: (1, 32),
        motion: (1, 80),
        output: (1, 160),
        jitter: ((1, 168), 24),
        jitter_scale: [-0.5, 0.5],
        motion_per_pixel: true,
        depth_inverted: true,
    },
    // The title's temporal upscaler, `TAA2U:CS_TAA2_Resolve`, which it runs
    // with `VICTORY_OWN_TAAU=1`: the same source compiled to write its result
    // at the output size, with the same table layout (read from the shader
    // offline). DLSS then upscales: render size in, output size out.
    Map {
        hash: "22f4971293b8fdcc",
        colour: (1, 56),
        depth: (1, 32),
        motion: (1, 80),
        output: (1, 160),
        jitter: ((1, 168), 24),
        jitter_scale: [-0.5, 0.5],
        motion_per_pixel: true,
        depth_inverted: true,
    },
];

/// The jitter in render pixels from the words the title keeps it in.
/// `VICTORY_DLSS_JITTER_SCALE=<x>,<y>` replaces the map's scale (to try the
/// other sign in play).
pub(crate) fn jitter_pixels(words: [f32; 2], scale: [f32; 2], render: (u32, u32)) -> [f32; 2] {
    static SET: OnceLock<Option<[f32; 2]>> = OnceLock::new();
    let set = *SET.get_or_init(|| pair(&std::env::var("VICTORY_DLSS_JITTER_SCALE").unwrap_or_default()));
    let s = set.unwrap_or(scale);
    [words[0] * s[0] * render.0 as f32, words[1] * s[1] * render.1 as f32]
}

/// `VICTORY_DLSS_MOTION_SCALE=<x>,<y>`: the motion scale DLSS is given, in
/// place of the map's (to try pixels or a sign in play).
pub(crate) fn motion_scale_set() -> Option<[f32; 2]> {
    static SET: OnceLock<Option<[f32; 2]>> = OnceLock::new();
    *SET.get_or_init(|| pair(&std::env::var("VICTORY_DLSS_MOTION_SCALE").unwrap_or_default()))
}

fn pair(text: &str) -> Option<[f32; 2]> {
    let (a, b) = text.split_once(',')?;
    Some([a.trim().parse().ok()?, b.trim().parse().ok()?])
}
/// Two floats of title memory at `at` (the title's GPU addresses are CPU
/// addresses), if that memory can be read.
pub(crate) fn two_floats(at: usize) -> Option<[f32; 2]> {
    if at == 0 || !readable(at, 8) {
        return None;
    }
    // SAFETY: committed, readable memory of the process, checked above.
    let words = unsafe { std::ptr::read_unaligned(at as *const [u32; 2]) };
    Some(words.map(f32::from_bits))
}

/// Whether `len` bytes at `at` are committed and readable.
pub(crate) fn readable(at: usize, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_READONLY,
        PAGE_READWRITE, PAGE_WRITECOPY, VirtualQuery,
    };
    let mut from = at;
    while from < at.saturating_add(len) {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(from as *const _, &mut info, size_of_val(&info)) };
        let readable = PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE;
        if n == 0 || info.State != MEM_COMMIT || info.Protect & readable == 0 {
            return false;
        }
        from = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// The Streamline SDK version these declarations match (`sl_version.h`):
/// major, minor and patch, then the SDK's magic number.
const SDK_VERSION: u64 = (2 << 48) | (14 << 32) | (16 << 16) | 0xfedc;

const FEATURE_DLSS: u32 = 0;
const FEATURE_REFLEX: u32 = 3;
const FEATURE_PCL: u32 = 4;
const FEATURE_DLSS_G: u32 = 1000;

/// `VICTORY_DLSS_REFLEX=1` (with `VICTORY_DLSS=1`): NVIDIA Reflex in low
/// latency mode, and the PC latency markers of each frame ([`marker`]).
pub(crate) fn reflex_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| on() && crate::switch::on("VICTORY_DLSS_REFLEX"))
}

/// `VICTORY_DLSS_FG=1` (with `VICTORY_DLSS=1`): DLSS Frame Generation is
/// loaded and whether it can run is logged. It needs hardware-accelerated GPU
/// scheduling; its per-frame inputs are the plan in `docs/dlss.md`.
pub(crate) fn frame_generation_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| on() && crate::switch::on("VICTORY_DLSS_FG"))
}

/// `sl::ReflexOptions`, version 1.
#[repr(C)]
struct ReflexOptions {
    base: Base,
    mode: u32,
    frame_limit_us: u32,
    use_markers_to_optimize: bool,
    virtual_key: u16,
    id_thread: u32,
}

const REFLEX_OPTIONS: Guid = Guid(0xf03a_f81a, 0x6d0b, 0x4902, [0xa6, 0x51, 0xc4, 0x96, 0x5e, 0x21, 0x54, 0x34]);
/// `sl::ReflexMode::eLowLatency`.
const REFLEX_LOW_LATENCY: u32 = 1;

/// The PC latency markers (`sl::PCLMarker`) the layer sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Marker {
    /// The title begins a frame (its frame-origin wait ends).
    SimulationStart = 0,
    /// The title presents the frame's planes.
    SimulationEnd = 1,
    RenderSubmitStart = 2,
    RenderSubmitEnd = 3,
    /// Around the host's `Present`.
    PresentStart = 4,
    PresentEnd = 5,
}

/// Turns Reflex on and finds the marker function; `None` (logged) if
/// either is refused.
fn reflex(function_of: &dyn Fn(u32, &std::ffi::CStr) -> Result<*mut c_void, String>) -> Option<SetMarker> {
    let set = match function_of(FEATURE_REFLEX, c"slReflexSetOptions") {
        // SAFETY: the function `sl_reflex.h` declares under this name.
        Ok(f) => unsafe { std::mem::transmute::<*mut c_void, SetReflex>(f) },
        Err(why) => {
            tracing::warn!("Reflex not available: {why}");
            return None;
        }
    };
    let options = ReflexOptions {
        base: Base::new(REFLEX_OPTIONS, 1),
        mode: REFLEX_LOW_LATENCY,
        frame_limit_us: 0,
        use_markers_to_optimize: false,
        virtual_key: 0,
        id_thread: 0,
    };
    // SAFETY: valid options living through the call.
    let r = unsafe { set(&raw const options) };
    if r != 0 {
        tracing::warn!("Reflex not turned on: {}", result_text(r));
        return None;
    }
    match function_of(FEATURE_PCL, c"slPCLSetMarker") {
        Ok(f) => {
            tracing::info!("Reflex: low latency mode on, PC latency markers sent each frame");
            // SAFETY: the function `sl_pcl.h` declares under this name.
            Some(unsafe { std::mem::transmute::<*mut c_void, SetMarker>(f) })
        }
        Err(why) => {
            tracing::warn!("Reflex markers not available: {why}");
            None
        }
    }
}

/// The Streamline attached to the host device, if any.
fn attached() -> Option<&'static Dlss> {
    ATTACHED.get()?.as_ref().ok()
}

/// Sends latency marker `m` for the title's frame `frame` (Reflex on only).
pub(crate) fn marker(m: Marker, frame: u64) {
    if !reflex_on() {
        return;
    }
    let Some(d) = attached() else { return };
    let Some(set) = d.markers else { return };
    let index = frame as u32;
    let mut token = std::ptr::null_mut();
    // SAFETY: an out pointer and a frame index of ours; the token is
    // Streamline's and lives in its ring of frames.
    unsafe {
        if (d.new_token)(&raw mut token, &raw const index) == 0 && !token.is_null() {
            let r = set(m as u32, token);
            if r != 0 {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| tracing::warn!(marker = ?m, "Reflex marker refused: {}", result_text(r)));
            }
        }
    }
}

/// `factory` as Streamline's proxy (`slUpgradeInterface`), when DLSS is on
/// and attached: a swap chain made with it presents through Streamline, which
/// runs its per-frame bookkeeping there (without it Streamline logs that
/// `presentCommon()` was not observed) and which frame generation needs. The
/// factory as it was otherwise.
pub(crate) fn proxy_factory(factory: windows::Win32::Graphics::Dxgi::IDXGIFactory2) -> windows::Win32::Graphics::Dxgi::IDXGIFactory2 {
    let Some(d) = attached() else { return factory };
    let mut raw = factory.into_raw();
    // SAFETY: a live factory whose reference we hand over; Streamline puts
    // its proxy (holding the factory) in its place, or leaves it.
    unsafe {
        let r = (d.upgrade)(&raw mut raw);
        if r == 0 {
            tracing::info!("DLSS: the swap chain is made through Streamline's factory");
        } else {
            tracing::warn!("DLSS: Streamline's factory refused ({}); the swap chain is made as before", result_text(r));
        }
        windows::Win32::Graphics::Dxgi::IDXGIFactory2::from_raw(raw)
    }
}

/// A structure's type id (a GUID) and version, which Streamline reads
/// first in every structure it is given.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Guid(u32, u16, u16, [u8; 8]);

#[repr(C)]
struct Base {
    next: *mut c_void,
    kind: Guid,
    version: usize,
}

impl Base {
    const fn new(kind: Guid, version: usize) -> Base {
        Base { next: std::ptr::null_mut(), kind, version }
    }
}

const PREFERENCES: Guid = Guid(0x1ca1_0965, 0xbf8e, 0x432b, [0x8d, 0xa1, 0x67, 0x16, 0xd8, 0x79, 0xfb, 0x14]);
const VIEWPORT: Guid = Guid(0x171b_6435, 0x9b3c, 0x4fc8, [0x99, 0x94, 0xfb, 0xe5, 0x25, 0x69, 0xaa, 0xa4]);
const RESOURCE: Guid = Guid(0x3a9d_70cf, 0x2418, 0x4b72, [0x83, 0x91, 0x13, 0xf8, 0x72, 0x1c, 0x72, 0x61]);
const RESOURCE_TAG: Guid = Guid(0x4c6a_5aad, 0xb445, 0x496c, [0x87, 0xff, 0x1a, 0xf3, 0x84, 0x5b, 0xe6, 0x53]);
const CONSTANTS: Guid = Guid(0xdcd3_5ad7, 0x4e4a, 0x4bad, [0xa9, 0x0c, 0xe0, 0xc4, 0x9e, 0xb2, 0x3a, 0xfe]);
const ADAPTER_INFO: Guid = Guid(0x0677_315f, 0xa746, 0x4492, [0x9f, 0x42, 0xcb, 0x61, 0x42, 0xc9, 0xc3, 0xd4]);
const DLSS_OPTIONS: Guid = Guid(0x6ac8_26e4, 0x4c61, 0x4101, [0xa9, 0x2d, 0x63, 0x8d, 0x42, 0x10, 0x57, 0xb8]);
const DLSS_OPTIMAL: Guid = Guid(0xef1d_0957, 0xfd58, 0x4df7, [0xb5, 0x04, 0x8b, 0x69, 0xd8, 0xaa, 0x6b, 0x76]);

/// `sl::Preferences`, version 1.
#[repr(C)]
struct Preferences {
    base: Base,
    show_console: bool,
    log_level: u32,
    paths_to_plugins: *const *const u16,
    num_paths_to_plugins: u32,
    path_to_logs_and_data: *const u16,
    allocate_callback: *const c_void,
    release_callback: *const c_void,
    log_message_callback: Option<extern "C" fn(u32, *const c_char)>,
    flags: u64,
    features_to_load: *const u32,
    num_features_to_load: u32,
    application_id: u32,
    engine: u32,
    engine_version: *const c_char,
    project_id: *const c_char,
    render_api: u32,
}

/// Preference flags: the host restores its command list after an
/// evaluation (this layer rebinds everything anyway), and nothing is hooked.
const DISABLE_CL_STATE_TRACKING: u64 = 1 << 0;
const USE_MANUAL_HOOKING: u64 = 1 << 2;

/// `sl::ViewportHandle`.
#[repr(C)]
struct Viewport {
    base: Base,
    value: u32,
}

/// `sl::Resource`, version 1: a D3D12 resource and the state it is in when
/// the evaluation runs (with manual hooking Streamline cannot track it).
#[repr(C)]
struct Resource {
    base: Base,
    kind: u8,
    native: *mut c_void,
    memory: *mut c_void,
    view: *mut c_void,
    state: u32,
    width: u32,
    height: u32,
    native_format: u32,
    mip_levels: u32,
    array_layers: u32,
    gpu_virtual_address: u64,
    flags: u32,
    usage: u32,
    internal_flags: u16,
    reserved: u16,
}

impl Resource {
    fn texture(resource: &ID3D12Resource, state: u32) -> Resource {
        Resource {
            base: Base::new(RESOURCE, 1),
            kind: 0,
            native: resource.as_raw(),
            memory: std::ptr::null_mut(),
            view: std::ptr::null_mut(),
            state,
            width: 0,
            height: 0,
            native_format: 0,
            mip_levels: 0,
            array_layers: 0,
            gpu_virtual_address: 0,
            flags: 0,
            usage: 0,
            internal_flags: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Extent {
    top: u32,
    left: u32,
    width: u32,
    height: u32,
}

/// `sl::ResourceTag`: what a resource is for this evaluation.
#[repr(C)]
struct Tag {
    base: Base,
    resource: *mut Resource,
    kind: u32,
    lifecycle: u32,
    extent: Extent,
}

const BUFFER_DEPTH: u32 = 0;
const BUFFER_MOTION: u32 = 1;
const BUFFER_INPUT_COLOUR: u32 = 3;
const BUFFER_OUTPUT_COLOUR: u32 = 4;
/// Valid only while the call it is passed to runs.
const ONLY_VALID_NOW: u32 = 0;

/// `sl::Constants`, version 2. Row-major matrices without the jitter.
#[repr(C)]
#[derive(Clone, Copy)]
struct Constants {
    base_next: *mut c_void,
    base_kind: Guid,
    base_version: usize,
    camera_view_to_clip: [[f32; 4]; 4],
    clip_to_camera_view: [[f32; 4]; 4],
    clip_to_lens_clip: [[f32; 4]; 4],
    clip_to_prev_clip: [[f32; 4]; 4],
    prev_clip_to_clip: [[f32; 4]; 4],
    jitter_offset: [f32; 2],
    mvec_scale: [f32; 2],
    camera_pinhole_offset: [f32; 2],
    camera_pos: [f32; 3],
    camera_up: [f32; 3],
    camera_right: [f32; 3],
    camera_fwd: [f32; 3],
    camera_near: f32,
    camera_far: f32,
    camera_fov: f32,
    camera_aspect_ratio: f32,
    motion_vectors_invalid_value: f32,
    depth_inverted: u8,
    camera_motion_included: u8,
    motion_vectors_3d: u8,
    reset: u8,
    orthographic_projection: u8,
    motion_vectors_dilated: u8,
    motion_vectors_jittered: u8,
    min_relative_linear_depth_object_separation: f32,
}

const IDENTITY: [[f32; 4]; 4] = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
/// Streamline's "no value" for a float.
const INVALID_FLOAT: f32 = f32::MAX;
const FALSE: u8 = 0;
const TRUE: u8 = 1;

/// `sl::AdapterInfo`.
#[repr(C)]
struct AdapterInfo {
    base: Base,
    luid: *const u8,
    luid_bytes: u32,
    vk_physical_device: *mut c_void,
}

/// `sl::DLSSOptions`, version 3.
#[repr(C)]
#[derive(Clone, Copy)]
struct Options {
    base_next: *mut c_void,
    base_kind: Guid,
    base_version: usize,
    mode: u32,
    output_width: u32,
    output_height: u32,
    sharpness: f32,
    pre_exposure: f32,
    exposure_scale: f32,
    colour_buffers_hdr: u8,
    indicator_invert_x: u8,
    indicator_invert_y: u8,
    dlaa_preset: u32,
    quality_preset: u32,
    balanced_preset: u32,
    performance_preset: u32,
    ultra_performance_preset: u32,
    ultra_quality_preset: u32,
    use_auto_exposure: u8,
    alpha_upscaling_enabled: u8,
}

/// `sl::DLSSOptimalSettings`.
#[repr(C)]
struct Optimal {
    base: Base,
    render_width: u32,
    render_height: u32,
    sharpness: f32,
    render_width_min: u32,
    render_height_min: u32,
    render_width_max: u32,
    render_height_max: u32,
}

/// DLSS's quality modes (`sl::DLSSMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Performance = 1,
    Balanced = 2,
    Quality = 3,
    UltraPerformance = 4,
    Dlaa = 6,
}

/// The mode whose render size is nearest to output over `ratio`: the title
/// picks its render size (`VICTORY_RENDER_SCALE`, a fixed render height),
/// DLSS takes what it is given within its mode's range.
pub(crate) fn mode_for(ratio: f32) -> Mode {
    if ratio >= 2.6 {
        Mode::UltraPerformance
    } else if ratio >= 1.85 {
        Mode::Performance
    } else if ratio >= 1.6 {
        Mode::Balanced
    } else if ratio >= 1.2 {
        Mode::Quality
    } else {
        Mode::Dlaa
    }
}

/// The DLSS model asked for with `VICTORY_DLSS_PRESET=<letter>` (J, K, L or
/// M: the transformer models; DLSS 4 is K, DLSS 4.5 is L and M), else 0:
/// the driver's default for the mode, which for DLSS 310.9 is a transformer
/// model too.
pub(crate) fn preset(text: Option<&str>) -> u32 {
    match text.map(|t| t.trim().to_ascii_uppercase()).as_deref() {
        Some("J") => 10,
        Some("K") => 11,
        Some("L") => 12,
        Some("M") => 13,
        _ => 0,
    }
}

/// What `GetProcAddress` gives.
type Proc = unsafe extern "system" fn() -> isize;
type Init = unsafe extern "C" fn(*const Preferences, u64) -> i32;
type SetDevice = unsafe extern "C" fn(*mut c_void) -> i32;
type IsSupported = unsafe extern "C" fn(u32, *const AdapterInfo) -> i32;
type GetFunction = unsafe extern "C" fn(u32, *const c_char, *mut *mut c_void) -> i32;
type NewToken = unsafe extern "C" fn(*mut *mut c_void, *const u32) -> i32;
type SetConstants = unsafe extern "C" fn(*const Constants, *const c_void, *const Viewport) -> i32;
type Evaluate = unsafe extern "C" fn(u32, *const c_void, *const *const Base, u32, *mut c_void) -> i32;
type SetOptions = unsafe extern "C" fn(*const Viewport, *const Options) -> i32;
type GetOptimal = unsafe extern "C" fn(*const Options, *mut Optimal) -> i32;
type Upgrade = unsafe extern "C" fn(*mut *mut c_void) -> i32;
type SetMarker = unsafe extern "C" fn(u32, *const c_void) -> i32;
type SetReflex = unsafe extern "C" fn(*const ReflexOptions) -> i32;

/// Streamline, loaded and attached to one device.
pub(crate) struct Dlss {
    device: usize,
    new_token: NewToken,
    set_constants: SetConstants,
    evaluate: Evaluate,
    set_options: SetOptions,
    get_optimal: GetOptimal,
    upgrade: Upgrade,
    /// PC latency markers (`slPCLSetMarker`), with `VICTORY_DLSS_REFLEX=1`.
    markers: Option<SetMarker>,
    /// The options last set, to set them again only when they change.
    options: Mutex<Option<(u32, u32, u32, u32, u8)>>,
}

// SAFETY: plain function pointers and a device address; Streamline's
// calls used here are thread safe or made under `options`' lock.
unsafe impl Send for Dlss {}
// SAFETY: as above.
unsafe impl Sync for Dlss {}

/// Streamline's result codes that say what to do, by number (`sl_result.h`).
pub(crate) fn result_text(code: i32) -> String {
    let name = match code {
        0 => "ok",
        1 => "file or folder not found (eErrorIO)",
        2 => "the driver is too old",
        3 => "Windows is too old",
        4 => "hardware GPU scheduling is off",
        5 => "no device (eErrorDeviceNotCreated)",
        6 => "no supported adapter: DLSS needs an NVIDIA RTX card",
        7 => "this adapter is not supported",
        8 => "no plugins found",
        15 => "NGX failed (the DLSS library did not load or start)",
        19 => "invalid integration",
        20 => "an input is missing",
        21 => "not initialised",
        22 => "the evaluation failed",
        25 => "invalid parameter",
        26 => "constants are missing",
        32 => "the feature is not supported",
        34 => "the feature failed to load",
        _ => "",
    };
    format!("{code} {name}")
}

/// The process's one Streamline: made at the first [`attach`], with that
/// call's device, or the reason it could not be.
static ATTACHED: OnceLock<Result<Dlss, String>> = OnceLock::new();

/// Loads Streamline from [`dir`], starts it and gives it `device`; checks
/// that DLSS runs on the device's adapter. Once per process: later calls
/// get the first call's result, and fail for another device (Streamline
/// works with one).
pub(crate) fn attach(device: &ID3D12Device) -> Result<&'static Dlss, String> {
    let made = ATTACHED.get_or_init(|| {
        let made = start(device);
        match &made {
            Ok(_) => tracing::info!("DLSS: Streamline started; DLSS Super Resolution is supported on this card"),
            Err(why) => tracing::warn!("DLSS not available: {why}"),
        }
        made
    });
    match made {
        Ok(d) if d.device == device.as_raw() as usize => Ok(d),
        Ok(_) => Err("Streamline is attached to another device".to_owned()),
        Err(why) => Err(why.clone()),
    }
}

fn wide(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// Whether `path` carries a valid Authenticode signature (NVIDIA signs
/// every Streamline and DLSS file; a file that is not signed is not
/// loaded into the title's process).
fn signed(path: &std::path::Path) -> bool {
    use windows_sys::Win32::Security::WinTrust::*;
    let name = wide(path);
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: name.as_ptr(),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    data.Anonymous.pFile = &raw mut file;
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: the structures and the path live through both calls; the
    // second releases what the first kept.
    unsafe {
        let verdict = WinVerifyTrust(std::ptr::null_mut(), &raw mut action, (&raw mut data).cast());
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        WinVerifyTrust(std::ptr::null_mut(), &raw mut action, (&raw mut data).cast());
        verdict == 0
    }
}

extern "C" fn log_message(kind: u32, text: *const c_char) {
    if text.is_null() {
        return;
    }
    // SAFETY: Streamline passes a NUL-terminated message.
    let text = unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy();
    let text = text.trim_end();
    match kind {
        2 => tracing::warn!("Streamline error: {text}"),
        1 => tracing::warn!("Streamline: {text}"),
        _ => tracing::debug!("Streamline: {text}"),
    }
}

fn start(device: &ID3D12Device) -> Result<Dlss, String> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW};
    let dir = dir().ok_or("no folder for Streamline (LOCALAPPDATA is not set)")?;
    let interposer = dir.join("sl.interposer.dll");
    if !interposer.exists() {
        return Err(format!(
            "{} not found: run scripts\\get-dlss.ps1 (or set VICTORY_DLSS_DIR)",
            interposer.display()
        ));
    }
    if !signed(&interposer) {
        return Err(format!("{} has no valid signature: not loaded", interposer.display()));
    }
    let name = wide(&interposer);
    // SAFETY: a signed library of NVIDIA's, by full path; it stays loaded
    // for the life of the process (its functions are kept).
    let module = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
    if module.is_null() {
        return Err(format!("{} did not load", interposer.display()));
    }
    let find = |f: &[u8]| {
        // SAFETY: a loaded module and a NUL-terminated name.
        unsafe { GetProcAddress(module, f.as_ptr()) }.ok_or_else(|| format!("Streamline has no {}", String::from_utf8_lossy(&f[..f.len() - 1])))
    };
    // SAFETY: each name's function has the type declared for it above
    // (`sl_core_api.h`).
    let (init, set_device, supported, get_function, new_token, set_constants, evaluate, upgrade) = unsafe {
        (
            std::mem::transmute::<Proc, Init>(find(b"slInit\0")?),
            std::mem::transmute::<Proc, SetDevice>(find(b"slSetD3DDevice\0")?),
            std::mem::transmute::<Proc, IsSupported>(find(b"slIsFeatureSupported\0")?),
            std::mem::transmute::<Proc, GetFunction>(find(b"slGetFeatureFunction\0")?),
            std::mem::transmute::<Proc, NewToken>(find(b"slGetNewFrameToken\0")?),
            std::mem::transmute::<Proc, SetConstants>(find(b"slSetConstants\0")?),
            std::mem::transmute::<Proc, Evaluate>(find(b"slEvaluateFeature\0")?),
            std::mem::transmute::<Proc, Upgrade>(find(b"slUpgradeInterface\0")?),
        )
    };
    let plugins = wide(&dir);
    let paths = [plugins.as_ptr()];
    // Reflex and its latency markers, and frame generation, only when asked.
    let mut features = vec![FEATURE_DLSS];
    if reflex_on() {
        features.extend([FEATURE_REFLEX, FEATURE_PCL]);
    }
    if frame_generation_on() {
        features.push(FEATURE_DLSS_G);
    }
    // Logs and NGX's data in the layer's own folder, outside the repository.
    let logs = std::env::var_os("LOCALAPPDATA").map(|l| wide(&std::path::Path::new(&l).join("victory").join("dlss-logs")));
    if let Some(l) = &logs {
        let _ = std::fs::create_dir_all(String::from_utf16_lossy(&l[..l.len() - 1]));
    }
    let preferences = Preferences {
        base: Base::new(PREFERENCES, 1),
        show_console: false,
        log_level: 1,
        paths_to_plugins: paths.as_ptr(),
        num_paths_to_plugins: 1,
        path_to_logs_and_data: logs.as_ref().map_or(std::ptr::null(), |l| l.as_ptr()),
        allocate_callback: std::ptr::null(),
        release_callback: std::ptr::null(),
        log_message_callback: Some(log_message),
        // No updates over the air: nothing is downloaded while the title runs.
        flags: DISABLE_CL_STATE_TRACKING | USE_MANUAL_HOOKING,
        features_to_load: features.as_ptr(),
        num_features_to_load: features.len() as u32,
        application_id: 0,
        engine: 0,
        engine_version: c"victory".as_ptr(),
        project_id: c"5b1b6f3e-6c2a-4d8e-9a57-0d1c2e3f4a5b".as_ptr(),
        render_api: 1,
    };
    // SAFETY: valid preferences whose pointers live through the call
    // (Streamline keeps a copy).
    let r = unsafe { init(&raw const preferences, SDK_VERSION) };
    if r != 0 {
        return Err(format!("slInit: {}", result_text(r)));
    }
    // SAFETY: a live device; Streamline holds a reference of its own.
    let r = unsafe { set_device(device.as_raw()) };
    if r != 0 {
        return Err(format!("slSetD3DDevice: {}", result_text(r)));
    }
    // SAFETY: a plain query of a live device.
    let luid = unsafe { device.GetAdapterLuid() };
    let info = AdapterInfo {
        base: Base::new(ADAPTER_INFO, 1),
        luid: (&raw const luid).cast(),
        luid_bytes: size_of_val(&luid) as u32,
        vk_physical_device: std::ptr::null_mut(),
    };
    // SAFETY: valid structures living through the call.
    let r = unsafe { supported(FEATURE_DLSS, &raw const info) };
    if r != 0 {
        return Err(format!("DLSS on this adapter: {}", result_text(r)));
    }
    let function_of = |feature: u32, name: &std::ffi::CStr| -> Result<*mut c_void, String> {
        let mut f = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name and an out pointer of ours.
        let r = unsafe { get_function(feature, name.as_ptr(), &raw mut f) };
        if r != 0 || f.is_null() {
            return Err(format!("{}: {}", name.to_string_lossy(), result_text(r)));
        }
        Ok(f)
    };
    let function = |name: &std::ffi::CStr| function_of(FEATURE_DLSS, name);
    // SAFETY: the functions `sl_dlss.h` declares under these names.
    let (set_options, get_optimal) = unsafe {
        (
            std::mem::transmute::<*mut c_void, SetOptions>(function(c"slDLSSSetOptions")?),
            std::mem::transmute::<*mut c_void, GetOptimal>(function(c"slDLSSGetOptimalSettings")?),
        )
    };
    let markers = reflex_on().then(|| reflex(&function_of)).flatten();
    if frame_generation_on() {
        // SAFETY: valid structures living through the call.
        let r = unsafe { supported(FEATURE_DLSS_G, &raw const info) };
        if r == 0 {
            tracing::info!("DLSS Frame Generation: supported on this card and system (not wired into the present yet; docs/dlss.md)");
        } else {
            tracing::warn!("DLSS Frame Generation not available: {} (it needs hardware-accelerated GPU scheduling on, an RTX 40 or 50 card and a recent driver)", result_text(r));
        }
    }
    Ok(Dlss {
        device: device.as_raw() as usize,
        new_token,
        set_constants,
        evaluate,
        set_options,
        get_optimal,
        upgrade,
        markers,
        options: Mutex::new(None),
    })
}

/// A texture and the host state it is in when the evaluation runs.
pub(crate) struct Image<'a> {
    pub(crate) resource: &'a ID3D12Resource,
    pub(crate) state: D3D12_RESOURCE_STATES,
}

/// One frame's evaluation.
pub(crate) struct Frame<'a> {
    pub(crate) colour: Image<'a>,
    pub(crate) depth: Image<'a>,
    pub(crate) motion: Image<'a>,
    /// Written whole at output size; must allow unordered access.
    pub(crate) output: Image<'a>,
    /// The part of the inputs that holds the picture.
    pub(crate) render: (u32, u32),
    pub(crate) out: (u32, u32),
    /// The sub-pixel offset the frame was drawn with, in render pixels.
    pub(crate) jitter: [f32; 2],
    /// What turns the motion vectors into a fraction of the screen
    /// (1 / render size for vectors in pixels; 1 for vectors in [-1, 1]).
    pub(crate) motion_scale: [f32; 2],
    pub(crate) depth_inverted: bool,
    /// The colour is light (HDR), not 0..1.
    pub(crate) hdr: bool,
    /// The last frames do not belong to this one (a cut).
    pub(crate) reset: bool,
    pub(crate) index: u32,
    pub(crate) preset: u32,
}

impl Dlss {
    /// The render size DLSS likes for `mode` at output size `out`, and the
    /// smallest and largest it takes.
    pub(crate) fn optimal(&self, mode: Mode, out: (u32, u32)) -> Result<[(u32, u32); 3], String> {
        let options = options(mode as u32, out, false, 0);
        let mut o = Optimal {
            base: Base::new(DLSS_OPTIMAL, 1),
            render_width: 0,
            render_height: 0,
            sharpness: 0.0,
            render_width_min: 0,
            render_height_min: 0,
            render_width_max: 0,
            render_height_max: 0,
        };
        // SAFETY: valid structures living through the call.
        let r = unsafe { (self.get_optimal)(&raw const options, &raw mut o) };
        if r != 0 {
            return Err(format!("slDLSSGetOptimalSettings: {}", result_text(r)));
        }
        Ok([
            (o.render_width, o.render_height),
            (o.render_width_min, o.render_height_min),
            (o.render_width_max, o.render_height_max),
        ])
    }

    /// Records DLSS Super Resolution of `frame` into `list`. The list's
    /// pipeline, root signature, descriptor heaps and root arguments are
    /// left as DLSS set them: the caller sets its own again.
    pub(crate) fn evaluate(&self, list: &ID3D12GraphicsCommandList, frame: &Frame) -> Result<(), String> {
        let mode = mode_for(frame.out.0 as f32 / frame.render.0.max(1) as f32) as u32;
        let viewport = Viewport { base: Base::new(VIEWPORT, 1), value: 0 };
        {
            let key = (mode, frame.out.0, frame.out.1, frame.preset, u8::from(frame.hdr));
            let mut set = self.options.lock().unwrap_or_else(|e| e.into_inner());
            if *set != Some(key) {
                let o = options(mode, frame.out, frame.hdr, frame.preset);
                // SAFETY: valid structures living through the call.
                let r = unsafe { (self.set_options)(&raw const viewport, &raw const o) };
                if r != 0 {
                    return Err(format!("slDLSSSetOptions: {}", result_text(r)));
                }
                *set = Some(key);
            }
        }
        let mut token = std::ptr::null_mut();
        // SAFETY: an out pointer and a frame index of ours.
        let r = unsafe { (self.new_token)(&raw mut token, &raw const frame.index) };
        if r != 0 || token.is_null() {
            return Err(format!("slGetNewFrameToken: {}", result_text(r)));
        }
        let constants = constants(frame);
        // SAFETY: valid structures; the token is Streamline's.
        let r = unsafe { (self.set_constants)(&raw const constants, token, &raw const viewport) };
        if r != 0 {
            return Err(format!("slSetConstants: {}", result_text(r)));
        }
        let state = |i: &Image| i.state.0 as u32;
        let mut resources = [
            Resource::texture(frame.colour.resource, state(&frame.colour)),
            Resource::texture(frame.depth.resource, state(&frame.depth)),
            Resource::texture(frame.motion.resource, state(&frame.motion)),
            Resource::texture(frame.output.resource, state(&frame.output)),
        ];
        let render = Extent { top: 0, left: 0, width: frame.render.0, height: frame.render.1 };
        let out = Extent { top: 0, left: 0, width: frame.out.0, height: frame.out.1 };
        let [c, d, m, o] = resources.each_mut().map(std::ptr::from_mut);
        let tags = [
            tag(c, BUFFER_INPUT_COLOUR, render),
            tag(d, BUFFER_DEPTH, render),
            tag(m, BUFFER_MOTION, render),
            tag(o, BUFFER_OUTPUT_COLOUR, out),
        ];
        let inputs: [*const Base; 5] = [
            (&raw const viewport).cast(),
            (&raw const tags[0]).cast(),
            (&raw const tags[1]).cast(),
            (&raw const tags[2]).cast(),
            (&raw const tags[3]).cast(),
        ];
        // SAFETY: an open list of the attached device; the structures and
        // resources live through the call (the tags are valid only now).
        let r = unsafe { (self.evaluate)(FEATURE_DLSS, token, inputs.as_ptr(), inputs.len() as u32, list.as_raw()) };
        if r != 0 {
            return Err(format!("slEvaluateFeature: {}", result_text(r)));
        }
        Ok(())
    }
}

fn tag(resource: *mut Resource, kind: u32, extent: Extent) -> Tag {
    Tag { base: Base::new(RESOURCE_TAG, 1), resource, kind, lifecycle: ONLY_VALID_NOW, extent }
}

fn options(mode: u32, (w, h): (u32, u32), hdr: bool, preset: u32) -> Options {
    Options {
        base_next: std::ptr::null_mut(),
        base_kind: DLSS_OPTIONS,
        base_version: 3,
        mode,
        output_width: w,
        output_height: h,
        sharpness: 0.0,
        pre_exposure: 1.0,
        exposure_scale: 1.0,
        colour_buffers_hdr: u8::from(hdr),
        indicator_invert_x: FALSE,
        indicator_invert_y: FALSE,
        dlaa_preset: preset,
        quality_preset: preset,
        balanced_preset: preset,
        performance_preset: preset,
        ultra_performance_preset: preset,
        ultra_quality_preset: preset,
        // The title's exposure texture is not passed (yet): DLSS works it out.
        use_auto_exposure: TRUE,
        alpha_upscaling_enabled: FALSE,
    }
}

/// The constants DLSS Super Resolution reads. Its camera matrices are for
/// frame generation and the 3D motion it does not use here: identity.
fn constants(frame: &Frame) -> Constants {
    Constants {
        base_next: std::ptr::null_mut(),
        base_kind: CONSTANTS,
        base_version: 2,
        camera_view_to_clip: IDENTITY,
        clip_to_camera_view: IDENTITY,
        clip_to_lens_clip: IDENTITY,
        clip_to_prev_clip: IDENTITY,
        prev_clip_to_clip: IDENTITY,
        jitter_offset: frame.jitter,
        mvec_scale: frame.motion_scale,
        camera_pinhole_offset: [0.0, 0.0],
        camera_pos: [0.0; 3],
        camera_up: [0.0, 1.0, 0.0],
        camera_right: [1.0, 0.0, 0.0],
        camera_fwd: [0.0, 0.0, 1.0],
        camera_near: 0.1,
        camera_far: 10000.0,
        camera_fov: 1.0,
        camera_aspect_ratio: frame.out.0 as f32 / frame.out.1.max(1) as f32,
        motion_vectors_invalid_value: INVALID_FLOAT,
        depth_inverted: u8::from(frame.depth_inverted),
        camera_motion_included: TRUE,
        motion_vectors_3d: FALSE,
        reset: u8::from(frame.reset),
        orthographic_projection: FALSE,
        motion_vectors_dilated: FALSE,
        motion_vectors_jittered: FALSE,
        min_relative_linear_depth_object_separation: 40.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

    /// The sizes and offsets of Streamline 2.14.16's structures on x64, as
    /// its headers lay them out (`sl_core_types.h`, `sl_consts.h`,
    /// `sl_dlss.h`).
    #[test]
    fn structures_have_streamlines_layout() {
        assert_eq!(size_of::<Base>(), 32);
        assert_eq!(size_of::<Preferences>(), 144);
        assert_eq!(offset_of!(Preferences, log_level), 36);
        assert_eq!(offset_of!(Preferences, flags), 88);
        assert_eq!(offset_of!(Preferences, engine), 112);
        assert_eq!(offset_of!(Preferences, render_api), 136);
        assert_eq!(size_of::<Viewport>(), 40);
        assert_eq!(size_of::<Resource>(), 112);
        assert_eq!(offset_of!(Resource, native), 40);
        assert_eq!(offset_of!(Resource, state), 64);
        assert_eq!(offset_of!(Resource, gpu_virtual_address), 88);
        assert_eq!(size_of::<Tag>(), 64);
        assert_eq!(offset_of!(Tag, extent), 48);
        assert_eq!(size_of::<Constants>(), 456);
        assert_eq!(offset_of!(Constants, jitter_offset), 352);
        assert_eq!(offset_of!(Constants, camera_near), 424);
        assert_eq!(offset_of!(Constants, depth_inverted), 444);
        assert_eq!(offset_of!(Constants, min_relative_linear_depth_object_separation), 452);
        assert_eq!(size_of::<AdapterInfo>(), 56);
        assert_eq!(size_of::<Options>(), 88);
        assert_eq!(offset_of!(Options, colour_buffers_hdr), 56);
        assert_eq!(offset_of!(Options, dlaa_preset), 60);
        assert_eq!(offset_of!(Options, use_auto_exposure), 84);
        assert_eq!(size_of::<Optimal>(), 64);
        assert_eq!(size_of::<ReflexOptions>(), 48);
        assert_eq!(offset_of!(ReflexOptions, use_markers_to_optimize), 40);
        assert_eq!(offset_of!(ReflexOptions, id_thread), 44);
        assert_eq!(SDK_VERSION, 0x0002_000e_0010_fedc);
    }

    #[test]
    fn modes_follow_the_scale() {
        assert_eq!(mode_for(2.0), Mode::Performance, "half: 540p to 1080p");
        assert_eq!(mode_for(1.5), Mode::Quality, "720p to 1080p");
        assert_eq!(mode_for(1.724), Mode::Balanced);
        assert_eq!(mode_for(3.0), Mode::UltraPerformance);
        assert_eq!(mode_for(1.0), Mode::Dlaa);
    }

    #[test]
    fn jitter_words_become_pixels() {
        // Clip-space x of -0.001021 at 960 wide is +0.49 pixels; y of
        // -0.000517 at 540 high is -0.14 pixels (clip y points up).
        let [x, y] = jitter_pixels([-0.001021, -0.000517], [-0.5, 0.5], (960, 540));
        assert!((x - 0.49).abs() < 1e-3 && (y + 0.1396).abs() < 1e-3, "{x} {y}");
        assert_eq!(pair(" 1 , -0.5"), Some([1.0, -0.5]));
        assert_eq!(pair("1"), None);
    }

    #[test]
    fn presets_by_letter() {
        assert_eq!(preset(None), 0);
        assert_eq!(preset(Some("k")), 11);
        assert_eq!(preset(Some(" M ")), 13);
        assert_eq!(preset(Some("Z")), 0);
    }

    #[test]
    fn result_codes_are_named() {
        assert_eq!(result_text(0), "0 ok");
        assert!(result_text(6).contains("RTX"));
        assert_eq!(result_text(99), "99 ");
    }

    fn half(h: u16) -> f32 {
        let (sign, exp, man) = (h >> 15, i32::from((h >> 10) & 0x1f), f32::from(h & 0x3ff));
        let v = match exp {
            0 => man * 2f32.powi(-24),
            31 => f32::INFINITY,
            e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
        };
        if sign == 1 { -v } else { v }
    }

    fn to_half(f: f32) -> u16 {
        // Normal values in 0..2 only (the test's colours).
        if f == 0.0 {
            return 0;
        }
        let bits = f.to_bits();
        let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
        ((exp as u16) << 10) | ((bits >> 13) & 0x3ff) as u16
    }

    /// The card with the most memory of its own.
    fn device() -> Option<ID3D12Device> {
        // SAFETY: plain DXGI queries and device creation.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let mut best: Option<(usize, IDXGIAdapter1)> = None;
            for i in 0.. {
                let Ok(adapter) = factory.EnumAdapters1(i) else { break };
                let Ok(desc) = adapter.GetDesc1() else { continue };
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 && best.as_ref().is_none_or(|b| desc.DedicatedVideoMemory > b.0) {
                    best = Some((desc.DedicatedVideoMemory, adapter));
                }
            }
            let adapter: IDXGIAdapter = best?.1.cast().ok()?;
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    fn texture(device: &ID3D12Device, (w, h): (u32, u32), format: DXGI_FORMAT, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(w),
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: flags,
            ..Default::default()
        };
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out) }.unwrap();
        out.unwrap()
    }

    fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, size: u64, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out) }.unwrap();
        out.unwrap()
    }

    fn located(resource: &ID3D12Resource, footprint: Option<(DXGI_FORMAT, u32, u32, u32)>) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: the same pointer, not counted; used while it lives.
            pResource: unsafe { std::mem::transmute_copy(resource) },
            Type: if footprint.is_some() { D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT } else { D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX },
            Anonymous: match footprint {
                Some((format, w, h, pitch)) => D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: format, Width: w, Height: h, Depth: 1, RowPitch: pitch },
                    },
                },
                None => D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            },
        }
    }

    struct Rig {
        device: ID3D12Device,
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        done: u64,
    }

    impl Rig {
        fn new(device: ID3D12Device) -> Rig {
            // SAFETY: plain creation of objects of our own.
            unsafe {
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                    .unwrap();
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
                list.Close().unwrap();
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
                Rig { device, queue, allocator, list, fence, done: 0 }
            }
        }

        fn begin(&self) {
            // SAFETY: the list's last run was waited for.
            unsafe {
                self.allocator.Reset().unwrap();
                self.list.Reset(&self.allocator, None).unwrap();
            }
        }

        fn run(&mut self) {
            self.done += 1;
            // SAFETY: our closed list; our fence and event.
            unsafe {
                self.list.Close().unwrap();
                self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
                self.queue.Signal(&self.fence, self.done).unwrap();
                let event = CreateEventW(None, false, false, None).unwrap();
                self.fence.SetEventOnCompletion(self.done, event).unwrap();
                WaitForSingleObject(event, INFINITE);
                let _ = CloseHandle(event);
            }
            // SAFETY: a plain query.
            assert!(unsafe { self.device.GetDeviceRemovedReason() }.is_ok(), "the device is lost");
        }

        /// A texture of `w` x `h` texels of `bytes_per` bytes each from
        /// `data`, left in `state`.
        fn upload(&mut self, (w, h): (u32, u32), format: DXGI_FORMAT, bytes_per: u32, data: &[u8], state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
            let pitch = (w * bytes_per).next_multiple_of(256);
            let upload = buffer(&self.device, D3D12_HEAP_TYPE_UPLOAD, u64::from(pitch * h), D3D12_RESOURCE_STATE_GENERIC_READ);
            // SAFETY: a mapped upload buffer of that size.
            unsafe {
                let mut at = std::ptr::null_mut();
                upload.Map(0, None, Some(&mut at)).unwrap();
                let row = (w * bytes_per) as usize;
                for y in 0..h as usize {
                    std::ptr::copy_nonoverlapping(data[y * row..].as_ptr(), at.cast::<u8>().add(y * pitch as usize), row);
                }
                upload.Unmap(0, None);
            }
            let tex = texture(&self.device, (w, h), format, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST);
            self.begin();
            // SAFETY: an open list and live resources.
            unsafe {
                self.list.CopyTextureRegion(&located(&tex, None), 0, 0, 0, &located(&upload, Some((format, w, h, pitch))), None);
                self.list.ResourceBarrier(&[crate::host::transition(&tex, D3D12_RESOURCE_STATE_COPY_DEST, state)]);
            }
            self.run();
            tex
        }

        /// An RGBA16F texture read back as floats; it is in `state`.
        fn read(&mut self, tex: &ID3D12Resource, (w, h): (u32, u32), state: D3D12_RESOURCE_STATES) -> Vec<[f32; 4]> {
            let pitch = (w * 8).next_multiple_of(256);
            let read = buffer(&self.device, D3D12_HEAP_TYPE_READBACK, u64::from(pitch * h), D3D12_RESOURCE_STATE_COPY_DEST);
            self.begin();
            // SAFETY: an open list and live resources.
            unsafe {
                self.list.ResourceBarrier(&[crate::host::transition(tex, state, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                self.list.CopyTextureRegion(&located(&read, Some((DXGI_FORMAT_R16G16B16A16_FLOAT, w, h, pitch))), 0, 0, 0, &located(tex, None), None);
                self.list.ResourceBarrier(&[crate::host::transition(tex, D3D12_RESOURCE_STATE_COPY_SOURCE, state)]);
            }
            self.run();
            let mut out = Vec::with_capacity((w * h) as usize);
            // SAFETY: a mapped readback buffer of that size, written by the run waited for.
            unsafe {
                let mut at = std::ptr::null_mut();
                read.Map(0, None, Some(&mut at)).unwrap();
                for y in 0..h as usize {
                    let row = std::slice::from_raw_parts(at.cast::<u16>().byte_add(y * pitch as usize), w as usize * 4);
                    out.extend(row.chunks_exact(4).map(|c| [half(c[0]), half(c[1]), half(c[2]), half(c[3])]));
                }
                read.Unmap(0, None);
            }
            out
        }
    }

    const A: [f32; 3] = [0.8, 0.2, 0.35];
    const B: [f32; 3] = [0.1, 0.6, 0.85];

    /// DLSS Super Resolution on the card, through Streamline: a still frame
    /// of two flat halves at 960x540 (depth one plane, no motion), upscaled
    /// to 1920x1080 eight times with jitter, comes out as the same two
    /// flat halves. Skipped (with a message) without Streamline's files or
    /// an NVIDIA RTX card.
    #[test]
    fn gpu_dlss_upscales_a_synthetic_frame() {
        let Some(device) = device() else {
            eprintln!("no D3D12 device: skipped");
            return;
        };
        let dlss = match attach(&device) {
            Ok(d) => d,
            Err(why) => {
                eprintln!("DLSS not available, skipped: {why}");
                return;
            }
        };
        let (render, out) = ((960u32, 540u32), (1920u32, 1080u32));
        let [optimal, least, most] = dlss.optimal(Mode::Performance, out).expect("optimal settings");
        eprintln!("DLSS Performance at 1920x1080: render {optimal:?}, from {least:?} to {most:?}");
        assert!(least.0 <= render.0 && render.0 <= most.0, "960 wide is in Performance's range");
        let mut rig = Rig::new(device);
        let read_state = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        let mut colour = Vec::with_capacity((render.0 * render.1 * 8) as usize);
        for _y in 0..render.1 {
            for x in 0..render.0 {
                let c = if x < render.0 / 2 { A } else { B };
                for v in [c[0], c[1], c[2], 1.0] {
                    colour.extend(to_half(v).to_le_bytes());
                }
            }
        }
        let colour = rig.upload(render, DXGI_FORMAT_R16G16B16A16_FLOAT, 8, &colour, read_state);
        let depth: Vec<u8> = (0..render.0 * render.1).flat_map(|_| 0.5f32.to_le_bytes()).collect();
        let depth = rig.upload(render, DXGI_FORMAT_R32_FLOAT, 4, &depth, read_state);
        let motion = rig.upload(render, DXGI_FORMAT_R16G16_FLOAT, 4, &vec![0u8; (render.0 * render.1 * 4) as usize], read_state);
        let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let output = texture(&rig.device, out, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, uav);
        // Halton (2, 3) jitter in render pixels, centred.
        let halton = |mut i: u32, b: u32| {
            let (mut f, mut r) = (1.0f32, 0.0f32);
            while i > 0 {
                f /= b as f32;
                r += f * (i % b) as f32;
                i /= b;
            }
            r - 0.5
        };
        let started = std::time::Instant::now();
        for i in 0..8u32 {
            rig.begin();
            let frame = Frame {
                colour: Image { resource: &colour, state: read_state },
                depth: Image { resource: &depth, state: read_state },
                motion: Image { resource: &motion, state: read_state },
                output: Image { resource: &output, state: uav },
                render,
                out,
                jitter: [halton(i + 1, 2), halton(i + 1, 3)],
                motion_scale: [1.0 / render.0 as f32, 1.0 / render.1 as f32],
                depth_inverted: false,
                hdr: false,
                reset: i == 0,
                index: i,
                preset: 0,
            };
            dlss.evaluate(&rig.list, &frame).expect("evaluated");
            rig.run();
        }
        eprintln!("8 DLSS frames recorded and run in {:?} (the first makes DLSS's own resources)", started.elapsed());
        let pixels = rig.read(&output, out, uav);
        let mut worst = 0.0f32;
        for y in (16..out.1 - 16).step_by(7) {
            for x in (16..out.0 - 16).step_by(7) {
                if x.abs_diff(out.0 / 2) < 48 {
                    continue;
                }
                let want = if x < out.0 / 2 { A } else { B };
                let p = pixels[(y * out.0 + x) as usize];
                for c in 0..3 {
                    worst = worst.max((p[c] - want[c]).abs());
                }
            }
        }
        eprintln!("largest difference from the flat colours: {worst:.4}");
        assert!(worst < 0.03, "DLSS kept flat areas flat: worst difference {worst}");
        // Streamline's factory proxy (what the swap chain is made with) still
        // works as a factory.
        // SAFETY: plain DXGI calls.
        let proxy = proxy_factory(unsafe { CreateDXGIFactory2::<IDXGIFactory2>(DXGI_CREATE_FACTORY_FLAGS(0)) }.unwrap());
        // SAFETY: a plain query.
        assert!(unsafe { proxy.EnumAdapters(0) }.is_ok(), "the proxy factory lists adapters");
    }
}
