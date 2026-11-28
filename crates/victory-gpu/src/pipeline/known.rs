//! Every pipeline the title has ever created, kept on disk
//! (`VICTORY_PIPELINE_BOOK=1`), and their host pipelines made ahead of a run
//! (`victory warm`), so that the driver's own cache on disk has them before
//! a draw asks for one.
//!
//! The title creates 84,000 pipelines while it loads and draws with some
//! 2,000 of them; which ones is known from earlier runs (`library`), and
//! those are made during loading. One met for the first time (a place never
//! seen, or everything after a change of the translator or of the driver) is
//! compiled when a draw wants it. Making all of them in the running title
//! (`VICTORY_EARLY_ALL=1`) ended it inside the driver's compiler; made here,
//! a compiler that crashes or hangs ends a child process, costs that pipeline
//! alone and is named in `warm-bad.txt`.
//!
//! Two files beside the library, both only ever added to (a torn end is cut
//! off): `known-shaders.bin` (stage, hash, length, container) and
//! `known-pipelines.bin` (length, then the key, the shaders' stages and
//! hashes, and the graphics state). They hold the title's own code: they stay
//! in the cache folder.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

const SHADERS: &str = "known-shaders.bin";
const PIPELINES: &str = "known-pipelines.bin";
/// Stages by their number in the files.
const STAGES: [&str; 8] =
    ["vertex", "pixel", "domain", "hull", "geometry", "compute", "amplification", "mesh"];

/// `VICTORY_PIPELINE_BOOK=1`: the pipelines the title creates are kept.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_BOOK").is_some_and(|v| v == "1"))
}

struct Book {
    dir: PathBuf,
    keys: HashSet<[u8; 16]>,
    shaders: HashSet<[u8; 32]>,
    /// Not yet written: shaders, then pipelines.
    new: [Vec<u8>; 2],
    added: usize,
}

static BOOK: OnceLock<Option<Mutex<Book>>> = OnceLock::new();

fn book() -> Option<&'static Mutex<Book>> {
    BOOK.get_or_init(|| {
        let dir = super::library::folder().filter(|_| on())?;
        std::fs::create_dir_all(&dir).ok()?;
        let mut keys = HashSet::new();
        let mut shaders = HashSet::new();
        let ends = [
            walk_shaders(&dir, |_, hash, _, _| {
                shaders.insert(hash);
            }),
            walk_pipelines(&dir, |_, bytes| {
                keys.extend(bytes.first_chunk::<16>().copied());
            }),
        ];
        for (file, end) in [SHADERS, PIPELINES].iter().zip(ends) {
            let path = dir.join(file);
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > end)
                && let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path)
            {
                let _ = f.set_len(end);
            }
        }
        tracing::info!(pipelines = keys.len(), shaders = shaders.len(), "title pipelines known from earlier runs");
        let spawned = crate::named::spawn("victory-known", || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                flush();
            }
        });
        spawned.ok()?;
        if warm_in_play() {
            let at = dir.clone();
            if let Err(e) = crate::named::spawn("victory-warm-play", move || play_warm(&at)) {
                tracing::warn!(error = %e, "VICTORY_WARM_IN_PLAY: no warm-up beside the run");
            }
        }
        Some(Mutex::new(Book { dir, keys, shaders, new: Default::default(), added: 0 }))
    })
    .as_ref()
}

/// The title created a pipeline.
pub(super) fn note(key: &str, shaders: &[Arc<Shader>], graphics: Option<&GraphicsState>) {
    let Some(book) = book() else { return };
    let mut id = [0u8; 16];
    for (at, b) in id.iter_mut().enumerate() {
        *b = key.get(2 * at..2 * at + 2).and_then(|x| u8::from_str_radix(x, 16).ok()).unwrap_or(0);
    }
    if warm_in_play() && !shaders.is_empty() {
        let mut created = CREATED.lock().unwrap_or_else(|e| e.into_inner());
        let created = created.get_or_insert_default();
        if created.0.insert(id) {
            created.1.push(id);
        }
    }
    let mut book = book.lock().unwrap_or_else(|e| e.into_inner());
    if shaders.is_empty() || !book.keys.insert(id) {
        return;
    }
    let mut entry = id.to_vec();
    entry.push(shaders.len() as u8);
    for s in shaders {
        let hash = *blake3::hash(&s.container).as_bytes();
        let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u8;
        entry.push(stage);
        entry.extend(hash);
        if book.shaders.insert(hash) {
            let new = &mut book.new[0];
            new.push(stage);
            new.extend(hash);
            new.extend((s.container.len() as u32).to_le_bytes());
            new.extend(&s.container);
        }
    }
    entry.push(u8::from(graphics.is_some()));
    if let Some(g) = graphics {
        g.put(&mut entry);
    }
    book.new[1].extend((entry.len() as u32).to_le_bytes());
    book.new[1].extend(entry);
    book.added += 1;
}

/// `VICTORY_WARM_IN_PLAY=1` (with the book on): while the title runs,
/// `victory warm` builds the recorded pipelines not yet in the driver's
/// cache beside it ([`play_warm`]), those the title created in this run
/// first, the latest first: they are of the place being played, and the
/// likeliest to be drawn with next. Its processes run at idle priority, on
/// the cores the run leaves free, and only while the run's own pipeline
/// workers have nothing waiting. Off by default: a run is CPU-bound, and
/// what it costs in frames is not measured enough yet (docs/changes-2026-11-27.md).
fn warm_in_play() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WARM_IN_PLAY").is_some_and(|v| v == "1"))
}

/// With [`warm_in_play`]: the keys of the pipelines this run created, in
/// the order it created them.
#[allow(clippy::type_complexity)]
static CREATED: Mutex<Option<(HashSet<[u8; 16]>, Vec<[u8; 16]>)>> = Mutex::new(None);

/// The file of the keys [`play_warm`] has made first.
const PLAY_FIRST: &str = "play-created.txt";

/// The warm-up beside the run ([`warm_in_play`]): a minute after the book
/// opens (past the start's own compiles), batches of `victory warm`, each
/// told the run's pipelines first. The processes are in a job that ends
/// them with the run.
fn play_warm(dir: &Path) {
    use std::io::BufRead;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::time::Duration;
    const START_AFTER: Duration = Duration::from_secs(60);
    const BATCH: usize = 200;
    const IDLE_PRIORITY_CLASS: u32 = 0x40;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let jobs = std::env::var("VICTORY_WARM_IN_PLAY_JOBS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(2).max(1);
    std::thread::sleep(START_AFTER);
    let (Some(job), Ok(exe)) = (kill_on_close_job(), std::env::current_exe()) else {
        tracing::warn!("VICTORY_WARM_IN_PLAY: no job for the warm-up's processes; none is started");
        return;
    };
    let first = dir.join(PLAY_FIRST);
    loop {
        // The run's own pipeline workers have work: theirs comes first.
        if super::make::waiting() > 0 {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        let keys: Vec<String> = {
            let created = CREATED.lock().unwrap_or_else(|e| e.into_inner());
            created
                .iter()
                .flat_map(|c| c.1.iter().rev())
                .map(|id| id.iter().map(|b| format!("{b:02x}")).collect())
                .collect()
        };
        if std::fs::write(&first, keys.join("\n")).is_err() {
            return;
        }
        let began = std::time::Instant::now();
        let child = std::process::Command::new(&exe)
            .arg("warm")
            .arg("--first")
            .arg(&first)
            .args(["--limit", &BATCH.to_string(), "--jobs", &jobs.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .creation_flags(IDLE_PRIORITY_CLASS | CREATE_NO_WINDOW)
            .spawn();
        let Ok(mut child) = child else { return };
        // SAFETY: a job of ours and the process just started.
        unsafe { windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job, child.as_raw_handle()) };
        let (mut nothing, mut said) = (false, String::new());
        if let Some(out) = child.stdout.take() {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                nothing |= line.starts_with("nothing to make");
                if line.contains(" made in ") {
                    said = line;
                }
            }
        }
        let _ = child.wait();
        tracing::info!(
            seconds = began.elapsed().as_secs(),
            first = keys.len(),
            said = %said,
            "VICTORY_WARM_IN_PLAY: pipelines built beside the run"
        );
        if nothing {
            // All made: again once the run has created more.
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

/// A job whose processes end when its last handle closes: when the run ends,
/// however it ends.
fn kill_on_close_job() -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::System::JobObjects::*;
    // SAFETY: plain calls; the limit structure is of the size given.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        (set != 0).then_some(job)
    }
}

/// Writes what was noted.
fn flush() {
    let Some(Some(book)) = BOOK.get() else { return };
    let (dir, new, added) = {
        let mut book = book.lock().unwrap_or_else(|e| e.into_inner());
        if book.added == 0 {
            return;
        }
        (book.dir.clone(), std::mem::take(&mut book.new), std::mem::take(&mut book.added))
    };
    for (file, bytes) in [SHADERS, PIPELINES].iter().zip(&new) {
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(file))
            .and_then(|mut f| f.write_all(bytes));
        if let Err(e) = written {
            tracing::warn!(error = %e, file, "title pipelines not written down");
            return;
        }
    }
    tracing::info!(pipelines = added, "title pipelines written down");
}

/// Calls `f` with each container's stage, hash, place and length; where the
/// last whole one ends.
fn walk_shaders(dir: &Path, mut f: impl FnMut(u8, [u8; 32], u64, u32)) -> u64 {
    let Ok(file) = std::fs::File::open(dir.join(SHADERS)) else { return 0 };
    let len = file.metadata().map_or(0, |m| m.len());
    let mut file = std::io::BufReader::new(file);
    let mut at = 0;
    let mut head = [0u8; 37];
    while file.read_exact(&mut head).is_ok() {
        let size = u32::from_le_bytes([head[33], head[34], head[35], head[36]]);
        if at + 37 + u64::from(size) > len || file.seek_relative(i64::from(size)).is_err() {
            break;
        }
        let mut hash = [0; 32];
        hash.copy_from_slice(&head[1..33]);
        f(head[0], hash, at + 37, size);
        at += 37 + u64::from(size);
    }
    at
}

/// Calls `f` with each pipeline's number and entry; where the last whole one
/// ends.
fn walk_pipelines(dir: &Path, mut f: impl FnMut(usize, &[u8])) -> u64 {
    let bytes = std::fs::read(dir.join(PIPELINES)).unwrap_or_default();
    let (mut at, mut n) = (0, 0);
    while let Some(head) = bytes.get(at..at + 4) {
        let size = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let Some(entry) = bytes.get(at + 4..at + 4 + size) else { break };
        f(n, entry);
        (at, n) = (at + 4 + size, n + 1);
    }
    at as u64
}

/// How many pipelines are known in `dir`.
pub fn count(dir: &Path) -> usize {
    let mut n = 0;
    walk_pipelines(dir, |_, _| n += 1);
    n
}

/// The key of each pipeline known in `dir`, as the early list has it.
pub fn keys(dir: &Path) -> Vec<String> {
    let mut all = Vec::new();
    walk_pipelines(dir, |_, e| {
        all.push(e.get(..16).unwrap_or_default().iter().map(|b| format!("{b:02x}")).collect());
    });
    all
}

/// What a driver's cached pipelines depend on besides the title's: the
/// translator with its settings, and the driver.
pub fn fingerprint() -> String {
    let mut h = blake3::Hasher::new();
    h.update(&crate::shader_cache::translation());
    if let Some(host) = crate::host::host() {
        h.update(&host.driver().to_le_bytes());
    }
    h.finalize().to_hex()[..16].to_owned()
}

/// What a driver's cached pipeline depends on besides its host description
/// (`names`): the driver.
pub fn driver_fingerprint() -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"driver");
    if let Some(host) = crate::host::host() {
        h.update(&host.driver().to_le_bytes());
    }
    h.finalize().to_hex()[..16].to_owned()
}

/// The physical memory free now, in MB.
pub fn free_memory_mb() -> Option<u64> {
    crate::host::available_physical_ram().map(|b| b >> 20)
}

/// The translations of earlier runs are in `dir` (the pipeline library is
/// not used: what is made here is the driver's to keep).
pub fn use_translations(dir: PathBuf) {
    crate::shader_cache::use_shader_cache(dir);
}

/// What came of one pipeline in [`make`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Made {
    /// Milliseconds in all.
    pub ms: f64,
    /// Of them, translating its shaders (or reading them from the cache).
    pub translate_ms: f64,
    /// Whether there is a host pipeline.
    pub there: bool,
}

/// Stages the host pipelines are made of ([`HostPipeline::new`]): only these
/// are translated before the pipeline is made, so that the time of the
/// translations is told apart from the driver's.
const MADE_OF: [&str; 4] = ["vertex", "pixel", "mesh", "compute"];

/// Makes the host pipelines of those of `which` (numbers in the file) and
/// lets each go again: the driver's cache on disk keeps them. `each` is told
/// the number before, and what came of it after.
pub fn make(dir: &Path, which: &[usize], mut each: impl FnMut(usize, Option<Made>)) {
    walk_entries(dir, which, |n, entry, read| {
        each(n, None);
        let began = std::time::Instant::now();
        let parsed = parse(entry, read);
        if let Some((shaders, _)) = &parsed {
            for s in shaders.iter().filter(|s| MADE_OF.contains(&s.stage)) {
                s.translated();
            }
        }
        let translate_ms = began.elapsed().as_secs_f64() * 1e3;
        let there = parsed.and_then(|(shaders, g)| HostPipeline::new(&shaders, g.as_deref())).is_some();
        each(n, Some(Made { ms: began.elapsed().as_secs_f64() * 1e3, translate_ms, there }));
    });
}

/// The name each of `which` has in the pipeline library: the hash of all
/// that its host description holds (the translated shaders' code, the root
/// signature, the state; `library::name`), found without making it. `None`:
/// it gets no host pipeline (a shader that does not translate, a stage the
/// host is not given). Two pipelines of one name are one pipeline to the
/// driver, and a name that stays the same across a change of the translator
/// is a pipeline whose driver cache entry is still good.
pub fn names(dir: &Path, which: &[usize], mut each: impl FnMut(usize, Option<String>)) {
    walk_entries(dir, which, |n, entry, read| {
        let name = parse(entry, read).and_then(|(shaders, g)| {
            let (name, _) = super::library::name_only(|| HostPipeline::new(&shaders, g.as_deref()));
            name
        });
        each(n, name);
    });
}

/// Calls `f` with each of `which` (numbers in the file), in that order: its
/// entry and a reader of the containers by hash.
#[allow(clippy::type_complexity)]
fn walk_entries(dir: &Path, which: &[usize], mut f: impl FnMut(usize, &[u8], &mut dyn FnMut(&[u8; 32]) -> Option<Vec<u8>>)) {
    let mut places = HashMap::new();
    walk_shaders(dir, |stage, hash, at, len| {
        places.insert(hash, (stage, at, len));
    });
    let Ok(mut file) = std::fs::File::open(dir.join(SHADERS)) else { return };
    let wanted: HashSet<usize> = which.iter().copied().collect();
    let mut entries = HashMap::new();
    walk_pipelines(dir, |n, bytes| {
        if wanted.contains(&n) {
            entries.insert(n, bytes.to_vec());
        }
    });
    let mut read = |hash: &[u8; 32]| {
        let &(_, at, len) = places.get(hash)?;
        let mut bytes = vec![0; len as usize];
        file.seek(SeekFrom::Start(at)).ok()?;
        file.read_exact(&mut bytes).ok()?;
        Some(bytes)
    };
    for n in which {
        let Some(entry) = entries.get(n) else { continue };
        f(*n, entry, &mut read);
    }
}

#[allow(clippy::type_complexity)]
fn parse(
    entry: &[u8],
    read: &mut dyn FnMut(&[u8; 32]) -> Option<Vec<u8>>,
) -> Option<(Vec<Arc<Shader>>, Option<Box<GraphicsState>>)> {
    let mut rest = entry.get(16..)?;
    let (&count, after) = rest.split_first()?;
    rest = after;
    let mut shaders = Vec::new();
    for _ in 0..count {
        let (head, after) = rest.split_first_chunk::<33>()?;
        rest = after;
        let mut hash = [0; 32];
        hash.copy_from_slice(&head[1..]);
        shaders.push(super::shader(STAGES.get(usize::from(head[0]))?, &read(&hash)?));
    }
    let (&graphics, rest) = rest.split_first()?;
    let g = match graphics {
        0 => None,
        _ => Some(Box::new(GraphicsState::get(rest)?)),
    };
    Some((shaders, g))
}

/// Calls `$f` with each number of a graphics state but its input elements',
/// as a place of 32 bits, in one order.
macro_rules! numbers {
    ($g:expr, $f:ident) => {{
        $f!($g.blend.AlphaToCoverageEnable.0);
        $f!($g.blend.IndependentBlendEnable.0);
        for t in &mut $g.blend.RenderTarget {
            $f!(t.BlendEnable.0);
            $f!(t.LogicOpEnable.0);
            $f!(t.SrcBlend.0);
            $f!(t.DestBlend.0);
            $f!(t.BlendOp.0);
            $f!(t.SrcBlendAlpha.0);
            $f!(t.DestBlendAlpha.0);
            $f!(t.BlendOpAlpha.0);
            $f!(t.LogicOp.0);
            $f!(t.RenderTargetWriteMask);
        }
        $f!($g.sample_mask);
        $f!($g.rasterizer.FillMode.0);
        $f!($g.rasterizer.CullMode.0);
        $f!($g.rasterizer.FrontCounterClockwise.0);
        $f!($g.rasterizer.DepthBias);
        $f!($g.rasterizer.DepthClipEnable.0);
        $f!($g.rasterizer.MultisampleEnable.0);
        $f!($g.rasterizer.AntialiasedLineEnable.0);
        $f!($g.rasterizer.ForcedSampleCount);
        $f!($g.rasterizer.ConservativeRaster.0);
        $f!($g.depth.DepthEnable.0);
        $f!($g.depth.DepthWriteMask.0);
        $f!($g.depth.DepthFunc.0);
        $f!($g.depth.StencilEnable.0);
        $f!($g.depth.StencilReadMask);
        $f!($g.depth.StencilWriteMask);
        for face in [&mut $g.depth.FrontFace, &mut $g.depth.BackFace] {
            $f!(face.StencilFailOp.0);
            $f!(face.StencilDepthFailOp.0);
            $f!(face.StencilPassOp.0);
            $f!(face.StencilFunc.0);
        }
        $f!($g.strip_cut.0);
        $f!($g.topology.0);
        $f!($g.targets.NumRenderTargets);
        for format in &mut $g.targets.RTFormats {
            $f!(format.0);
        }
        $f!($g.depth_format.0);
        $f!($g.samples.Count);
        $f!($g.samples.Quality);
    }};
}

impl GraphicsState {
    /// The state as bytes.
    fn put(&self, out: &mut Vec<u8>) {
        out.push(u8::from(self.depth_bounds));
        for f in [self.rasterizer.DepthBiasClamp, self.rasterizer.SlopeScaledDepthBias] {
            out.extend(f.to_le_bytes());
        }
        let mut plain = GraphicsState {
            blend: self.blend,
            sample_mask: self.sample_mask,
            rasterizer: self.rasterizer,
            depth: self.depth,
            strip_cut: self.strip_cut,
            topology: self.topology,
            targets: self.targets,
            depth_format: self.depth_format,
            samples: self.samples,
            ..Default::default()
        };
        macro_rules! write {
            ($x:expr) => {
                out.extend(($x as u32).to_le_bytes())
            };
        }
        numbers!(plain, write);
        out.push(self.inputs.len() as u8);
        for (e, name) in self.inputs.iter().zip(&self.names) {
            let name = name.as_bytes();
            out.push(name.len() as u8);
            out.extend(name);
            for v in [
                e.SemanticIndex,
                e.Format.0 as u32,
                e.InputSlot,
                e.AlignedByteOffset,
                e.InputSlotClass.0 as u32,
                e.InstanceDataStepRate,
            ] {
                out.extend(v.to_le_bytes());
            }
        }
    }

    fn get(bytes: &[u8]) -> Option<GraphicsState> {
        let mut g = GraphicsState::default();
        let (&bounds, mut rest) = bytes.split_first()?;
        g.depth_bounds = bounds != 0;
        let mut next = || {
            let (head, after) = rest.split_first_chunk::<4>()?;
            rest = after;
            Some(u32::from_le_bytes(*head))
        };
        g.rasterizer.DepthBiasClamp = f32::from_bits(next()?);
        g.rasterizer.SlopeScaledDepthBias = f32::from_bits(next()?);
        macro_rules! read {
            ($x:expr) => {
                $x = next()? as _
            };
        }
        numbers!(g, read);
        let (&count, mut rest) = rest.split_first()?;
        let mut elements = Vec::new();
        for _ in 0..count {
            let (&len, after) = rest.split_first()?;
            let (name, after) = after.split_at_checked(usize::from(len))?;
            let (numbers, after) = after.split_first_chunk::<24>()?;
            rest = after;
            let mut v = [0u32; 6];
            for (v, b) in v.iter_mut().zip(numbers.chunks_exact(4)) {
                *v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
            elements.push((CString::new(name).ok()?, v));
        }
        g.names = elements.iter().map(|e| e.0.clone()).collect();
        g.inputs = g
            .names
            .iter()
            .zip(&elements)
            .map(|(name, (_, v))| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(name.as_ptr().cast()),
                SemanticIndex: v[0],
                Format: DXGI_FORMAT(v[1] as i32),
                InputSlot: v[2],
                AlignedByteOffset: v[3],
                InputSlotClass: D3D12_INPUT_CLASSIFICATION(v[4] as i32),
                InstanceDataStepRate: v[5],
            })
            .collect();
        Some(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_comes_back_as_it_was() {
        let mut g = GraphicsState { sample_mask: 7, depth_bounds: true, ..Default::default() };
        g.rasterizer.DepthBias = -3;
        g.rasterizer.SlopeScaledDepthBias = 1.25;
        g.blend.RenderTarget[3].RenderTargetWriteMask = 5;
        g.depth.StencilReadMask = 9;
        g.targets.NumRenderTargets = 2;
        g.targets.RTFormats[1] = DXGI_FORMAT_R8_UNORM;
        g.depth_format = DXGI_FORMAT_D32_FLOAT;
        g.names = vec![CString::new("POSITION").unwrap(), CString::new("T").unwrap()];
        g.inputs = g
            .names
            .iter()
            .enumerate()
            .map(|(n, name)| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(name.as_ptr().cast()),
                SemanticIndex: n as u32,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 2,
                AlignedByteOffset: D3D12_APPEND_ALIGNED_ELEMENT,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                InstanceDataStepRate: 4,
            })
            .collect();
        let mut bytes = Vec::new();
        g.put(&mut bytes);
        let back = GraphicsState::get(&bytes).expect("whole");
        assert_eq!(back.describe(), g.describe());
        assert!(GraphicsState::get(&bytes[..bytes.len() - 1]).is_none());
        assert!(GraphicsState::get(&bytes[..9]).is_none());
    }

    #[test]
    fn a_torn_end_is_not_read() {
        let dir = std::env::temp_dir().join(format!("victory-known-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = Vec::new();
        for n in 0..3u8 {
            bytes.extend(20u32.to_le_bytes());
            bytes.extend([n; 20]);
        }
        std::fs::write(dir.join(PIPELINES), &bytes[..bytes.len() - 5]).unwrap();
        assert_eq!(count(&dir), 2);
        assert_eq!(keys(&dir)[1], "01".repeat(16));
        assert_eq!(walk_pipelines(&dir, |_, _| ()), 48);
        let mut shader = vec![5u8];
        shader.extend([9; 32]);
        shader.extend(4u32.to_le_bytes());
        shader.extend([1, 2, 3, 4]);
        let one = shader.len() as u64;
        shader.extend_from_slice(&shader.clone()[..20]);
        std::fs::write(dir.join(SHADERS), &shader).unwrap();
        let mut seen = Vec::new();
        assert_eq!(walk_shaders(&dir, |stage, hash, at, len| seen.push((stage, hash[0], at, len))), one);
        assert_eq!(seen, [(5, 9, 37, 4)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
