// SPDX-License-Identifier: GPL-2.0

//! The scheduler itself: the [`Scheduler`] that OSTD drives, one runqueue per
//! CPU, and the registry that ties a task to its scheduling state.
//!
//! OSTD does not have a scheduler of its own. It defines the
//! [`Scheduler`]/[`LocalRunQueue`] traits and leaves every decision to whoever
//! injects an implementation, which is what [`init`] does. OSTD then calls into
//! the injected scheduler at four kinds of event — a task becoming runnable, a
//! timer tick, a task yielding, and a task sleeping or exiting — and the whole
//! policy lives in [`RunQueue`](super::rq::RunQueue).
//!
//! # Events
//!
//! | Event | What the scheduler does |
//! |---|---|
//! | [`Scheduler::enqueue`] | Places the task, and asks for a preemption if the task is eligible and has an earlier deadline than the running one |
//! | [`UpdateFlags::Tick`] | Charges the running task and asks for a preemption if any waiting task should displace it |
//! | [`UpdateFlags::Yield`] | Charges the running task and gives up the CPU, which costs it one request of deadline |
//! | [`UpdateFlags::Wait`] | Charges the running task and lets it go |
//! | [`UpdateFlags::Exit`] | Charges the running task and drops its scheduling state |
//! | [`LocalRunQueue::try_pick_next`] | Picks the eligible task with the earliest deadline, or starts the idle task |
//!
//! # The bootstrap context
//!
//! The context OSTD starts the kernel in is not a task: it cannot be switched
//! away from and come back to, because there is no runqueue to put it on. It
//! therefore runs a task only when it asks to, which is what
//! [`UpdateFlags::Yield`] from OSTD's own entry point does once
//! `kernel_main` returns, and it is never preempted by a tick.
//!
//! # Locking
//!
//! One [`SpinLock`] per runqueue, taken with local interrupts disabled so that
//! a task cannot be migrated out from under a decision. The entity registry
//! below is a second lock, and the only nesting is a runqueue inside the
//! registry: the registry is looked up and released before a runqueue is
//! locked, and taken again only while a runqueue is held.
//!
//! [`SpinLock`]: ostd::sync::SpinLock

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};

use ostd::{
    cpu::{CpuId, PinCurrentCpu, num_cpus},
    sync::SpinLock,
    task::{
        Task, TaskOptions, disable_preempt, halt_cpu,
        scheduler::{
            EnqueueFlags, LocalRunQueue, Scheduler, UpdateFlags, enable_preemption_on_cpu,
            inject_scheduler,
        },
    },
    timer,
};
use spin::Once;

use super::{
    Entity, loadavg, now,
    entity::BASE_SLICE_NS,
    rq::{Entry, RunQueue},
    stats,
};

/// The scheduler that OSTD drives.
pub struct EevdfScheduler {
    /// The runqueue of each CPU.
    rqs: Box<[SpinLock<RunQueue>]>,
    /// The scheduling state of every task the scheduler knows, keyed by the
    /// address of the task.
    ///
    /// A task's `Arc` is what keeps it alive and where it lives, so its address
    /// is a key that is unique for as long as the task is. The state is not
    /// stored in the task because that would mean every task had to be built
    /// through this module, and OSTD creates tasks of its own.
    entities: SpinLock<BTreeMap<usize, Arc<Entity>>>,
}

impl EevdfScheduler {
    /// Creates a scheduler with an empty runqueue for every CPU.
    fn new() -> Self {
        let rqs = (0..num_cpus()).map(|_| SpinLock::new(RunQueue::new())).collect();
        Self {
            rqs,
            entities: SpinLock::new(BTreeMap::new()),
        }
    }

    /// Returns the runqueue of `cpu`.
    fn rq(&self, cpu: CpuId) -> &SpinLock<RunQueue> {
        &self.rqs[u32::from(cpu) as usize]
    }

    /// Makes a task runnable on the CPU that woke it, and returns that CPU if
    /// the task is worth interrupting it for.
    fn enqueue_task(
        &self,
        runnable: Arc<Task>,
        entity: Arc<Entity>,
        flags: EnqueueFlags,
    ) -> Option<CpuId> {
        // A task that OSTD woke while it was still queued belongs back on the
        // CPU it is already on. `TaskScheduleInfo::cpu` is the record of that,
        // and it is cleared when a task is dequeued, so a task that is not
        // queued has none.
        let current_cpu = disable_preempt().current_cpu();
        let (still_queued, target_cpu) =
            match runnable.schedule_info().cpu.set_if_is_none(current_cpu) {
                Ok(()) => (false, current_cpu),
                Err(on_cpu) => (true, on_cpu),
            };
        debug_assert!(
            flags != EnqueueFlags::Spawn || !still_queued,
            "a task that has just been created is not on a runqueue"
        );

        let mut rq = self.rq(target_cpu).disable_irq().lock();
        if still_queued && runnable.schedule_info().cpu.set_if_is_none(target_cpu).is_err() {
            // The task is on a runqueue already. This is the race between a
            // task being woken and it going to sleep: the waker got here
            // first, so the task is runnable and this wake is redundant.
            return None;
        }

        let is_new = !entity.is_placed();
        rq.place(Entry::new(runnable, entity.clone(), now()), is_new);

        // A preemption can only be asked for while a task is running: the
        // bootstrap context is the one thing a task cannot switch back to, so it
        // is never interrupted.
        rq.should_preempt_entity(&entity).then_some(target_cpu)
    }
}

impl Scheduler<Task> for EevdfScheduler {
    fn enqueue(&self, runnable: Arc<Task>, flags: EnqueueFlags) -> Option<CpuId> {
        let entity = entity_for(&runnable);
        self.enqueue_task(runnable, entity, flags)
    }

    fn local_rq_with(&self, f: &mut dyn FnMut(&dyn LocalRunQueue)) {
        let guard = disable_preempt();
        let rq = self.rq(guard.current_cpu()).disable_irq().lock();
        f(&*rq);
    }

    fn mut_local_rq_with(&self, f: &mut dyn FnMut(&mut dyn LocalRunQueue)) {
        let guard = disable_preempt();
        let mut rq = self.rq(guard.current_cpu()).disable_irq().lock();
        f(&mut *rq);
    }
}

impl LocalRunQueue for RunQueue {
    fn current(&self) -> Option<&Arc<Task>> {
        self.current_task()
    }

    fn update_current(&mut self, flags: UpdateFlags) -> bool {
        self.set_last_flags(flags);
        self.charge();
        match flags {
            // A tick preempts only if something waiting deserves the CPU more
            // than the running task does.
            UpdateFlags::Tick => self.should_switch(),
            // A yield hands the CPU to a different task or to nobody. The
            // running task is not dequeued, so there has to be a different task
            // to switch to, and a runnable task is never replaced by the idle
            // one.
            UpdateFlags::Yield => RunQueue::pick_next(self, true).is_some(),
            // Waiting and exiting both take the current task off the runqueue,
            // so anything left on it will run, and a CPU with nothing to run is
            // an outcome worth switching to as well.
            UpdateFlags::Wait | UpdateFlags::Exit => self.nr_queued() > 0 || self.can_idle(),
        }
    }

    fn try_pick_next(&mut self) -> Option<&Arc<Task>> {
        let yielding = self.last_flags() == UpdateFlags::Yield;
        if let Some(next) = RunQueue::pick_next(self, yielding) {
            self.install(next, true);
            return self.current_task();
        }
        // Nothing waiting deserves the CPU more than the task that has it, and
        // returning the running task would have OSTD try to switch to itself,
        // which it can only spin on. So this is `None` — except on a CPU that is
        // running no task at all, which is left to the idle task rather than to
        // nothing.
        let running = self.current_task().map(Arc::as_ptr);
        if running.is_none() {
            self.start_idling();
        }
        match self.current_task() {
            Some(current) if Some(Arc::as_ptr(current)) != running => Some(current),
            _ => None,
        }
    }

    fn dequeue_current(&mut self) -> Option<Arc<Task>> {
        RunQueue::dequeue_current(self).map(|entry| entry.into_task())
    }
}

/// The scheduler, once [`init`] has created it.
static SCHEDULER: Once<EevdfScheduler> = Once::new();

/// Makes this module the scheduler of the kernel.
///
/// Must be called before anything else uses a [`Task`], because OSTD asks the
/// scheduler what to do the moment a task is created, and because the first
/// task to be created becomes the one that gets run.
pub fn init() {
    SCHEDULER.call_once(EevdfScheduler::new);
    inject_scheduler(SCHEDULER.get().expect("just initialised"));
    init_on_cpu();
}

/// Makes the current CPU ready to be scheduled on, which is everything this
/// module needs on a CPU.
///
/// Called by [`init`] on the CPU that boots the kernel, and to be called on
/// every other CPU before it is left to its own devices. Registering a timer
/// callback twice on one CPU would account every tick twice, so this is
/// deliberately not idempotent.
pub fn init_on_cpu() {
    // Without this the running task is never charged, so its virtual runtime
    // never moves, so it never falls behind, so it never gives the CPU up.
    enable_preemption_on_cpu();
    timer::register_callback_on_cpu(|| {
        loadavg::on_tick(nr_running_on(disable_preempt().current_cpu()));
        stats::global().ticked();
    });
}

/// Creates a task of nice value `nice` and makes it runnable.
///
/// The scheduling state of the task is attached before it is run, so that it is
/// placed by the algorithm from its very first moment.
pub fn spawn(options: TaskOptions, nice: i8) -> Result<Arc<Task>, ostd::Error> {
    let task = build(options, nice)?;
    task.run();
    Ok(task)
}

/// Creates a task of nice value `nice` that wants a time slice of `slice_ns`
/// nanoseconds, and makes it runnable.
///
/// The slice is what buys latency: a task that asks for a short one gets a
/// deadline that much earlier than everyone else's, and is run next, without
/// giving up any of its share of the CPU. A task that asks for a long one is
/// run less often, but for longer.
pub fn spawn_with_slice(
    options: TaskOptions,
    nice: i8,
    slice_ns: u64,
) -> Result<Arc<Task>, ostd::Error> {
    let task = build_with_slice(options, nice, slice_ns)?;
    task.run();
    Ok(task)
}

/// Creates a task of nice value `nice` without making it runnable.
///
/// [`Task::run`] is what makes it runnable, and the task keeps its scheduling
/// state until then.
pub fn build(options: TaskOptions, nice: i8) -> Result<Arc<Task>, ostd::Error> {
    build_with_slice(options, nice, BASE_SLICE_NS)
}

/// Creates a task of nice value `nice` that wants a time slice of `slice_ns`
/// nanoseconds, without making it runnable.
pub fn build_with_slice(
    options: TaskOptions,
    nice: i8,
    slice_ns: u64,
) -> Result<Arc<Task>, ostd::Error> {
    let task = Arc::new(options.build()?);
    remember_entity(&task, Entity::with_options(nice, slice_ns));
    Ok(task)
}

/// Returns the scheduling state of `task`, if the scheduler has one.
pub fn entity_of(task: &Task) -> Option<Arc<Entity>> {
    let key = key_of(task);
    SCHEDULER
        .get()?
        .entities
        .disable_irq()
        .lock()
        .get(&key)
        .cloned()
}

/// Returns the number of runnable tasks on `cpu`, the idle task excluded.
pub fn nr_running_on(cpu: CpuId) -> usize {
    scheduler()
        .rq(cpu)
        .disable_irq()
        .lock()
        .nr_running()
}

/// Writes the contents of `cpu`'s runqueue to the kernel log.
pub(super) fn log_runqueue(cpu: CpuId) {
    scheduler().rq(cpu).disable_irq().lock().log(cpu);
}

/// Returns a snapshot of `cpu`'s statistics.
pub fn stats_of_cpu(cpu: CpuId) -> stats::CpuStatsSnapshot {
    SCHEDULER
        .get()
        .map(|scheduler| scheduler.rq(cpu).disable_irq().lock().stats().snapshot())
        .unwrap_or_default()
}

/// Returns whether every CPU in the system has nothing to run.
pub fn is_system_idle() -> bool {
    if SCHEDULER.get().is_none() {
        return true;
    }
    ostd::cpu::all_cpus().all(|cpu| nr_running_on(cpu) == 0)
}

/// Returns the scheduler, once [`init`] has created it.
fn scheduler() -> &'static EevdfScheduler {
    SCHEDULER.get().expect("the scheduler has not been initialised")
}

/// Returns the key a task's scheduling state is stored under.
fn key_of(task: &Task) -> usize {
    task as *const Task as usize
}

/// Attaches `entity` to `task`, replacing whatever state it had.
fn remember_entity(task: &Task, entity: Arc<Entity>) {
    scheduler()
        .entities
        .disable_irq()
        .lock()
        .insert(key_of(task), entity);
    stats::global().entity_created();
}

/// Returns the scheduling state of `task`, giving it a default one the first
/// time it is seen.
///
/// A task that was not built through [`build`] or [`spawn`] is one that OSTD
/// made, such as the one that runs the tests, and it gets the neutral weight.
fn entity_for(task: &Task) -> Arc<Entity> {
    let key = key_of(task);
    let mut entities = scheduler().entities.disable_irq().lock();
    if let Some(entity) = entities.get(&key) {
        return Arc::clone(entity);
    }
    let entity = Entity::new(0);
    entities.insert(key, Arc::clone(&entity));
    stats::global().entity_created();
    entity
}

/// Drops the scheduling state of a task that is exiting.
///
/// Doing nothing when this module was never initialised is right rather than
/// merely defensive: there is no registry to drop anything from, which is what
/// the test kernel of a crate without a `kernel_main` looks like.
pub(super) fn forget_entity(task: &Task) {
    let Some(scheduler) = SCHEDULER.get() else {
        return;
    };
    scheduler
        .entities
        .disable_irq()
        .lock()
        .remove(&key_of(task));
}

/// Runs while a CPU has nothing to do.
///
/// An idle task is what turns a runqueue that has run out of tasks into a CPU
/// that waits for an interrupt instead of one that spins: OSTD can only switch
/// between tasks, so something has to be running for a CPU to be able to stop
/// running.
///
/// A system that has run out of work also has nothing left to do, so an idle
/// system powers off. That is a policy for a kernel that is not running
/// processes yet, and it is what makes `cargo osdk run` finish; once there are
/// processes to run, this is where waiting for a power state change rather than
/// stopping belongs.
fn idle_task() {
    loop {
        halt_cpu();
    }
}

/// Builds the idle task of the current CPU, or returns `None` if its kernel
/// stack cannot be allocated.
///
/// Built on demand rather than at [`init`] so that a kernel which never runs out
/// of tasks does not pay for an idle task it never uses. What keeps a CPU that
/// has *never* had a task from idling is the other half of this, in
/// [`RunQueue::start_idling`](super::rq::RunQueue), which is what lets the
/// bootstrap context finish and the machine power off.
pub(super) fn idle_entry() -> Option<Entry> {
    let task = match TaskOptions::new(idle_task).build() {
        Ok(task) => Arc::new(task),
        Err(error) => {
            ostd::warn!("cannot create the idle task: {error:?}");
            return None;
        }
    };
    Some(Entry::idle(task, Entity::idle()))
}
