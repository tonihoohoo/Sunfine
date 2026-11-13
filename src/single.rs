//! One instance at a time. Two copies would both drive the display (e.g. an
//! old version still in the tray keeps writing its own level back, so the
//! new one's changes seem to do nothing). A newly started copy asks the
//! running one to quit and takes over.

/// Held for the life of the process.
pub struct Instance {
    #[cfg(windows)]
    _mutex: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
mod imp {
    use super::Instance;
    use windows::core::w;
    use windows::Win32::Foundation::{
        GetLastError, ERROR_ALREADY_EXISTS, WAIT_ABANDONED, WAIT_OBJECT_0,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, CreateMutexW, ResetEvent, SetEvent, WaitForSingleObject, INFINITE,
    };

    /// Takes the instance lock, asking an already running copy to quit
    /// first. `on_quit_request` runs (on a helper thread) when a later copy
    /// asks this one to quit.
    pub fn acquire(on_quit_request: impl FnOnce() + Send + 'static) -> Instance {
        unsafe {
            // Auto-reset event the running copy waits on.
            let quit = CreateEventW(None, false, false, w!("Local\\ultrafine-ctl-quit")).ok();
            let mutex = CreateMutexW(None, true, w!("Local\\ultrafine-ctl")).unwrap_or_default();
            if GetLastError() == ERROR_ALREADY_EXISTS {
                log::info!("another copy is running; asking it to quit");
                if let Some(q) = quit {
                    let _ = SetEvent(q);
                }
                match WaitForSingleObject(mutex, 10_000) {
                    r if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED => {
                        log::info!("took over from the previous copy")
                    }
                    _ => log::warn!("previous copy did not quit within 10 s; continuing anyway"),
                }
            }
            if let Some(q) = quit {
                // If the old copy never picked the request up (older
                // versions don't listen), don't quit ourselves on it.
                let _ = ResetEvent(q);
                let q = q.0 as usize;
                std::thread::Builder::new()
                    .name("single-instance".into())
                    .spawn(move || {
                        let q = windows::Win32::Foundation::HANDLE(q as _);
                        if WaitForSingleObject(q, INFINITE) == WAIT_OBJECT_0 {
                            log::info!("a newer copy started; quitting");
                            on_quit_request();
                        }
                    })
                    .ok();
            }
            Instance { _mutex: mutex }
        }
    }
}

#[cfg(windows)]
pub use imp::acquire;

#[cfg(not(windows))]
pub fn acquire(_on_quit_request: impl FnOnce() + Send + 'static) -> Instance {
    Instance {}
}
