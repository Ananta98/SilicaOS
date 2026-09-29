// SPDX-License-Identifier: GPL-2.0

//! What the scheduler knows about how the system is running.
//!
//! Three levels of counters are kept, all of them plain atomics so that they
//! can be read without disturbing a runqueue:
//!
//! - [`TaskStats`] per task, held in the task's [`Entity`](super::entity::Entity).
//!   This is what a future `/proc/<pid>/schedstat` would show: the time the
//!   task spent on a CPU, the time it spent queued for one, and the number of
//!   times it was dispatched and migrated.
//! - [`CpuStats`] per CPU, owned by the CPU's runqueue.
//!   This is what a future `/proc/stat` would show per CPU: switches, split by
//!   whether the outgoing task asked for them.
//! - [`GlobalStats`], system-wide counters.
//!
//! The load average is not here but in [`loadavg`](super::loadavg), because it
//! is a derived quantity rather than a counter.

use core::sync::atomic::{AtomicU64, Ordering};

use ostd::task::Task;

/// The scheduling statistics of one task.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskStats {
    /// The nice value that the task's weight was derived from.
    pub nice: i8,
    /// The task's weight, which is `1024` for a task of nice 0.
    pub weight: u32,
    /// The time slice the task asked for, in nanoseconds.
    pub slice_ns: u64,
    /// The task's virtual runtime, in virtual nanoseconds.
    pub vruntime: u64,
    /// The task's virtual deadline, the point in virtual time by which it wants
    /// to have finished its request.
    pub deadline: u64,
    /// The time the task has spent running, in nanoseconds.
    pub exec_ns: u64,
    /// The time the task has spent waiting on a runqueue, in nanoseconds.
    pub rq_wait_ns: u64,
    /// The number of times the task has been dispatched to a CPU.
    pub nr_switches: u64,
    /// The number of times the task has been woken on a CPU other than the one
    /// it last ran on, which stays zero until there is load balancing.
    pub nr_migrations: u64,
}

impl core::fmt::Display for TaskStats {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "nice {:3} weight {:5} slice {:>9} vruntime {:>13} deadline {:>13} \
             exec {:>13} rq_wait {:>13} switches {:8} migrations {:5}",
            self.nice,
            self.weight,
            self.slice_ns,
            self.vruntime,
            self.deadline,
            self.exec_ns,
            self.rq_wait_ns,
            self.nr_switches,
            self.nr_migrations,
        )
    }
}

/// Returns the scheduling statistics of `task`, if the scheduler is tracking
/// it.
///
/// A task the scheduler has never seen has no statistics. A task whose
/// statistics are not interesting costs a registry lookup, so this is meant
/// for occasional use such as a `procfs` read.
pub fn for_task(task: &Task) -> Option<TaskStats> {
    super::entity_of(task).map(|entity| entity.stats())
}

/// The statistics of one CPU.
#[derive(Debug)]
pub struct CpuStats {
    /// The number of task switches, voluntary and involuntary together.
    nr_switches: AtomicU64,
    /// The number of switches in which the outgoing task asked to be put back
    /// in the runqueue.
    nr_voluntary_switches: AtomicU64,
    /// The number of switches in which the outgoing task was preempted.
    nr_involuntary_switches: AtomicU64,
    /// The number of tasks placed on this CPU that last ran on another one.
    ///
    /// Always zero while every CPU puts a task on the one that woke it, which
    /// is the case until there is load balancing to move work elsewhere.
    nr_migrations: AtomicU64,
    /// The time this CPU spent running tasks, in nanoseconds.
    exec_ns: AtomicU64,
    /// The time this CPU spent idle, in nanoseconds.
    idle_ns: AtomicU64,
}

impl Default for CpuStats {
    /// A set of zeroed counters.
    fn default() -> Self {
        Self::new()
    }
}

impl CpuStats {
    /// Creates a set of zeroed counters.
    pub const fn new() -> Self {
        Self {
            nr_switches: AtomicU64::new(0),
            nr_voluntary_switches: AtomicU64::new(0),
            nr_involuntary_switches: AtomicU64::new(0),
            nr_migrations: AtomicU64::new(0),
            exec_ns: AtomicU64::new(0),
            idle_ns: AtomicU64::new(0),
        }
    }

    /// Records that this CPU spent `delta_ns` nanoseconds running a task.
    pub(crate) fn ran(&self, delta_ns: u64) {
        self.exec_ns.fetch_add(delta_ns, Ordering::Relaxed);
    }

    /// Records a switch away from the task that was running, given whether it
    /// asked to be switched away from.
    ///
    /// A switch is counted even when the runqueue was empty and the idle task
    /// started, because from the point of view of the system a task stopped
    /// running either way.
    pub(crate) fn switched_out(&self, voluntary: bool) {
        self.nr_switches.fetch_add(1, Ordering::Relaxed);
        if voluntary {
            self.nr_voluntary_switches.fetch_add(1, Ordering::Relaxed);
        } else {
            self.nr_involuntary_switches
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records that this CPU spent `idle_ns` nanoseconds idling.
    pub(crate) fn idled(&self, idle_ns: u64) {
        self.idle_ns.fetch_add(idle_ns, Ordering::Relaxed);
    }

    /// Returns a snapshot of the counters.
    pub fn snapshot(&self) -> CpuStatsSnapshot {
        CpuStatsSnapshot {
            nr_switches: self.nr_switches.load(Ordering::Relaxed),
            nr_voluntary_switches: self.nr_voluntary_switches.load(Ordering::Relaxed),
            nr_involuntary_switches: self.nr_involuntary_switches.load(Ordering::Relaxed),
            nr_migrations: self.nr_migrations.load(Ordering::Relaxed),
            exec_ns: self.exec_ns.load(Ordering::Relaxed),
            idle_ns: self.idle_ns.load(Ordering::Relaxed),
        }
    }
}

/// A snapshot of the statistics of one CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuStatsSnapshot {
    /// The number of task switches, voluntary and involuntary together.
    pub nr_switches: u64,
    /// The number of switches in which the outgoing task asked to be put back
    /// in the runqueue.
    pub nr_voluntary_switches: u64,
    /// The number of switches in which the outgoing task was preempted.
    pub nr_involuntary_switches: u64,
    /// The number of tasks placed on this CPU that last ran on another one.
    pub nr_migrations: u64,
    /// The time this CPU spent running tasks, in nanoseconds.
    pub exec_ns: u64,
    /// The time this CPU spent idle, in nanoseconds.
    pub idle_ns: u64,
}

impl core::fmt::Display for CpuStatsSnapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "switches {:9} (voluntary {:9}, involuntary {:9}) \
             migrations {:5} exec {:13} idle {:13}",
            self.nr_switches,
            self.nr_voluntary_switches,
            self.nr_involuntary_switches,
            self.nr_migrations,
            self.exec_ns,
            self.idle_ns,
        )
    }
}

/// The system-wide counters.
#[derive(Debug)]
pub struct GlobalStats {
    /// The number of context switches in the system.
    ctxt: AtomicU64,
    /// The number of timer ticks the scheduler has accounted for.
    nr_ticks: AtomicU64,
    /// The number of tasks the scheduler has created state for.
    nr_entities: AtomicU64,
}

impl Default for GlobalStats {
    /// A set of zeroed counters.
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalStats {
    /// Creates a set of zeroed counters.
    pub const fn new() -> Self {
        Self {
            ctxt: AtomicU64::new(0),
            nr_ticks: AtomicU64::new(0),
            nr_entities: AtomicU64::new(0),
        }
    }

    /// Records a context switch.
    pub(crate) fn switched(&self) {
        self.ctxt.fetch_add(1, Ordering::Relaxed);
    }

    /// Records that a timer tick was accounted for.
    pub(crate) fn ticked(&self) {
        self.nr_ticks.fetch_add(1, Ordering::Relaxed);
    }

    /// Records that the scheduler started tracking a task.
    pub(crate) fn entity_created(&self) {
        self.nr_entities.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns a snapshot of the counters.
    pub fn snapshot(&self) -> GlobalStatsSnapshot {
        GlobalStatsSnapshot {
            ctxt: self.ctxt.load(Ordering::Relaxed),
            nr_ticks: self.nr_ticks.load(Ordering::Relaxed),
            nr_entities: self.nr_entities.load(Ordering::Relaxed),
        }
    }
}

/// A snapshot of the system-wide counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlobalStatsSnapshot {
    /// The number of context switches in the system.
    pub ctxt: u64,
    /// The number of timer ticks the scheduler has accounted for.
    pub nr_ticks: u64,
    /// The number of tasks the scheduler has created state for.
    pub nr_entities: u64,
}

impl core::fmt::Display for GlobalStatsSnapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "ctxt {:10} ticks {:10} tracked tasks {:5}",
            self.ctxt, self.nr_ticks, self.nr_entities
        )
    }
}

/// The system-wide counters.
static GLOBAL: GlobalStats = GlobalStats::new();

/// Returns the system-wide counters.
pub fn global() -> &'static GlobalStats {
    &GLOBAL
}

/// Returns a snapshot of the system-wide counters.
pub fn global_snapshot() -> GlobalStatsSnapshot {
    GLOBAL.snapshot()
}

/// Writes the scheduler's statistics to the kernel log.
///
/// This is the view for bringing a scheduler up: what the system as a whole has
/// done, what each CPU has done, where each runqueue's virtual time is, and the
/// load average. The tasks of a runqueue are listed with the lag each of them
/// has, which is what makes a fairness or latency problem visible.
pub fn dump() {
    ostd::info!("scheduler statistics:");
    ostd::info!("  system: {}", global_snapshot());
    ostd::info!("  load average: {}", super::loadavg::loadavg());
    for cpu in ostd::cpu::all_cpus() {
        ostd::info!("  cpu{}: {}", u32::from(cpu), super::adapter::stats_of_cpu(cpu));
        super::adapter::log_runqueue(cpu);
    }
}
