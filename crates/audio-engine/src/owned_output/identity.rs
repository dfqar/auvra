//! No argv/env reads: ownership is the live supervisor chain plus vendor signature.
use objc2_foundation::{NSString, NSURL};
use std::{
    ffi::{CStr, c_void},
    path::PathBuf,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Identity {
    pid: i32,
    start_seconds: u64,
    start_micros: u64,
}
fn info(pid: i32) -> Option<libc::proc_bsdinfo> {
    let mut value = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    (unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            value.as_mut_ptr().cast(),
            size,
        )
    } == size)
        .then(|| unsafe { value.assume_init() })
}
fn path(pid: i32) -> Option<PathBuf> {
    let mut bytes = [0i8; 4096];
    if unsafe { libc::proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) } <= 0 {
        return None;
    }
    let path = unsafe { CStr::from_ptr(bytes.as_ptr()) }.to_str().ok()?;
    Some(PathBuf::from(path))
}
impl Identity {
    pub(super) fn verify(pid: i32) -> Option<Self> {
        if pid <= 0 {
            return None;
        }
        let child = info(pid)?;
        let parent = info(child.pbi_ppid as i32)?;
        if child.pbi_uid != unsafe { libc::getuid() }
            || parent.pbi_uid != child.pbi_uid
            || parent.pbi_ppid != unsafe { libc::getpid() } as u32
        {
            return None;
        }
        let executable = path(pid)?;
        let current = std::env::current_exe().ok()?;
        // The exact helper within this running bundle, not merely a filename suffix.
        let expected = current
            .parent()?
            .parent()?
            .join("Helpers/AuvraPlaybackHost");
        if path(child.pbi_ppid as i32)?.canonicalize().ok()? != expected.canonicalize().ok()?
            || executable.file_name()? != "soloist"
            || !vendor_signed(&executable)
        {
            return None;
        }
        Some(Self {
            pid,
            start_seconds: child.pbi_start_tvsec,
            start_micros: child.pbi_start_tvusec,
        })
    }
    pub(super) fn alive(self) -> bool {
        info(self.pid).is_some_and(|value| {
            value.pbi_start_tvsec == self.start_seconds
                && value.pbi_start_tvusec == self.start_micros
                && value.pbi_status != libc::SZOMB
        })
    }
}
#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecStaticCodeCreateWithPath(
        path: *const c_void,
        flags: u32,
        code: *mut *const c_void,
    ) -> i32;
    fn SecRequirementCreateWithString(
        text: *const c_void,
        flags: u32,
        requirement: *mut *const c_void,
    ) -> i32;
    fn SecStaticCodeCheckValidity(
        code: *const c_void,
        flags: u32,
        requirement: *const c_void,
    ) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
}
fn vendor_signed(path: &std::path::Path) -> bool {
    let Some(path) = path.to_str() else {
        return false;
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    let requirement = NSString::from_str(
        "anchor apple generic and identifier \"com.spotify.soloist\" and certificate leaf[subject.OU] = \"2FNC3A47ZF\"",
    );
    let (mut code, mut rule) = (std::ptr::null(), std::ptr::null());
    // NSURL/NSString are toll-free bridged CFURL/CFString; returned objects follow Create rule.
    let result = unsafe {
        SecStaticCodeCreateWithPath((&*url as *const NSURL).cast(), 0, &mut code) == 0
            && SecRequirementCreateWithString(
                (&*requirement as *const NSString).cast(),
                0,
                &mut rule,
            ) == 0
            && SecStaticCodeCheckValidity(code, 1 << 0, rule) == 0
    };
    unsafe {
        if !code.is_null() {
            CFRelease(code);
        }
        if !rule.is_null() {
            CFRelease(rule);
        }
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn does_not_accept_current_process_or_invalid_pid() {
        assert!(Identity::verify(0).is_none());
        assert!(Identity::verify(-1).is_none());
        assert!(Identity::verify(unsafe { libc::getpid() }).is_none());
    }
}
