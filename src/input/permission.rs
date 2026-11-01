//! Input Monitoring permission (TCC "ListenEvent").
//!
//! Note: macOS attributes the permission to the *responsible* process. When
//! run from Terminal that is Terminal.app; when run by launchd it is the
//! rustic-keys binary itself.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Granted,
    Denied,
    Unknown,
}

const K_IOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDCheckAccess(request_type: u32) -> u32;
    fn IOHIDRequestAccess(request_type: u32) -> bool;
}

pub fn check() -> Access {
    // kIOHIDAccessTypeGranted = 0, Denied = 1, Unknown = 2
    match unsafe { IOHIDCheckAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT) } {
        0 => Access::Granted,
        1 => Access::Denied,
        _ => Access::Unknown,
    }
}

/// Ask macOS for permission. Shows the system prompt the first time; returns
/// whether access is currently granted.
pub fn request() -> bool {
    unsafe { IOHIDRequestAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT) }
}
