/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::time::Duration;
use std::time::Instant;

use tracing::info;

/// Run a function and log how long it took.
pub fn time<F, R>(label: &str, f: F) -> R
where
    F: FnOnce() -> R,
{
    let now = Instant::now();
    let ret = f();
    info!("{} took {:.2?}", label, now.elapsed());
    ret
}

/// A timer that tracks both wall time and process CPU time.
pub struct ProcessTimer {
    wall_start: Instant,
    cpu_start: Option<Duration>,
}

impl ProcessTimer {
    pub fn new() -> Self {
        Self {
            wall_start: Instant::now(),
            cpu_start: get_process_cpu_time(),
        }
    }

    pub fn elapsed_wall(&self) -> Duration {
        self.wall_start.elapsed()
    }

    pub fn elapsed_cpu(&self) -> Option<Duration> {
        let cpu_start = self.cpu_start?;
        let cpu_now = get_process_cpu_time()?;
        cpu_now.checked_sub(cpu_start)
    }
}

#[cfg(unix)]
fn get_process_cpu_time() -> Option<Duration> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    (ret == 0).then(|| Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

#[cfg(not(unix))]
fn get_process_cpu_time() -> Option<Duration> {
    None
}
