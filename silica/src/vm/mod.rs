// SPDX-License-Identifier: GPL-2.0

//! Virtual memory management.
//!
//! An address space is a [`vmar::Vmar`]. It owns the hardware page table and the
//! list of mappings placed in it, and it is the only thing that changes either.
//!
//! # The system call surface
//!
//! The methods of [`vmar::Vmar`] mirror the system calls one for one and report
//! failures as [`errno::Errno`](crate::errno::Errno) values, so the syscall layer
//! is left with nothing but argument validation:
//!
//! | System call | Method |
//! |---|---|
//! | `mmap` | [`Vmar::mmap_anonymous`](vmar::Vmar::mmap_anonymous), [`Vmar::mmap_backed`](vmar::Vmar::mmap_backed) |
//! | `munmap` | [`Vmar::munmap`](vmar::Vmar::munmap) |
//! | `mprotect` | [`Vmar::mprotect`](vmar::Vmar::mprotect) |
//! | `mremap` | [`Vmar::mremap`](vmar::Vmar::mremap) |
//! | `madvise` | [`Vmar::madvise`](vmar::Vmar::madvise) |
//! | `sbrk` | [`Vmar::resize_mapping`](vmar::Vmar::resize_mapping) |
//! | `fork` | [`Vmar::fork_from`](vmar::Vmar::fork_from) |
//!
//! # Where an address space comes from
//!
//! Every task that has a `VmSpace` of its own holds one here, as its OSTD task
//! local data; [`current_vmar`] finds it, and the page-fault handler uses that.
//! The scheduler is responsible for installing the running task's address space
//! after every switch, which [`init`] arranges.

pub mod backing;
pub mod flags;
pub mod perms;
pub mod shm;
pub mod vmar;

use alloc::sync::Arc;

use ostd::{
    irq::InterruptLevel,
    mm::fault::inject_user_page_fault_handler,
    task::{Task, TaskOptions},
};
use ostd::arch::cpu::context::CpuException;
pub use self::vmar::{Vmar, VmarQuery, VMAR_CAP_ADDR, VMAR_LOWEST_ADDR};
use crate::errno::{Errno, Result};
use self::vmar::page_fault::PageFaultInfo;

/// Installs the hooks that the rest of the kernel relies on.
///
/// Called once during boot, before any task runs.
pub fn init() {
    // Covers the kernel reaching a user address through a fallible read or
    // write, which is how a process reads or writes another one's memory.
    inject_user_page_fault_handler(resolve_kernel_page_fault);
}

/// Returns the address space of the running task.
pub fn current_vmar() -> Result<Arc<Vmar>> {
    // `local_data` may only be read in task context, and a page fault can be
    // taken in an interrupt, so check before asking.
    if !matches!(InterruptLevel::current(), InterruptLevel::L0) {
        crate::return_errno!(EFAULT, "the address space was asked for outside task context");
    }
    let task = Task::current().ok_or(Errno::ESRCH)?;
    task.local_data()
        .downcast_ref::<Arc<Vmar>>()
        .ok_or(Errno::EINVAL)
        .cloned()
}

/// Gives a task the address space `vmar`.
///
/// Every task with its own `VmSpace` needs this, otherwise
/// [`current_vmar`] cannot find its address space and its page faults go
/// unresolved.
pub fn with_vmar(options: TaskOptions, vmar: &Arc<Vmar>) -> TaskOptions {
    options.local_data(Arc::clone(vmar))
}

/// Resolves a page fault taken while the CPU was in kernel mode, such as a
/// `process_vm_readv` of a page that is not resident yet.
///
/// OSTD signals failure with `Err(())`, so the reason has to be logged here:
/// without it the caller could only report "the access failed".
///
/// # Warning: declining is fatal
///
/// When this returns `Err(())`, the faulting access does not come back as an
/// error. OSTD's exception-table recovery cannot resume the instruction, and the
/// kernel dies without a message. That was observed directly: a kernel-mode
/// write to a read-only user page, and a read of an unmapped address, each ended
/// in an unreported reset.
///
/// The practical consequence is that every task that can reach a user address
/// must have one: see [`with_vmar`]. A task without an `Arc<Vmar>` makes
/// [`current_vmar`] fail, so every fault it takes ends here, and therefore ends
/// the kernel. This is also why no test can provoke a declined fault, and why
/// [`Vmar::handle_page_fault`] has to be called directly instead.
fn resolve_kernel_page_fault(exception: &CpuException) -> Result<(), ()> {
    let Some(info) = PageFaultInfo::from_cpu_exception(exception) else {
        return Err(());
    };
    match current_vmar().and_then(|vmar| vmar.handle_page_fault(&info.provoked())) {
        Ok(()) => Ok(()),
        Err(_) => {
            Err(())
        }
    }
}

/// Resolves a page fault taken while the CPU was in user mode.
///
/// The task calls this when `UserMode::execute` returns because of a CPU
/// exception, and turns an `Err` into a fault signal for the process.
pub fn handle_user_page_fault(exception: &CpuException) -> Result<()> {
    let Some(info) = PageFaultInfo::from_cpu_exception(exception) else {
        crate::return_errno!(EINVAL, "the exception is not a user-mode page fault");
    };
    let vmar = current_vmar()?;
    vmar.handle_page_fault(&info)
}

#[cfg(ktest)]
/// Helpers for tests that need to look at what a mapping actually contains.
///
/// Reading and writing a user address goes through the address space, so a test
/// has to install the space it is interested in first. That is what the
/// scheduler does between tasks, so these helpers are the test equivalent of a
/// context switch.
pub(crate) mod tests {
    use core::ops::Range;

    use ostd::mm::{HasPaddr, Paddr, PageFlags, Vaddr};

    use super::*;
    use crate::vm::perms::VmPerms;

    /// Stores `value` as a `u64` at `addr` in `vmar`, faulting the page in
    /// first.
    ///
    /// The page is populated explicitly because the test task has no address
    /// space of its own for the fault handler to find, so a fault raised by the
    /// write itself could not be resolved.
    ///
    /// The write is checked to be permitted before it is issued. A kernel-mode
    /// access to a user page the handler declines does not come back as an
    /// error, so finding out the hard way costs the whole run.
    pub(crate) fn poke(vmar: &Vmar, addr: Vaddr, value: u64) {
        fault_in(vmar, addr);
        assert!(
            perms_at(vmar, addr).granted().contains(VmPerms::WRITE),
            "poke at {addr:#x} needs a writable mapping, but it is {:?}",
            perms_at(vmar, addr)
        );
        vmar.activate();
        let mut writer = vmar
            .vm_space()
            .writer(addr, size_of::<u64>())
            .expect("the address is in user space");
        writer.write_val(&value).expect("the page is mapped");
    }

    /// Loads the `u64` at `addr` in `vmar`, faulting the page in first.
    pub(crate) fn peek(vmar: &Vmar, addr: Vaddr) -> u64 {
        fault_in(vmar, addr);
        assert!(
            perms_at(vmar, addr).granted().contains(VmPerms::READ),
            "peek at {addr:#x} needs a readable mapping, but it is {:?}",
            perms_at(vmar, addr)
        );
        vmar.activate();
        let mut reader = vmar
            .vm_space()
            .reader(addr, size_of::<u64>())
            .expect("the address is in user space");
        reader.read_val().expect("the page is mapped")
    }

    /// Returns the mapping that covers `addr`.
    ///
    /// A range has to be given to the query, and an empty one never matches a
    /// record, so the probe has to reach inside the address.
    pub(crate) fn record_at(vmar: &Vmar, addr: Vaddr) -> vmar::VmMapping {
        vmar.mappings_in(addr..addr + 1)
            .iter()
            .next()
            .unwrap_or_else(|| panic!("no mapping covers {addr:#x}"))
            .dup()
    }

    /// Returns the permissions of the mapping that covers `addr`.
    pub(crate) fn perms_at(vmar: &Vmar, addr: Vaddr) -> VmPerms {
        record_at(vmar, addr).perms()
    }

    /// Returns what the page table holds for the page containing `addr`: the
    /// frame's physical address and the page's flags.
    ///
    /// This is the only way to tell two things apart that the record bookkeeping
    /// cannot: whether an operation moved the *pages* or only the *records*, and
    /// whether a fault copied a page or granted permission on the existing one.
    pub(crate) fn page_at(vmar: &Vmar, addr: Vaddr) -> Option<(Paddr, PageFlags)> {
        use ostd::mm::vm_space::VmQueriedItem;

        let range = page_of(addr);
        vmar.activate();
        let guard = ostd::task::disable_preempt();
        let mut cursor = vmar
            .vm_space()
            .cursor(&guard, &range)
            .expect("the address is in user space");
        match cursor.query().expect("the range is page-aligned") {
            (_, Some(VmQueriedItem::MappedRam { frame, prop })) => Some((frame.paddr(), prop.flags)),
            (_, Some(VmQueriedItem::MappedIoMem { paddr, prop })) => Some((paddr, prop.flags)),
            (_, None) => None,
        }
    }

    /// Returns the physical address of the page containing `addr`, panicking if
    /// it is not resident.
    pub(crate) fn paddr_at(vmar: &Vmar, addr: Vaddr) -> Paddr {
        page_at(vmar, addr)
            .unwrap_or_else(|| panic!("the page at {addr:#x} is not resident"))
            .0
    }

    /// Returns whether the page containing `addr` has no page table entry.
    pub(crate) fn not_resident(vmar: &Vmar, addr: Vaddr) -> bool {
        page_at(vmar, addr).is_none()
    }

    /// Returns the range of the page containing `addr`.
    pub(crate) fn page_of(addr: Vaddr) -> Range<Vaddr> {
        let start = addr & !(ostd::mm::PAGE_SIZE - 1);
        start..start + ostd::mm::PAGE_SIZE
    }

    /// Resolves the page holding `addr` the way a real access would.
    fn fault_in(vmar: &Vmar, addr: Vaddr) {
        vmar.populate_range(&page_of(addr))
            .unwrap_or_else(|errno| panic!("no mapping holds the page at {addr:#x}: {errno}"));
    }

    /// Faults in every page of `vmar` over `range`, as a real access would.
    pub(crate) fn touch(vmar: &Vmar, range: Range<Vaddr>) -> Result<()> {
        vmar.populate_range(&range)
    }
}
