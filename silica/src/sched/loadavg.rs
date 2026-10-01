// SPDX-License-Identifier: GPL-2.0

//! The load average: how much work the system has been asked to do relative to
//! how much it could have done.
//!
//! Every tick each CPU adds the number of its runnable tasks to a
//! system-wide sum. Once an interval is over, that sum divided by the number
//! of ticks in it is the average load of the interval, and it is mixed into
//! three exponentially decayed averages with time constants of one, five and
//! fifteen minutes. The decay is the same as Linux's: an interval contributes
//! `1 - e^(-interval/τ)` of the difference between itself and the average, so
//! the one minute average moves by about 8% of the gap per interval and the
//! fifteen minute average by about 0.6%.
//!
//! # No floating point
//!
//! Everything is `u64` fixed point. [`LoadAvg`] is in units of
//! `1 / LOAD_AVG_MAX`, which is what makes the values directly comparable with
//! Linux's, and its [`Display`](core::fmt::Display) prints them with integer
//! arithmetic, because formatting a float needs an unstable feature in
//! `no_std` and the whole of a load average fits in the digits of a fraction.

use core::sync::atomic::{AtomicU64, Ordering};

use ostd::timer::{Jiffies, TIMER_FREQ};

/// How often an interval of the load average is accounted for, in seconds.
const LOAD_INTERVAL_S: u64 = 5;

/// The number of ticks in an interval, which is the interval in seconds times
/// the frequency of the clock that counts the ticks.
const TICKS_PER_INTERVAL: u64 = LOAD_INTERVAL_S * TIMER_FREQ;

/// The scale of a load average: a runnable task on an otherwise idle system is
/// a load of one.
pub const LOAD_AVG_MAX: u64 = 65535;

/// How many more bits of resolution the averages keep above [`LOAD_AVG_MAX`].
///
/// The decay moves an average by a fraction of the way towards each interval,
/// and in whole numbers that step rounds to nothing once the average is within
/// one part in `LOAD_FACTOR / (LOAD_FACTOR - EXP_15)` of its target — a little
/// under a third of a thousandth of a load. That is a stall a system at a load of
/// exactly one would sit at, and it would print as `0.99`; the extra bits move
/// the stall to well under the precision of the display.
const LOAD_AVG_BITS: u32 = 8;

/// The scale the averages are held in: a runnable task on an otherwise idle
/// system is [`LOAD_AVG_SCALE`].
pub const LOAD_AVG_SCALE: u64 = LOAD_AVG_MAX << LOAD_AVG_BITS;

/// The denominator of the decay factors, which are the retention of an
/// interval, `e^(-interval/τ)`, scaled to 11 bits.
///
/// The three factors and this denominator are Linux's own, so that a load
/// average here means what it means in `/proc/loadavg` there.
const LOAD_FACTOR: u64 = 2047;

/// The retention of an interval by the one minute average,
/// `e^(-5s/1min)`.
const EXP_1: u64 = 1884;

/// The retention of an interval by the five minute average,
/// `e^(-5s/5min)`.
const EXP_5: u64 = 2014;

/// The retention of an interval by the fifteen minute average,
/// `e^(-5s/15min)`.
const EXP_15: u64 = 2037;

/// The system-wide sum of the runnable tasks of every CPU over the interval
/// being accumulated.
static INTERVAL_SUM: AtomicU64 = AtomicU64::new(0);

/// The number of ticks the interval being accumulated has covered.
static INTERVAL_TICKS: AtomicU64 = AtomicU64::new(0);

/// The tick at which the current interval ends.
static INTERVAL_END: AtomicU64 = AtomicU64::new(TICKS_PER_INTERVAL);

/// The three decayed averages, as described by [`LoadAvg`].
static AVENRUN: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// Returns the load average of the system, or of the boot so far if the system
/// has not been running long enough to have an average.
pub fn loadavg() -> LoadAvg {
    LoadAvg {
        one: AVENRUN[0].load(Ordering::Relaxed),
        five: AVENRUN[1].load(Ordering::Relaxed),
        fifteen: AVENRUN[2].load(Ordering::Relaxed),
    }
}

/// Accounts one tick of a CPU that has `nr_running` runnable tasks.
///
/// Called from a timer interrupt on every CPU, so it takes no locks: the
/// counters are atomics. When an interval is over, exactly one CPU accounts it
/// — the one that claims the interval boundary — and the others carry on, so
/// concurrent ticks cannot fold the same interval twice.
pub(crate) fn on_tick(nr_running: usize) {
    let tick = Jiffies::elapsed().as_u64();

    INTERVAL_SUM.fetch_add(nr_running as u64, Ordering::Relaxed);
    INTERVAL_TICKS.fetch_add(1, Ordering::Relaxed);

    let end = INTERVAL_END.load(Ordering::Relaxed);
    if tick < end {
        return;
    }
    if INTERVAL_END
        .compare_exchange(
            end,
            next_boundary(end, tick),
            Ordering::Relaxed,
            Ordering::Relaxed,
        )
        .is_err()
    {
        // Another CPU got there first and accounts the interval.
        return;
    }

    let sum = INTERVAL_SUM.swap(0, Ordering::Relaxed);
    let ticks = INTERVAL_TICKS.swap(0, Ordering::Relaxed);
    if ticks == 0 {
        return;
    }

    // The average number of runnable tasks over the interval, in the units the
    // averages are held in.
    store(decay(
        [
            AVENRUN[0].load(Ordering::Relaxed),
            AVENRUN[1].load(Ordering::Relaxed),
            AVENRUN[2].load(Ordering::Relaxed),
        ],
        scale(sum, LOAD_AVG_SCALE, ticks),
    ));
}

/// Mixes one interval's load into the system-wide averages.
fn store(avenrun: [u64; 3]) {
    for (target, value) in AVENRUN.iter().zip(avenrun) {
        target.store(value, Ordering::Relaxed);
    }
}

/// Returns the boundary that follows `end` for a tick that arrived at `tick`.
///
/// Lining the next boundary up with the one just reached, rather than with a
/// count of intervals since boot, is what keeps a tick that arrives late from
/// shortening the interval it belongs to.
fn next_boundary(end: u64, tick: u64) -> u64 {
    let intervals = tick.saturating_sub(end) / TICKS_PER_INTERVAL + 1;
    end + intervals * TICKS_PER_INTERVAL
}

/// Returns the averages after one interval whose average load is `sample` has
/// been mixed into them.
///
/// This is the whole of the decay, kept separate from the accounting so that it
/// can be reasoned about and tested on its own.
fn decay(avenrun: [u64; 3], sample: u64) -> [u64; 3] {
    let mut decayed = [0; 3];
    for (index, out) in decayed.iter_mut().enumerate() {
        let old = avenrun[index];
        let exp = [EXP_1, EXP_5, EXP_15][index];
        // The step is towards the sample, which is usually below the average,
        // and so it is a signed quantity. Taking the difference first and
        // applying the sign afterwards is what keeps an average able to fall
        // towards an idle system rather than only ever rising.
        let difference = sample as i64 - old as i64;
        let step = scale(difference.unsigned_abs(), LOAD_FACTOR - exp, LOAD_FACTOR) as i64;
        let step = if difference < 0 { -step } else { step };
        *out = (old as i64 + step).max(0) as u64;
    }
    decayed
}

/// Returns `value * numerator / denominator`, rounded down and saturating.
fn scale(value: u64, numerator: u64, denominator: u64) -> u64 {
    let scaled = (value as u128) * (numerator as u128) / (denominator as u128);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// The load average of a system, as three exponentially decayed averages of
/// the number of runnable tasks.
///
/// The values are in units of `1 / LOAD_AVG_SCALE`, so `LOAD_AVG_SCALE` is a
/// load of one: a system with one runnable task for every CPU, and nothing else
/// to do, is as loaded as it can be. The scale is finer than the display for the
/// reason `LOAD_AVG_BITS` gives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadAvg {
    /// The one minute average.
    pub one: u64,
    /// The five minute average.
    pub five: u64,
    /// The fifteen minute average.
    pub fifteen: u64,
}

impl LoadAvg {
    /// Returns the one minute average as a floating point number.
    pub fn one_f64(&self) -> f64 {
        self.one as f64 / LOAD_AVG_SCALE as f64
    }

    /// Returns the five minute average as a floating point number.
    pub fn five_f64(&self) -> f64 {
        self.five as f64 / LOAD_AVG_SCALE as f64
    }

    /// Returns the fifteen minute average as a floating point number.
    pub fn fifteen_f64(&self) -> f64 {
        self.fifteen as f64 / LOAD_AVG_SCALE as f64
    }
}

impl core::fmt::Display for LoadAvg {
    /// Prints the three averages the way `/proc/loadavg` does, with two decimal
    /// places, as in `0.42 0.35 0.31`.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (index, value) in [self.one, self.five, self.fifteen].iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            // A thousandth of the scale, rounded rather than truncated so that
            // an average which is as close to a whole number as the fixed point
            // can hold still prints as that whole number.
            let thousandths = (((*value as u128) * 1000 + (LOAD_AVG_SCALE as u128) / 2)
                / (LOAD_AVG_SCALE as u128)) as u64;
            write!(f, "{}.{:02}", thousandths / 1000, (thousandths % 1000) / 10)?;
        }
        Ok(())
    }
}
