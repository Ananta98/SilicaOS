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
static AVENRUN: [AtomicU64; 3] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

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
            let thousandths =
                (value * 1000 + LOAD_AVG_SCALE / 2) / LOAD_AVG_SCALE;
            write!(f, "{}.{:02}", thousandths / 1000, (thousandths % 1000) / 10)?;
        }
        Ok(())
    }
}

#[cfg(ktest)]
mod tests {
    use alloc::string::ToString;

    use ostd::prelude::ktest;

    use super::*;

    /// A load of exactly one, in the units the averages are held in.
    const ONE: u64 = LOAD_AVG_SCALE;

    /// Returns the averages after `intervals` intervals of a load of `sample`.
    fn after_intervals(intervals: u64, sample: u64) -> LoadAvg {
        let mut avenrun = [0; 3];
        for _ in 0..intervals {
            avenrun = decay(avenrun, sample);
        }
        LoadAvg {
            one: avenrun[0],
            five: avenrun[1],
            fifteen: avenrun[2],
        }
    }

    /// Returns the movement one interval of `sample` makes in the average that
    /// decays with `exp`, starting from nothing.
    fn one_step(sample: u64, exp: u64) -> u64 {
        scale(sample, LOAD_FACTOR - exp, LOAD_FACTOR)
    }

    #[ktest]
    fn a_runnable_task_converges_to_a_load_of_one() {
        // Each interval moves an average a fixed fraction of the way towards
        // the interval's load, so a system that stays at a load of one
        // converges to one rather than drifting past it.
        assert_eq!(after_intervals(1, ONE).one, one_step(ONE, EXP_1));
        for intervals in [1, 5, 60, 600] {
            let loadavg = after_intervals(intervals, ONE);
            assert!(loadavg.one <= ONE, "an average cannot exceed its load");
        }
        // Each average needs a few multiples of its own time constant, and
        // then stops short of the sample only by the size of the last step that
        // the fixed point rounds away, which is not visible in the display.
        // 3000 intervals is two and a half hours, which is several time
        // constants of the longest of them.
        let loadavg = after_intervals(3000, ONE);
        let resolution = LOAD_FACTOR / (LOAD_FACTOR - EXP_15) + 1;
        for value in [loadavg.one, loadavg.five, loadavg.fifteen] {
            assert!(
                ONE - value <= resolution,
                "got {value}, within {resolution} of {ONE}"
            );
        }
        assert_eq!(loadavg.to_string(), "1.00 1.00 1.00");
    }

    #[ktest]
    fn an_idle_system_decays_to_zero() {
        let loadavg = after_intervals(600, 0);
        assert_eq!((loadavg.one, loadavg.five, loadavg.fifteen), (0, 0, 0));
    }

    #[ktest]
    fn each_average_moves_by_its_own_time_constant() {
        // One interval at a load of one moves the one minute average by the most
        // and the fifteen minute average by the least, because the mixing
        // factor is `1 - e^(-interval/tau)`.
        let loadavg = after_intervals(1, ONE);
        assert_eq!(loadavg.one, one_step(ONE, EXP_1));
        assert_eq!(loadavg.five, one_step(ONE, EXP_5));
        assert_eq!(loadavg.fifteen, one_step(ONE, EXP_15));
        assert!(
            loadavg.one > loadavg.five && loadavg.five > loadavg.fifteen,
            "{loadavg}"
        );
        // The one minute average has moved about 8% of the way in five seconds
        // and the fifteen minute one about 0.6%, which is what those time
        // constants mean.
        assert_eq!(one_step(ONE, EXP_1) * 100 / ONE, 7);
        assert!(loadavg.one * 100 > ONE * 7, "{}", loadavg.one);
        assert!(
            loadavg.fifteen * 100 < ONE,
            "the fifteen minute average has barely moved: {}",
            loadavg.fifteen
        );

        // And when the load goes away, each average falls by its own fraction,
        // so the one minute average is the first to forget the load and the
        // fifteen minute average the last. Comparing the fractions rather than
        // the averages is what makes this about the time constants: the
        // averages start from different heights.
        let mut busy = [0; 3];
        for _ in 0..3000 {
            busy = decay(busy, ONE);
        }
        let idle = decay(busy, 0);
        for (index, name) in ["one", "five", "fifteen"].into_iter().enumerate() {
            let retention = (idle[index] as u128 * 1000 / busy[index] as u128) as u64;
            assert!(
                retention < 1000,
                "the {name} average must fall when the load goes away, {retention} per mille kept"
            );
            if index > 0 {
                let previous = (idle[index - 1] as u128 * 1000 / busy[index - 1] as u128) as u64;
                assert!(
                    retention > previous,
                    "the {name} average must fall more slowly than the shorter one: {retention} against {previous}"
                );
            }
        }
    }

    #[ktest]
    fn a_loaded_system_does_not_exceed_its_peak() {
        // The decay only ever moves an average towards the sample, so no
        // average can be higher than the largest load seen.
        let mut avenrun = [0; 3];
        for _ in 0..50 {
            avenrun = decay(avenrun, ONE * 4);
        }
        assert!(avenrun[0] <= ONE * 4, "got {}", avenrun[0]);
        // A system that has been busy stays busier-looking for a while than one
        // that has just become busy, which is the point of a decayed average.
        let mut busy = [0; 3];
        for _ in 0..12 {
            busy = decay(busy, ONE);
        }
        let fresh = after_intervals(1, ONE);
        let busy = LoadAvg {
            one: busy[0],
            five: busy[1],
            fifteen: busy[2],
        };
        assert!(busy.one > fresh.one, "{busy} against {fresh}");
    }

    #[ktest]
    fn the_display_has_two_decimal_places() {
        assert_eq!(LoadAvg::default().to_string(), "0.00 0.00 0.00");
        assert_eq!(
            LoadAvg {
                one: 0,
                // Half the scale, which is a thousandth above a load of exactly
                // a half, and still prints as one.
                five: ONE / 2 + ONE / 1000,
                fifteen: ONE * 1234,
            }
            .to_string(),
            "0.00 0.50 1234.00"
        );
        assert_eq!(
            LoadAvg {
                one: ONE,
                five: 1,
                fifteen: ONE,
            }
            .to_string(),
            "1.00 0.00 1.00"
        );
    }

    #[ktest]
    fn the_float_conversion_agrees_with_the_display() {
        let loadavg = LoadAvg {
            one: ONE,
            five: 0,
            fifteen: ONE / 2,
        };
        assert_eq!(loadavg.one_f64(), 1.0);
        assert_eq!(loadavg.five_f64(), 0.0);
        assert!((loadavg.fifteen_f64() - 0.5).abs() < 0.0001);
    }

    #[ktest]
    fn the_interval_boundary_moves_on_by_whole_intervals() {
        // An interval that has just ended is followed by one whole interval
        // later, and a tick that arrives late does not shorten the interval it
        // belongs to.
        assert_eq!(next_boundary(TICKS_PER_INTERVAL, TICKS_PER_INTERVAL), 2 * TICKS_PER_INTERVAL);
        assert_eq!(
            next_boundary(TICKS_PER_INTERVAL, 2 * TICKS_PER_INTERVAL - 1),
            2 * TICKS_PER_INTERVAL
        );
        assert_eq!(
            next_boundary(TICKS_PER_INTERVAL, 2 * TICKS_PER_INTERVAL + 1),
            3 * TICKS_PER_INTERVAL
        );
        assert_eq!(next_boundary(0, 0), TICKS_PER_INTERVAL);
        assert_eq!(
            next_boundary(0, TICKS_PER_INTERVAL * 3 + 7),
            TICKS_PER_INTERVAL * 4
        );
        // An interval is five seconds, which is five thousand ticks of a
        // thousand hertz timer.
        assert_eq!(TICKS_PER_INTERVAL, 5 * TIMER_FREQ);
    }

    #[ktest]
    fn an_interval_is_folded_into_the_averages() {
        // Nothing else accounts into these counters in a kernel that does not
        // call `init`, which is every unit test kernel, so this can drive an
        // interval end to end and see where the averages end up.
        let saved = (
            INTERVAL_SUM.load(Ordering::Relaxed),
            INTERVAL_TICKS.load(Ordering::Relaxed),
            INTERVAL_END.load(Ordering::Relaxed),
            loadavg(),
        );
        INTERVAL_SUM.store(0, Ordering::Relaxed);
        INTERVAL_TICKS.store(0, Ordering::Relaxed);
        store([0, 0, 0]);

        // An interval that has not ended is accumulated and not yet counted.
        let end = Jiffies::elapsed().as_u64() + TICKS_PER_INTERVAL;
        INTERVAL_END.store(end, Ordering::Relaxed);
        for _ in 0..3 {
            on_tick(1);
        }
        assert_eq!(loadavg(), LoadAvg::default(), "an unfinished interval is not counted");
        assert_eq!(INTERVAL_SUM.load(Ordering::Relaxed), 3);

        // One runnable task for the whole interval is a load of one, and the
        // first interval moves the one minute average about 8% of the way there.
        INTERVAL_END.store(0, Ordering::Relaxed);
        on_tick(1);
        let counted = loadavg();
        assert_eq!(counted.one, one_step(ONE, EXP_1), "{counted}");
        assert_eq!(counted.five, one_step(ONE, EXP_5), "{counted}");
        assert_eq!(counted.fifteen, one_step(ONE, EXP_15), "{counted}");

        // A system that goes idle has its averages fall back towards zero.
        for _ in 0..3 {
            INTERVAL_END.store(0, Ordering::Relaxed);
            on_tick(0);
        }
        let idling = loadavg();
        assert!(
            idling.one < one_step(ONE, EXP_1),
            "the load average fell: {idling}"
        );
        for value in [idling.one, idling.five, idling.fifteen] {
            assert!(value <= ONE, "a load average cannot be {value}");
        }

        INTERVAL_SUM.store(saved.0, Ordering::Relaxed);
        INTERVAL_TICKS.store(saved.1, Ordering::Relaxed);
        INTERVAL_END.store(saved.2, Ordering::Relaxed);
        store([saved.3.one, saved.3.five, saved.3.fifteen]);
    }
}
