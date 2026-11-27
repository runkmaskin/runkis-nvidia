//! Threads of ours under their names in profilers.
//!
//! ETW, WPA and the debugger read the name the system keeps for a thread
//! (`GetThreadDescription`), which was empty for every thread of ours but two
//! in the profile of 2026-11-27. The standard library of toolchain 1.98 sets
//! it from `std::thread::Builder::name` already (the test below sees it), so
//! that profile's gap is in what the trace recorded, not in the threads. This
//! sets it once more, explicitly, where the thread starts: [`spawn`] for the
//! spawn sites, [`describe_current`] for threads spawned where the builder is
//! not ours to change (`crate::cores` calls it). Nothing else changes.

/// A thread named `name`, for Rust and for the system: as
/// `std::thread::Builder::new().name(name).spawn(f)`, and the thread's first
/// act is [`describe_current`].
pub(crate) fn spawn<F, T>(name: impl Into<String>, f: F) -> std::io::Result<std::thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new().name(name.into()).spawn(move || {
        describe_current();
        f()
    })
}

/// Gives the calling thread's Rust name to the system, if it has one. Only a
/// description changes: nothing the thread does.
pub(crate) fn describe_current() {
    let Some(name) = std::thread::current().name().map(str::to_owned) else {
        return;
    };
    describe(&name);
}

/// Sets the calling thread's system description to `name`.
fn describe(name: &str) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadDescription};
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: this thread's own handle; a string ending in 0 that lives
    // through the call (the system copies it).
    unsafe { SetThreadDescription(GetCurrentThread(), wide.as_ptr()) };
}

#[cfg(test)]
mod tests {
    /// The system's description of the calling thread.
    fn description() -> String {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadDescription};
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: this thread's handle; the system allocates the string,
        // which is freed with LocalFree.
        unsafe {
            if GetThreadDescription(GetCurrentThread(), &mut text) < 0 || text.is_null() {
                return String::new();
            }
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let out = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            windows_sys::Win32::Foundation::LocalFree(text.cast());
            out
        }
    }

    #[test]
    fn a_thread_of_ours_has_its_name_for_the_system_too() {
        let seen = super::spawn("victory-test-named", || (std::thread::current().name().map(str::to_owned), description()))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(seen, (Some("victory-test-named".to_owned()), "victory-test-named".to_owned()));
        // A thread spawned elsewhere takes its Rust name over.
        let seen = std::thread::Builder::new()
            .name("victory-test-later".into())
            .spawn(|| {
                // (The standard library sets it too, on this toolchain.)
                let from_std = description();
                super::describe("something else");
                super::describe_current();
                (from_std, description())
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(seen, ("victory-test-later".to_owned(), "victory-test-later".to_owned()));
    }
}
