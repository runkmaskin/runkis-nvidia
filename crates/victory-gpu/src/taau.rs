//! Finds the title's temporal upscaler at run time and reports what it
//! reads and writes (`VICTORY_TAAU_REPORT=1`, diagnostics builds only).
//!
//! DLSS needs the inputs of the pass it stands in for: the colour at render
//! size, its depth and motion vectors, the jitter, and the texture the pass
//! writes at output size. None of that may be kept in the repository, so it
//! is found from the running title: a compute pass is an upscaler when its
//! dispatch covers a texture it writes, and the textures it reads are
//! smaller by one ratio in both directions. The report names, per binding
//! (root parameter and place in its table), the texture's size and format,
//! and the constant buffers' first words, a few frames in a row, so that the
//! jitter is the pair of words that moves every frame by less than a pixel.
//! See `docs/dlss.md` for what a run found.

use std::collections::HashMap;
use std::sync::Mutex;

/// Whether the report is on (`VICTORY_TAAU_REPORT=1`).
pub(crate) fn report_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_TAAU_REPORT"))
}

/// Shader hashes `VICTORY_TAAU_REPORT` names (comma-separated, as the log
/// names shaders): their passes are reported whatever their shape, to see
/// a pass that runs at one size (a TAA before a separate upscale).
fn names(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Whether the shader `hash` is one `VICTORY_TAAU_REPORT` names.
pub(crate) fn named(hash: &str) -> bool {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES
        .get_or_init(|| names(&std::env::var("VICTORY_TAAU_REPORT").unwrap_or_default()))
        .iter()
        .any(|h| h == hash)
}

/// Read or written by the pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Written,
}

/// One 2D texture a pass binds, as the report sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Image {
    pub(crate) access: Access,
    /// Root parameter and place in its table, in dwords.
    pub(crate) slot: u8,
    pub(crate) offset: u32,
    pub(crate) size: (u32, u32),
    /// Host DXGI format number.
    pub(crate) format: u32,
}

/// What makes a pass an upscaler: the texture it writes at output size and
/// the size most of what it reads has.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Shape {
    pub(crate) output: (u32, u32),
    pub(crate) render: (u32, u32),
    /// Output size over render size.
    pub(crate) ratio: f32,
}

/// Whether `images`, bound by a dispatch that runs `covered` threads in
/// groups of `group`, are an upscaler's. The written texture whose size the
/// dispatch covers to within a group is the output; the textures read at a
/// smaller size, by the same ratio in both directions (1.1 to 4 times), are
/// its inputs. At least two such inputs are asked for (colour and depth,
/// colour and motion), so that a pass that scales one picture up (a blur's
/// last step, the interface) is not taken for one.
pub(crate) fn upscale_shape(covered: (u32, u32), group: (u32, u32), images: &[Image]) -> Option<Shape> {
    let fits = |size: u32, threads: u32, step: u32| threads >= size && threads - size < step.max(1);
    let output = images
        .iter()
        .filter(|i| i.access == Access::Written)
        .filter(|i| fits(i.size.0, covered.0, group.0) && fits(i.size.1, covered.1, group.1))
        .map(|i| i.size)
        .max_by_key(|s| u64::from(s.0) * u64::from(s.1))?;
    let ratio_of = |s: (u32, u32)| -> Option<f32> {
        if s.0 == 0 || s.1 == 0 || s.0 >= output.0 || s.1 >= output.1 {
            return None;
        }
        let (rx, ry) = (output.0 as f32 / s.0 as f32, output.1 as f32 / s.1 as f32);
        ((1.1..=4.0).contains(&rx) && (rx - ry).abs() <= 0.02 * rx).then_some(rx)
    };
    let mut counts: HashMap<(u32, u32), usize> = HashMap::new();
    for i in images.iter().filter(|i| i.access == Access::Read) {
        if ratio_of(i.size).is_some() {
            *counts.entry(i.size).or_default() += 1;
        }
    }
    let (render, n) = counts.into_iter().max_by_key(|&(s, n)| (n, u64::from(s.0) * u64::from(s.1)))?;
    if n < 2 {
        return None;
    }
    Some(Shape {
        output,
        render,
        ratio: ratio_of(render)?,
    })
}

/// What a texture of this host format usually is in an upscaler's inputs,
/// by format alone (a guess the report prints next to each binding).
pub(crate) fn guess(format: u32) -> &'static str {
    match format {
        // R32 typeless/float, D32, R24G8 and D24S8 families, D32S8, R16 unorm/D16.
        39..=41 | 44..=47 | 19..=22 | 55 | 56 => "depth?",
        // Two 16-bit channels: float, unorm, snorm.
        33..=38 => "motion?",
        // R11G11B10, R16G16B16A16 float, R10G10B10A2, RGBA8, BGRA8.
        26 | 9 | 10 | 23..=25 | 27..=29 | 87 | 91 => "colour?",
        // One 8- or 16-bit channel.
        49..=54 | 57..=65 => "mask?",
        _ => "",
    }
}

/// Each pass reported, by its shader's hash: how often it was seen.
static SEEN: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// Whether a dispatch of the shader `hash` gets a report now: the first six
/// times it is seen, then six in a row every 600th time (the constant buffers
/// of consecutive frames show which words are the jitter).
pub(crate) fn wanted(hash: &str) -> Option<u64> {
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let n = seen.get_or_insert_with(HashMap::new).entry(hash.to_owned()).or_default();
    *n += 1;
    (*n <= 6 || (*n % 600) < 6).then_some(*n)
}

/// The constant buffer words at a title address as text (eight floats and
/// their bits a line), if that memory can be read: the title's GPU
/// addresses are CPU addresses.
pub(crate) fn words(address: u64, bytes: u32) -> Option<String> {
    use std::fmt::Write;
    let len = (bytes as usize).min(1024) / 4 * 4;
    if address == 0 || len == 0 || !crate::dlss::readable(address as usize, len) {
        return None;
    }
    // SAFETY: committed, readable memory of the process, checked above.
    let slice = unsafe { std::slice::from_raw_parts(address as *const u32, len / 4) };
    let mut text = String::new();
    for (row, chunk) in slice.chunks(8).enumerate() {
        let _ = write!(text, "\n      [{:3}]", row * 8);
        for w in chunk {
            let f = f32::from_bits(*w);
            if f.is_finite() && (f == 0.0 || (1e-6..1e7).contains(&f.abs())) {
                let _ = write!(text, " {f:.6}");
            } else {
                let _ = write!(text, " {w:#010x}");
            }
        }
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(access: Access, offset: u32, size: (u32, u32), format: u32) -> Image {
        Image { access, slot: 1, offset, size, format }
    }

    /// Colour, depth and motion at half size, last frame's result at full
    /// size; the output written at full size, the dispatch in 8x8 groups.
    fn upscaler() -> Vec<Image> {
        vec![
            image(Access::Read, 0, (960, 540), 26),
            image(Access::Read, 8, (960, 540), 41),
            image(Access::Read, 16, (960, 540), 34),
            image(Access::Read, 24, (1920, 1080), 10),
            image(Access::Written, 120, (1920, 1080), 10),
            image(Access::Written, 128, (1, 1), 41),
        ]
    }

    #[test]
    fn an_upscaler_is_found_by_what_it_writes_and_reads() {
        let shape = upscale_shape((1920, 1080), (8, 8), &upscaler()).expect("an upscaler");
        assert_eq!(shape.output, (1920, 1080));
        assert_eq!(shape.render, (960, 540));
        assert!((shape.ratio - 2.0).abs() < 1e-6);
        // 720p to 1080p, with a dispatch rounded up to whole 16x16 groups.
        let mut third: Vec<Image> = upscaler();
        for i in &mut third {
            if i.size == (960, 540) {
                i.size = (1280, 720);
            }
        }
        let shape = upscale_shape((1920, 1088), (16, 16), &third).expect("720p in");
        assert_eq!(shape.render, (1280, 720));
    }

    #[test]
    fn passes_at_one_size_or_scaling_down_are_not_upscalers() {
        // A blur: everything at one size.
        let same = [
            image(Access::Read, 0, (960, 540), 26),
            image(Access::Read, 8, (960, 540), 41),
            image(Access::Written, 120, (960, 540), 26),
        ];
        assert_eq!(upscale_shape((960, 544), (8, 8), &same), None);
        // A downsample: it writes the smaller texture.
        let down = [
            image(Access::Read, 0, (1920, 1080), 26),
            image(Access::Read, 8, (1920, 1080), 41),
            image(Access::Written, 120, (960, 540), 26),
        ];
        assert_eq!(upscale_shape((960, 544), (8, 8), &down), None);
        // One picture scaled up (one input only).
        let one = [image(Access::Read, 0, (960, 540), 26), image(Access::Written, 120, (1920, 1080), 26)];
        assert_eq!(upscale_shape((1920, 1080), (8, 8), &one), None);
    }

    #[test]
    fn the_dispatch_must_cover_the_output() {
        // Only the render size is covered: a pass at render size that also
        // writes a full-size texture in part.
        assert_eq!(upscale_shape((960, 544), (8, 8), &upscaler()), None);
        // More than a group past the output.
        assert_eq!(upscale_shape((1936, 1080), (8, 8), &upscaler()), None);
    }

    #[test]
    fn inputs_must_shrink_by_one_ratio() {
        let squashed = [
            image(Access::Read, 0, (960, 1080), 26),
            image(Access::Read, 8, (960, 1080), 41),
            image(Access::Written, 120, (1920, 1080), 26),
        ];
        assert_eq!(upscale_shape((1920, 1080), (8, 8), &squashed), None, "one direction only");
    }

    #[test]
    fn named_passes_are_hashes_in_the_list() {
        assert_eq!(names("1"), Vec::<String>::new());
        assert_eq!(names("DF11dcd012cd7382, 1, nothex0123456789"), ["df11dcd012cd7382"]);
    }

    #[test]
    fn formats_are_guessed_by_family() {
        assert_eq!(guess(41), "depth?");
        assert_eq!(guess(34), "motion?");
        assert_eq!(guess(26), "colour?");
        assert_eq!(guess(61), "mask?");
        assert_eq!(guess(2), "");
    }

    #[test]
    fn each_pass_is_reported_six_times_then_six_every_600() {
        let hash = "test-only-pass";
        let picked: Vec<u64> = (1..=1300u64).filter_map(|_| wanted(hash)).collect();
        // Counted from 1: 1..=6, 600..=605 and 1200..=1205.
        assert_eq!(picked, [1, 2, 3, 4, 5, 6, 600, 601, 602, 603, 604, 605, 1200, 1201, 1202, 1203, 1204, 1205]);
    }

    #[test]
    fn unreadable_memory_gives_no_words() {
        assert_eq!(words(0, 64), None);
        assert_eq!(words(8, 64), None, "the first page is never mapped");
        let data: [f32; 9] = [0.0, 1.0, -0.25, 1920.0, 0.5, 0.0, 0.0, 0.0, 2.0];
        let text = words(data.as_ptr() as u64, 36).expect("our own memory");
        assert!(text.contains("-0.250000") && text.contains("1920.000000"), "{text}");
        assert!(text.contains("[  8]"), "a second line: {text}");
    }
}
