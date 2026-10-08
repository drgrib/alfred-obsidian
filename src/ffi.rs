use std::env;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::Path;
use std::sync::OnceLock;

extern "C" {
    fn perform_ocr(path: *const c_char, fast_level: c_int) -> *mut c_char;
    fn free_ocr_string(ptr: *mut c_char);
}

extern "C" {
    /// Signal 0 is a no-op that still fails with ESRCH when the pid does not exist,
    /// which makes it a cheap liveness probe.
    pub fn kill(pid: i32, sig: i32) -> i32;
}

/// True unless the `ocr_accurate` workflow variable opts into Vision's accurate
/// recognizer. Fast is the default because it is several times cheaper on the user's
/// hardware; accuracy is something the user raises deliberately. Read once per
/// process: shards inherit the environment, so every shard agrees.
pub fn use_fast_ocr() -> bool {
    static FAST: OnceLock<bool> = OnceLock::new();
    *FAST.get_or_init(|| {
        let accurate = env::var("ocr_accurate")
            .map(|value| matches!(value.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        !accurate
    })
}

/// Extracts the text of an attachment through the native Vision and PDFKit frameworks.
///
/// Returns an empty string when the file cannot be read, so a failed extraction is
/// cached as "no text" rather than retried on every pass.
///
/// This runs Vision in the calling process. ImageIO and Vision can die with a signal on
/// malformed input, which no `@catch` can intercept, so indexing code should go through
/// `ocr_pool` instead, which runs this inside disposable shard processes.
pub fn recognize_text(path: &str) -> String {
    if let Ok(c_path) = CString::new(path) {
        unsafe {
            let ptr = perform_ocr(c_path.as_ptr(), use_fast_ocr() as c_int);
            if !ptr.is_null() {
                let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
                free_ocr_string(ptr);
                return text;
            }
        }
    }
    String::new()
}

/// True when the file is an attachment whose text can be extracted: raster images go
/// through Vision, and PDFs use their embedded text layer with Vision as a fallback for
/// pages that turn out to be scans.
pub fn is_supported_image(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(ext.to_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp" | "pdf"),
        None => false,
    }
}
