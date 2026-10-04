//! Win32 named mutexes, which the Worker's processes coordinate through.

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex};
use windows::core::{HSTRING, PCWSTR};

/// A handle to a named mutex. Dropping it releases ownership (a no-op when
/// this handle never owned it) and closes the handle.
pub(crate) struct NamedMutex(HANDLE);

impl NamedMutex {
    /// Creates and owns the mutex, or returns `None` when any handle to it
    /// exists already, including one in this process. Suits single-instance
    /// guards, where a second instance must never proceed.
    pub(crate) fn acquire(name: &str) -> windows::core::Result<Option<Self>> {
        let name = HSTRING::from(name);
        let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // Close without releasing: this handle never took ownership,
            // and a release here would drop this thread's ownership taken
            // through another handle.
            unsafe { CloseHandle(handle) }?;
            return Ok(None);
        }
        Ok(Some(Self(handle)))
    }
}

// Only the service installer waits for ownership.
#[cfg(feature = "worker")]
impl NamedMutex {
    /// Opens the mutex without taking ownership, creating it when no
    /// process has it open.
    pub(crate) fn open(name: &str) -> windows::core::Result<Self> {
        let name = HSTRING::from(name);
        unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.map(Self)
    }

    /// Waits up to `timeout` for ownership; `false` means it timed out.
    pub(crate) fn wait(&self, timeout: std::time::Duration) -> windows::core::Result<bool> {
        use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::WaitForSingleObject;

        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let status = unsafe { WaitForSingleObject(self.0, millis) };
        if status == WAIT_OBJECT_0 || status == WAIT_ABANDONED {
            Ok(true)
        } else if status == WAIT_TIMEOUT {
            Ok(false)
        } else {
            Err(windows::core::Error::from_thread())
        }
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}
