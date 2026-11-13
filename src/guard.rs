//! Time limit for display I/O.
//!
//! hidapi's feature-report calls wait for the device without a timeout
//! (Windows: `HidD_SetFeature`, and `GetOverlappedResult(.., INFINITE)` for
//! reads). If the display firmware stops answering, the calling thread
//! hangs until the display is power cycled, and with it every hotkey, the
//! tray status and the settings. [`Guarded`] runs the real device on a
//! thread of its own and gives each call a deadline: a late call becomes an
//! error, the worker drops the device and keeps running, and it does not
//! open the display again until the stuck call has returned (a new handle to
//! a firmware that does not answer would only hang a second thread).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::worker::{Connector, Device};

type Job<D> = Box<dyn FnOnce(&D) + Send>;

/// Set while a device thread is inside a call to the display.
#[derive(Clone, Default)]
pub struct Busy(Arc<AtomicBool>);

impl Busy {
    pub fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A device whose calls run on its own thread with a deadline.
pub struct Guarded<D> {
    jobs: Sender<Job<D>>,
    has_sensor: bool,
    timeout: Duration,
}

impl<D: Device + Send + 'static> Guarded<D> {
    pub fn new(dev: D, timeout: Duration, busy: Busy) -> Result<Self> {
        let has_sensor = dev.has_light_sensor();
        let (jobs, rx) = channel::<Job<D>>();
        std::thread::Builder::new()
            .name("display-io".into())
            .spawn(move || {
                // Ends when the Guarded is dropped (after a late call returns).
                for job in rx {
                    busy.0.store(true, Ordering::SeqCst);
                    job(&dev);
                    busy.0.store(false, Ordering::SeqCst);
                }
            })?;
        Ok(Self {
            jobs,
            has_sensor,
            timeout,
        })
    }

    fn call<R: Send + 'static>(
        &self,
        what: &str,
        extra: Duration,
        f: impl FnOnce(&D) -> R + Send + 'static,
    ) -> Result<R> {
        let (tx, rx) = channel();
        self.jobs
            .send(Box::new(move |d| {
                let _ = tx.send(f(d));
            }))
            .map_err(|_| anyhow!("display thread ended"))?;
        let limit = self.timeout + extra;
        rx.recv_timeout(limit).map_err(|_| {
            anyhow::Error::new(NotResponding(format!(
                "display did not answer {what} within {} ms",
                limit.as_millis()
            )))
        })
    }
}

/// A call to the display missed its deadline.
#[derive(Debug)]
pub struct NotResponding(String);

impl std::fmt::Display for NotResponding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotResponding {}

/// The error means "the display stopped answering" (not "unplugged").
pub fn is_not_responding(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NotResponding>().is_some()
}

impl<D: Device + Send + 'static> Device for Guarded<D> {
    fn has_light_sensor(&self) -> bool {
        self.has_sensor
    }
    fn brightness(&self) -> Result<u32> {
        self.call("a brightness read", Duration::ZERO, |d| d.brightness())?
    }
    fn set_brightness(&self, level: u32, transition_ms: u16) -> Result<()> {
        self.call("a brightness write", Duration::ZERO, move |d| {
            d.set_brightness(level, transition_ms)
        })?
    }
    fn read_lux(&self, timeout_ms: i32) -> Result<Option<u32>> {
        let wait = Duration::from_millis(timeout_ms.max(0) as u64);
        self.call("a sensor read", wait, move |d| d.read_lux(timeout_ms))?
    }
    fn initial_lux(&self) -> Option<u32> {
        // Best effort: a late answer is simply "not available".
        self.call("the initial lux request", Duration::ZERO, |d| {
            d.initial_lux()
        })
        .ok()
        .flatten()
    }
}

/// Wraps a connector so every opened device is [`Guarded`].
pub struct GuardedConnector<C> {
    inner: C,
    timeout: Duration,
    busy: Busy,
    waiting_logged: bool,
}

impl<C> GuardedConnector<C> {
    pub fn new(inner: C, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            busy: Busy::default(),
            waiting_logged: false,
        }
    }

    /// A previous device thread is still stuck in a call to the display.
    pub fn stuck(&self) -> bool {
        self.busy.get()
    }
}

impl<C: Connector> Connector for GuardedConnector<C>
where
    C::D: Send + 'static,
{
    type D = Guarded<C::D>;

    fn open(&mut self) -> Result<Option<Self::D>> {
        if self.busy.get() {
            if !self.waiting_logged {
                log::warn!(
                    "display still not answering an earlier call; not reopening until it does \
                     (power cycle the display if this lasts)"
                );
                self.waiting_logged = true;
            }
            return Ok(None);
        }
        if self.waiting_logged {
            log::info!("display answered the stuck call; reopening");
            self.waiting_logged = false;
        }
        match self.inner.open()? {
            Some(d) => Ok(Some(Guarded::new(d, self.timeout, self.busy.clone())?)),
            None => Ok(None),
        }
    }
}
