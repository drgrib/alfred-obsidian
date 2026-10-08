use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;

extern "C" {
    fn perform_ocr(path: *const c_char) -> *mut c_char;
    fn free_ocr_string(ptr: *mut c_char);
}

extern "C" {
    /// Signal 0 is a no-op that still fails with ESRCH when the pid does not exist,
    /// which makes it a cheap liveness probe.
    pub fn kill(pid: i32, sig: i32) -> i32;
}

/// Extracts the text of an attachment through the native Vision and PDFKit frameworks.
///
/// Returns an empty string when the file cannot be read, so a failed extraction is
/// cached as "no text" rather than retried on every pass.
pub fn recognize_text(path: &str) -> String {
    if let Ok(c_path) = CString::new(path) {
        unsafe {
            let ptr = perform_ocr(c_path.as_ptr());
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
