//! TCC permission helpers for screen-recording and microphone access.
//!
//! These wrap the macOS privacy APIs so callers can check (and optionally
//! request) the permissions required before starting a capture session.

/// Check (and optionally request) Screen Recording permission.
///
/// When `prompt` is `false`, this performs a non-intrusive preflight check via
/// `CGPreflightScreenCaptureAccess`. When `prompt` is `true`, it calls
/// `CGRequestScreenCaptureAccess`, which triggers the system permission dialog
/// (or adds the app to the Screen Recording list) and returns whether access is
/// granted.
///
/// Returns `true` if screen-capture access is currently granted.
#[cfg(target_os = "macos")]
pub fn screen_capture_access(prompt: bool) -> bool {
    // These symbols are available on macOS 10.15+; this crate is macOS-only.
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    unsafe {
        if prompt {
            CGRequestScreenCaptureAccess()
        } else {
            CGPreflightScreenCaptureAccess()
        }
    }
}

/// Non-macOS fallback: there is no screen-capture TCC concept.
#[cfg(not(target_os = "macos"))]
pub fn screen_capture_access(_prompt: bool) -> bool {
    false
}

/// Check (and optionally request) microphone permission.
///
/// Returns one of `"authorized"`, `"denied"`, `"restricted"`,
/// `"not_determined"`, or `"unknown"`.
///
/// When `prompt` is `true` and the current status is `"not_determined"`, this
/// kicks off a non-blocking `requestAccessForMediaType` request (which shows the
/// system dialog). It does not wait for the user's response; the returned string
/// reflects the status at the time of the call.
#[cfg(target_os = "macos")]
pub fn microphone_access(prompt: bool) -> &'static str {
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

    unsafe {
        let media_type = match AVMediaTypeAudio {
            Some(mt) => mt,
            None => return "unknown",
        };

        let status = AVCaptureDevice::authorizationStatusForMediaType(media_type);

        if prompt && status == AVAuthorizationStatus::NotDetermined {
            // Fire off the request without blocking on the async completion.
            let handler = RcBlock::new(|_granted: Bool| {});
            AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &handler);
        }

        match status {
            AVAuthorizationStatus::Authorized => "authorized",
            AVAuthorizationStatus::Denied => "denied",
            AVAuthorizationStatus::Restricted => "restricted",
            AVAuthorizationStatus::NotDetermined => "not_determined",
            _ => "unknown",
        }
    }
}

/// Non-macOS fallback: there is no microphone TCC concept.
#[cfg(not(target_os = "macos"))]
pub fn microphone_access(_prompt: bool) -> &'static str {
    "unknown"
}
