//! Standalone apps: `neyuki build` writes a copy of the runtime with the
//! app's scripts and assets appended, and that copy runs the app instead of
//! the command line.
//!
//! While an app runs, its bundle is installed here as a read-only virtual
//! filesystem. `fs`, `require`, `ui.loadFont` and `ui.loadImage` look a
//! relative path up in the bundle first and fall back to the real
//! filesystem, so the same script works from its project directory under
//! `neyuki run` and from the built executable.

pub mod build;
pub mod bundle;
pub mod icon;
pub mod unpack;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub use bundle::{Bundle, Entry};

/// The bundle of the running app, if this process is one.
static INSTALLED: RwLock<Option<Arc<Bundle>>> = RwLock::new(None);

/// Name of the payload file inside a macOS `.app` (`Contents/Resources/`).
/// The executable there is an unmodified runtime, which keeps its code
/// signature intact.
pub const MACOS_PAYLOAD: &str = "app.nykpak";

/// Makes `bundle` the virtual filesystem for the rest of the process (or
/// removes it with `None`).
pub fn install(bundle: Option<Bundle>) {
    let mut slot = INSTALLED
        .write()
        .unwrap_or_else(|poison| poison.into_inner());
    *slot = bundle.map(Arc::new);
}

/// The installed bundle, if any.
pub fn installed() -> Option<Arc<Bundle>> {
    INSTALLED
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

/// Reads a relative path from the installed bundle. `None` means the path is
/// not bundled and the caller should use the real filesystem.
pub fn read(path: &str) -> Option<std::io::Result<Vec<u8>>> {
    installed()?.read(path)
}

/// What a path names in the installed bundle.
pub fn entry(path: &str) -> Option<Entry> {
    installed()?.entry(path)
}

/// Fails when `path` is bundled, for operations that would change it.
pub fn check_writable(path: &str) -> Result<(), String> {
    match entry(path) {
        Some(_) => Err(format!("`{}` is bundled with the app and read-only", path)),
        None => Ok(()),
    }
}

/// The payload file a macOS `.app` keeps next to an executable at `exe`
/// (`Name.app/Contents/MacOS/name` → `Name.app/Contents/Resources/app.nykpak`).
pub fn macos_payload_path(exe: &Path) -> Option<PathBuf> {
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    if macos_dir.file_name()? != "MacOS" || contents.file_name()? != "Contents" {
        return None;
    }
    Some(contents.join("Resources").join(MACOS_PAYLOAD))
}

/// Finds the bundle of the running executable: a payload appended to it, or
/// the payload file of the macOS `.app` it sits in.
pub fn find_own_bundle() -> Result<Option<Bundle>, String> {
    let Ok(exe) = std::env::current_exe() else {
        return Ok(None);
    };
    if let Some(bundle) = Bundle::from_executable(&exe)? {
        return Ok(Some(bundle));
    }
    match macos_payload_path(&exe) {
        Some(payload) if payload.is_file() => Bundle::from_payload_file(&payload).map(Some),
        _ => Ok(None),
    }
}

/// Runs the installed bundle's entry script.
pub fn run_installed() -> Result<(), String> {
    let bundle = installed().ok_or_else(|| "no app bundle is installed".to_string())?;
    let entry = bundle
        .meta("entry")
        .ok_or_else(|| "the app bundle names no entry script".to_string())?
        .to_string();
    let bytes = bundle
        .read(&entry)
        .ok_or_else(|| {
            format!(
                "the app bundle does not contain its entry script '{}'",
                entry
            )
        })?
        .map_err(|err| format!("cannot read {}: {}", entry, err))?;
    crate::vm::run_bytes(&entry, bytes, crate::compiler::Options::RUN)
}

/// Shows an error that ended an app. Apps built without a console on
/// Windows have nowhere to print it, so it goes to a message box there.
pub fn report_error(message: &str) {
    eprintln!("{}", message);
    #[cfg(windows)]
    if let Some(bundle) = installed()
        && bundle.meta("console") != Some("true")
    {
        show_error_box(bundle.meta("name"), message);
    }
}

#[cfg(windows)]
fn show_error_box(title: Option<&str>, message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let wide = |s: &str| {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let text = wide(message);
    let caption = wide(title.unwrap_or("Neyuki"));
    // SAFETY: both pointers are NUL-terminated UTF-16 buffers that outlive
    // the call, and a null owner window is allowed.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(test)]
mod tests;
