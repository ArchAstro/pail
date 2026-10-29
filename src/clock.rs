//! The clock leases are judged by (`lease`), so tests can move time instead of waiting.
//!
//! A lease reads time three ways, each with its own failure to guard against:
//!
//! 1. [`Clock::instant`]: how long ago *this process* refreshed its lease. Monotonic, so a
//!    jump of the wall clock cannot make a stale lease look fresh.
//! 2. [`Clock::unix_secs`]: the age of *another* holder's lease, against the store's
//!    LastModified. The only clock the two sides share, however far apart they are.
//! 3. [`Clock::sleep`]: the refresher's pause between rewrites.
//!
//! A [`Store`](crate::store::Store) carries its clock ([`Store::with_clock`]
//! (crate::store::Store::with_clock)); production uses [`SystemClock`]. The refinement
//! tests (`tests/refinement.rs`) advance their own one tick at a time, as the model does
//! (`verify/clock.qnt`).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub trait Clock: Send + Sync + 'static {
    /// A monotonic reading: only differences between two readings mean anything.
    fn instant(&self) -> Duration;
    /// The wall clock, in unix seconds.
    fn unix_secs(&self) -> i64;
    /// Resolves once `d` has passed on this clock.
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// The real time.
pub struct SystemClock;

impl Clock for SystemClock {
    fn instant(&self) -> Duration {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed()
    }

    fn unix_secs(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(d))
    }
}

pub fn system() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}
