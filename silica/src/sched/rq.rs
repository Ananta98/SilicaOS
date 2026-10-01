// SPDX-License-Identifier: GPL-2.0

//! A CPU's runqueue, which is where the EEVDF algorithm is implemented.
//!
//! A [`RunQueue`] holds every task that is runnable on one CPU: the one running
//! now in `current`, and the rest in `queued`. Together they form the set whose
//! weighted mean virtual runtime is the runqueue's virtual time, which is the
//! reference point every task's lag is measured against.
//!
//! # Choosing the next task
//!
//! [`RunQueue::best`] walks `queued` in increasing virtual deadline and returns
//! the first task that is eligible. Because the walk never looks back, that is
//! the eligible task with the earliest deadline of all, with no need to sort or
//! to search the whole queue for the best of them.
//!
//! The running task competes in the same comparison, so it keeps running for as
//! long as it stays eligible, which is until it has had its share of the CPU.
//! Nothing has to watch a slice expire to end a turn, because a turn ends when a
//! task has had enough rather than when a timer says so; what a turn is
//! measured in is the clock, since that is the only thing that makes a decision
//! possible. A task's slice does decide something: it sets how far ahead of its
//! virtual runtime the task's deadline is, and so how much CPU it may take before
//! a woken task with an earlier deadline may interrupt it.
//!
//! # Keeping the virtual time cheap
//!
//! The weighted mean is kept as two running sums: `sum_weight` and
//! `sum_offset_weighted`, the latter counting `(vruntime - min_vruntime) *
//! weight`. Measuring against `min_vruntime` rather than zero is what keeps
//! those sums inside a `u64`. Virtual runtimes themselves grow for as long as
//! the system is up, whereas the spread between two of them on one runqueue is
//! bounded by the size of a request, so the difference is the quantity that
//! stays small. `min_vruntime` is the smallest virtual runtime in the runqueue
//! and only ever moves forward.
//!
//! # Locking
//!
//! A runqueue belongs to one CPU and may only be touched while that CPU's lock
//! is held, so nothing in here needs to be atomic except the counters in
//! [`stats`](crate::sched::stats), which the statistics readers take the lock
//! to read.
//!
//! The only lock this module nests is the runqueue lock, and the scheduler's
//! entity registry, always in that order: the registry is looked up and
//! released before a runqueue is locked, and only ever taken again while a
//! runqueue is held.

use alloc::{collections::BTreeMap, sync::Arc};

use ostd::task::Task;

use ostd::{cpu::CpuId, task::scheduler::UpdateFlags};

use super::{Entity, NSEC_PER_TICK, now, stats::CpuStats};

/// A runnable task together with the scheduling state that places it in a
/// runqueue, and the tick at which it joined that runqueue.
///
/// The arrival tick is per runqueue rather than per task because a task woken
/// on one CPU and run on another is queued twice, and the time spent waiting
/// for a CPU is a property of a wait, not of a task.
#[derive(Clone)]
pub struct Entry {
    /// The task.
    pub(crate) task: Arc<Task>,
    /// The task's scheduling state.
    pub(crate) entity: Arc<Entity>,
    /// The tick at which the task joined this runqueue, which is when its
    /// wait for the CPU started.
    pub(crate) arrival: u64,
    /// Whether this is the runqueue's idle task, which runs only when nothing
    /// else can.
    pub(crate) is_idle: bool,
}

impl Entry {
    /// Creates an entry for `task` that joined the runqueue at tick `arrival`.
    pub fn new(task: Arc<Task>, entity: Arc<Entity>, arrival: u64) -> Self {
        Self {
            task,
            entity,
            arrival,
            is_idle: false,
        }
    }

    /// Creates the entry of the idle task of a CPU.
    ///
    /// The idle task is not enqueued: it is installed as the running task
    /// directly, so it must never be woken, and nothing may be scheduled in its
    /// place. That is what its weight of zero and the runqueue's exclusion of
    /// it are for.
    pub fn idle(task: Arc<Task>, entity: Arc<Entity>) -> Self {
        Self {
            task,
            entity,
            arrival: now(),
            is_idle: true,
        }
    }

    /// Returns the task.
    pub fn into_task(self) -> Arc<Task> {
        self.task
    }

    /// Returns a reference to the task.
    pub fn task(&self) -> &Arc<Task> {
        &self.task
    }

    /// Returns a reference to the entity.
    pub fn entity(&self) -> &Arc<Entity> {
        &self.entity
    }
}

/// The position of a task in a runqueue.
///
/// A runqueue is ordered by virtual deadline so that [`RunQueue::best`] can
/// stop at the first eligible task. Ties on the deadline go to the smaller
/// virtual runtime, and remaining ties to the sequence number, which makes the
/// choice deterministic even between tasks that are otherwise
/// indistinguishable.
///
/// All three fields are constant for as long as a task is in `queued`, which is
/// what lets a runqueue find a queued task by the position it was placed at: a
/// queued task's virtual runtime is never charged, and its weight and slice are
/// fixed for the life of the task.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RunKey {
    /// The task's virtual deadline.
    deadline: u64,
    /// The task's virtual runtime, which breaks deadline ties in favour of
    /// whoever has run less.
    vruntime: u64,
    /// The task's sequence number, which breaks the remaining ties.
    seq: u32,
}

impl RunKey {
    /// Returns the position of `entity`, with its deadline pushed one request
    /// further out if `yielding`.
    ///
    /// The push is how a task that asked to be switched away from loses its
    /// place: it is treated as if it had asked for one more request, which
    /// puts it behind everything that is waiting with a request of its own. It
    /// only affects the comparison, so a yield that changes nothing leaves the
    /// task exactly where it was.
    fn of(entity: &Entity, yielding: bool) -> Self {
        let deadline = entity.deadline();
        Self {
            deadline: deadline.saturating_add(if yielding { entity.request() } else { 0 }),
            vruntime: entity.vruntime(),
            seq: entity.seq(),
        }
    }
}

/// Returns whether `entity` may run while the runqueue's virtual time is
/// `avg_vruntime`.
///
/// A task is eligible while it has not received more than its share, which is
/// while its virtual runtime has not passed the runqueue's.
fn is_eligible(entity: &Entity, avg_vruntime: u64) -> bool {
    entity.vruntime() <= avg_vruntime
}

/// The runqueue of one CPU.
pub struct RunQueue {
    /// The task running on this CPU, or nothing at all while no task runs,
    /// which is the case in the bootstrap context.
    current: Option<Entry>,
    /// The runnable tasks waiting for the CPU, in order of virtual deadline.
    queued: BTreeMap<RunKey, Entry>,
    /// The sum of the weights of `current` and `queued`.
    sum_weight: u64,
    /// The sum of `(vruntime - min_vruntime) * weight` over `current` and
    /// `queued`.
    sum_offset_weighted: u64,
    /// The smallest virtual runtime in `current` and `queued`, which only ever
    /// moves forward.
    min_vruntime: u64,
    /// The tick at which the running task was last charged.
    last_update: u64,
    /// The idle task of this CPU, kept even while it is not running so that it
    /// can be run again.
    idle: Option<Entry>,
    /// Whether a task has ever been enqueued here, which tells a CPU that has
    /// work to do from one that never had any.
    has_run_tasks: bool,
    /// The event that last asked for a scheduling decision, which decides
    /// whether a pending yield counts and whether the task being removed is
    /// exiting.
    last_flags: UpdateFlags,
    /// This CPU's statistics.
    stats: CpuStats,
}

impl RunQueue {
    /// Creates an empty runqueue.
    pub const fn new() -> Self {
        Self {
            current: None,
            queued: BTreeMap::new(),
            sum_weight: 0,
            sum_offset_weighted: 0,
            min_vruntime: 0,
            last_update: 0,
            idle: None,
            has_run_tasks: false,
            last_flags: UpdateFlags::Tick,
            stats: CpuStats::new(),
        }
    }

    /// Returns the task running on this CPU, if any.
    pub fn current_task(&self) -> Option<&Arc<Task>> {
        self.current.as_ref().map(|entry| &entry.task)
    }

    /// Returns the scheduling state of the task running on this CPU, if any.
    #[cfg(ktest)]
    pub fn current_entity(&self) -> Option<&Arc<Entity>> {
        self.current.as_ref().map(|entry| &entry.entity)
    }

    /// Returns the number of runnable tasks on this CPU.
    ///
    /// The idle task is not runnable: it runs because there is nothing else to
    /// run, which is not the same as there being work. Neither is the bootstrap
    /// context, which is not a task at all.
    ///
    /// This is what the load average counts.
    pub fn nr_running(&self) -> usize {
        self.queued.len() + usize::from(self.current.as_ref().is_some_and(|entry| !entry.is_idle))
    }

    /// Returns the number of tasks waiting for the CPU, the running task
    /// excluded.
    pub fn nr_queued(&self) -> usize {
        self.queued.len()
    }

    /// Returns this CPU's statistics.
    pub fn stats(&self) -> &CpuStats {
        &self.stats
    }

    /// Returns the runqueue's virtual time: the weighted mean of the virtual
    /// runtimes of its runnable tasks.
    ///
    /// A task's lag is the difference between this and its own virtual runtime,
    /// so a task below this point has received less than its share and a task
    /// above it has received more.
    pub fn avg_vruntime(&self) -> u64 {
        if self.sum_weight == 0 {
            return self.min_vruntime;
        }
        self.min_vruntime
            .saturating_add(self.sum_offset_weighted / self.sum_weight)
    }

    /// Returns the smallest virtual runtime in the runqueue.
    pub fn min_vruntime(&self) -> u64 {
        self.min_vruntime
    }

    /// Returns whether the CPU is running its idle task.
    pub fn is_idling(&self) -> bool {
        self.current.as_ref().is_some_and(|entry| entry.is_idle)
    }

    /// Charges the running task for the time since it was last charged, so that
    /// its virtual runtime and this CPU's accounting are up to date before any
    /// decision is taken.
    pub fn charge(&mut self) {
        let now = now();
        if self.current.is_none() {
            // Nothing is running, so there is nothing to charge, but the clock
            // still has to be noted for the next task that is dispatched.
            self.last_update = now;
            return;
        }
        self.charge_until(now);
    }

    /// Charges the running task for the time that has passed since `at`, which
    /// is where the caller last left the clock.
    pub fn charge_until(&mut self, at: u64) {
        let delta = at.saturating_sub(self.last_update);
        self.last_update = at;
        self.charge_delta(delta);
    }

    /// Charges the running task `delta` ticks of time.
    ///
    /// A task of large weight advances its virtual runtime more slowly, so this
    /// is where a task is given the share of the CPU its weight earns it.
    ///
    /// The charge is made in nanoseconds rather than in ticks because the clock
    /// ticks once a millisecond and a task of the largest weight advances its
    /// virtual runtime by only `1024 / 88761` of a tick, which is nothing at all
    /// in whole ticks. Nanoseconds leave the step of a heavy task at tens of
    /// thousands, so no task is ever charged nothing for a tick of running. It
    /// is the same conversion that puts the time on a CPU into the statistics
    /// being reported in nanoseconds.
    pub fn charge_delta(&mut self, delta: u64) {
        if delta == 0 {
            return;
        }
        let Some(current) = &self.current else {
            return;
        };

        let before = current.entity.vruntime();
        let delta_ns = delta.saturating_mul(NSEC_PER_TICK);
        current.entity.charge(delta_ns);
        self.sum_offset_weighted = self.sum_offset_weighted.saturating_add(
            current
                .entity
                .vruntime()
                .saturating_sub(before)
                .saturating_mul(current.entity.weight() as u64),
        );
        if current.is_idle {
            self.stats.idled(delta_ns);
        } else {
            self.stats.ran(delta_ns);
        }
        if before == self.min_vruntime {
            self.advance_min_vruntime();
        }
    }

    /// Places `entry` in the runqueue at the position its lag earns it.
    ///
    /// A task that is coming back is given the lag it was last seen with, which
    /// is what it takes for a task that gives up the CPU to get it back and what
    /// it takes for a task that sleeps to be treated as it was rather than as a
    /// newcomer. The lag is bounded, so a task that slept for a minute does not
    /// come back owed a minute of CPU: it comes back owed what it was owed when
    /// it went to sleep, which is at most a couple of requests, and that is what
    /// keeps a task from gaming the scheduler by sleeping at the end of a good
    /// turn to escape a debt.
    ///
    /// A task the scheduler has not seen before is given a lag of zero, the
    /// neutral position: it is eligible, but it queues behind the tasks that are
    /// already behind.
    pub fn place(&mut self, entry: Entry, is_new: bool) {
        let lag = if is_new { 0 } else { entry.entity.lag() };
        // Adding a task moves the runqueue's virtual time, because the virtual
        // time is the mean of the tasks on it and there is one more of them now:
        // a task that is owed CPU is placed below the mean and raises it, and a
        // task that has had too much is placed above and lowers it. So the lag a
        // task comes back with has to be scaled by the change in the total
        // weight, or it would come back with a different lag from the one it
        // left with. This is what Linux does when it places an entity; the
        // arithmetic is the solution of `V_after = mean(V_before, v)` for `v`.
        let weight = entry.entity.weight() as i64;
        let total = self.sum_weight as i64;
        let scaled_lag = if total == 0 || weight == 0 {
            // Nothing to be late for, or a task that is not in the mean at all.
            lag
        } else {
            ((lag as i128 * (total as i128 + weight as i128)) / total as i128) as i64
        };
        let base_vruntime = self.avg_vruntime();
        let vruntime = (base_vruntime as i128 - scaled_lag as i128).max(0) as u64;

        if self.sum_weight == 0 {
            self.min_vruntime = vruntime;
            entry.entity.set_vruntime(vruntime);
        } else {
            if vruntime < self.min_vruntime {
                let diff = self.min_vruntime - vruntime;
                self.min_vruntime = vruntime;
                self.sum_offset_weighted = self
                    .sum_offset_weighted
                    .saturating_add(diff.saturating_mul(self.sum_weight));
            }
            entry.entity.set_vruntime(vruntime);
        }
        entry.entity.set_placed();
        self.insert(entry, false);
        self.has_run_tasks = true;
    }

    /// Returns the task that should run, or `None` if the running task should
    /// keep running.
    ///
    /// `None` is also the right answer when the runqueue holds nothing but the
    /// running task, which is what keeps a yield of the only runnable task from
    /// switching to itself.
    pub fn pick_next(&self, yielding: bool) -> Option<Entry> {
        let best = self.best()?;
        self.wins_over_current(best, yielding).then(|| best.clone())
    }

    /// Returns the queued task with the earliest virtual deadline among those
    /// that are eligible.
    fn best(&self) -> Option<&Entry> {
        if self.queued.is_empty() {
            return None;
        }
        let avg_vruntime = self.avg_vruntime();
        // The queue is ordered by deadline, so the first eligible task found is
        // the eligible task with the earliest deadline. Rounding in the
        // weighted mean can in principle leave nothing eligible, and a
        // runqueue with nothing eligible must still run something, so the
        // earliest deadline stands in.
        self.queued
            .iter()
            .find(|(_, entry)| is_eligible(&entry.entity, avg_vruntime))
            .map(|(_, entry)| entry)
            .or_else(|| self.queued.values().next())
    }

    /// Returns whether the CPU could start idling if it had nothing else to
    /// run, which is what makes giving up the CPU worth a switch.
    pub fn can_idle(&self) -> bool {
        self.has_run_tasks && !self.is_idling()
    }

    /// Returns whether `candidate` should displace the running task.
    fn wins_over_current(&self, candidate: &Entry, yielding: bool) -> bool {
        let Some(current) = &self.current else {
            return true;
        };
        // The idle task gives way to anything, and so does a task that has
        // received more than its share.
        if current.is_idle || !is_eligible(&current.entity, self.avg_vruntime()) {
            return true;
        }
        RunKey::of(&candidate.entity, false) < RunKey::of(&current.entity, yielding)
    }

    /// Returns whether switching away from the running task would pay off,
    /// which is what the scheduler asks on every timer tick.
    pub fn should_switch(&self) -> bool {
        if self.current.is_none() {
            // There is no running task to switch away from. This is the
            // bootstrap context, which no task can switch back to, so it is
            // never preempted.
            return false;
        }
        self.best()
            .is_some_and(|best| self.wins_over_current(best, false))
    }

    /// Returns whether the task of `entity`, which has just been enqueued, is
    /// worth interrupting the running task for.
    ///
    /// This is the same comparison a wakeup would face a moment later, so a
    /// task that asks for a preemption here will also be picked when the tick
    /// arrives. Only a task that is eligible can, because fairness comes
    /// before urgency.
    pub fn should_preempt_entity(&self, entity: &Entity) -> bool {
        let Some(current) = &self.current else {
            // Nothing is running, and the context that is running is one that
            // cannot be switched back to.
            return false;
        };
        if current.is_idle {
            return true;
        }
        is_eligible(entity, self.avg_vruntime())
            && RunKey::of(entity, false) < RunKey::of(&current.entity, false)
    }

    /// Makes `next` the running task, putting the running task back into the
    /// runqueue unless it is the idle task.
    ///
    /// `from_queue` says whether `next` is in `queued`, which it is not when it
    /// is the idle task being started.
    ///
    /// The caller must have charged the runqueue with [`RunQueue::charge`]
    /// first, because the outgoing task's accounting is folded into the switch
    /// it is about to make.
    pub fn install(&mut self, next: Entry, from_queue: bool) {
        let next = if from_queue {
            let key = RunKey::of(&next.entity, false);
            self.queued.remove(&key).unwrap_or_else(|| {
                // The runqueue lock is held for the whole decision, so a
                // task cannot leave the queue underneath it. Switching to the
                // copy in hand anyway would leave the queue holding a task
                // that is running, which would be far worse than a panic.
                panic!("the picked task is not in the runqueue");
            })
        } else {
            // The idle task is never queued and never counted: a task of weight
            // zero is left out of the weighted mean, which is exactly what an
            // idle task should be, since it competes for nothing.
            next
        };

        // Neither task moves in or out of the running sums: one was queued and
        // is now running, the other was running and is now queued.
        if let Some(mut current) = self.current.replace(next) {
            // Only a task that was running is a switch away from something, so
            // the first task a CPU runs is a dispatch and not a context switch.
            self.stats
                .switched_out(self.last_flags == UpdateFlags::Yield);
            super::stats::global().switched();
            if !current.is_idle {
                current.arrival = now();
                self.insert(current, true);
            }
        }

        let running = self.current.as_ref().expect("just replaced");
        running.entity.dispatched(now(), running.arrival);
    }

    /// Removes the running task from the runqueue and returns it.
    ///
    /// The idle task is not removed: a runqueue holding nothing but its idle
    /// task has to keep it, because the idle task is the only thing that can
    /// wait for an interrupt.
    pub fn dequeue_current(&mut self) -> Option<Entry> {
        if self.is_idling() {
            return None;
        }
        let current = self.current.take()?;
        // The record of which CPU the task is on is what says whether it is on a
        // runqueue at all, so clearing it here is what lets a later wake of this
        // task put it back on one instead of being taken for a wake of a task
        // that is already queued.
        current.task.schedule_info().cpu.set_to_none();
        self.remember_lag(&current.entity);
        self.unaccount(&current.entity);
        if self.last_flags == UpdateFlags::Exit {
            // The task is exiting and will never be enqueued again, so its
            // scheduling state is not worth keeping.
            super::adapter::forget_entity(&current.task);
        }
        Some(current)
    }

    /// Starts idling this CPU if it has work to wait for and nothing to run.
    ///
    /// Returns whether the CPU is running the idle task afterwards.
    pub fn start_idling(&mut self) -> bool {
        if self.is_idling() {
            return true;
        }
        if !self.has_run_tasks {
            // This runqueue has never held a task, so there is nothing to wait
            // for. Saying so leaves the CPU with no running task at all, which
            // is what lets the bootstrap context finish and the machine power
            // off.
            return false;
        }
        if self.idle.is_none() {
            // The idle task is built here, under the runqueue lock, rather than
            // when the CPU starts up, so that a kernel that never runs out of
            // tasks never pays for one. Allocating a kernel stack is a few
            // atomic operations on a free list, which is a fair price for that.
            let Some(idle) = super::adapter::idle_entry() else {
                return false;
            };
            self.idle = Some(idle.clone());
        }
        let idle = self.idle.clone().expect("just created");
        self.install(idle, false);
        true
    }

    /// Returns the last event that asked for a scheduling decision.
    pub fn last_flags(&self) -> UpdateFlags {
        self.last_flags
    }

    /// Records the event that asked for a scheduling decision.
    pub fn set_last_flags(&mut self, flags: UpdateFlags) {
        self.last_flags = flags;
    }

    /// Returns the tasks waiting for the CPU, earliest deadline first.
    pub fn queued_tasks(&self) -> impl Iterator<Item = (&Arc<Task>, &Arc<Entity>)> {
        self.queued
            .values()
            .map(|entry| (&entry.task, &entry.entity))
    }

    /// Writes what this runqueue holds to the kernel log.
    ///
    /// For bringing a scheduler up: where the runqueue's virtual time is, what
    /// the running task has been given, and what every waiting task is owed.
    pub(super) fn log(&self, cpu: CpuId) {
        ostd::info!(
            "  cpu{}: {} runnable, {} waiting, virtual time {} (smallest {})",
            u32::from(cpu),
            self.nr_running(),
            self.nr_queued(),
            self.avg_vruntime(),
            self.min_vruntime(),
        );
        match &self.current {
            Some(current) => ostd::info!(
                "    running {}: {}",
                if current.is_idle {
                    "the idle task"
                } else {
                    "a task"
                },
                current.entity.stats(),
            ),
            None => ostd::info!("    running: no task"),
        }
        for (task, entity) in self.queued_tasks() {
            ostd::info!("    waiting {:p}: {}", Arc::as_ptr(task), entity.stats());
        }
    }

    /// Puts `entry` into the queue at the position its deadline gives it.
    ///
    /// `counted` says whether the task is already in the running sums, which is
    /// the case for a task that is being switched away from and a task that is
    /// being switched to, and is not the case for a task that is arriving.
    fn insert(&mut self, entry: Entry, counted: bool) {
        let key = RunKey::of(&entry.entity, false);
        let entity = Arc::clone(&entry.entity);
        let previous = self.queued.insert(key, entry);
        debug_assert!(previous.is_none(), "a task is queued only once");
        if !counted {
            self.account(&entity);
        }
    }

    /// Records the lag `entity` leaves the runqueue with.
    ///
    /// The lag is bounded by a couple of requests, which is the bound Linux
    /// uses: without it a task could be owed an unbounded amount of CPU simply
    /// by going to sleep while the runqueue's virtual time ran on, and then take
    /// all of it back the moment it woke up.
    fn remember_lag(&self, entity: &Entity) {
        let lag = self.avg_vruntime() as i64 - entity.vruntime() as i64;
        let limit = 2 * entity.request() as i64;
        entity.set_lag(lag.clamp(-limit, limit));
    }

    /// Adds `entity` to the running sums.
    fn account(&mut self, entity: &Entity) {
        let weight = entity.weight() as u64;
        self.sum_weight = self.sum_weight.saturating_add(weight);
        self.sum_offset_weighted = self.sum_offset_weighted.saturating_add(
            entity
                .vruntime()
                .saturating_sub(self.min_vruntime)
                .saturating_mul(weight),
        );
    }

    /// Removes `entity` from the running sums, and moves `min_vruntime` up if
    /// it was the one that left.
    fn unaccount(&mut self, entity: &Entity) {
        let weight = entity.weight() as u64;
        let vruntime = entity.vruntime().saturating_sub(self.min_vruntime);
        self.sum_weight = self.sum_weight.saturating_sub(weight);
        self.sum_offset_weighted = self
            .sum_offset_weighted
            .saturating_sub(vruntime.saturating_mul(weight));
        if vruntime == 0 {
            self.advance_min_vruntime();
        }
    }

    /// Moves `min_vruntime` up to the smallest virtual runtime left in the
    /// runqueue, if the smallest one has left.
    fn advance_min_vruntime(&mut self) {
        let Some(smallest) = self
            .current
            .iter()
            .chain(self.queued.values())
            .map(|entry| entry.entity.vruntime())
            .min()
        else {
            return;
        };
        let advance = smallest.saturating_sub(self.min_vruntime);
        if advance == 0 {
            return;
        }
        self.min_vruntime = smallest;
        // Every offset shrinks by the same amount, so the weighted sum does
        // too. This is what keeps the sums from overflowing on a long run.
        self.sum_offset_weighted = self
            .sum_offset_weighted
            .saturating_sub(advance.saturating_mul(self.sum_weight));
    }
}
