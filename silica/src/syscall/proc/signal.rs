// SPDX-License-Identifier: GPL-2.0

//! Signal system calls: `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn`,
//! `kill`, `tgkill` and `sigaltstack`.

use alloc::vec::Vec;
use core::mem::size_of;

use ostd::{
    arch::cpu::context::UserContext,
    mm::{FallibleVmRead, FallibleVmWrite, VmWriter},
};

use crate::{
    api::{
        errno::{Errno, Result},
        signal::{SIGSET_WORDS, SigAction, SigHandler, SigInfo, SigSet, SigStack, Signal},
    },
    proc::{
        Proc,
        thread::{Thread, ThreadFlags},
    },
};

/// The size of `struct sigaction` as user space lays it out.
///
/// A handler, a 128-byte mask, the flags, and the restorer address.
const SIGACTION_LEN: usize = 8 + SIGSET_WORDS * 8 + 4 + 4 + 8;

/// `sigset_t`, as passed to and from user space.
fn decode_sigset(buf: &[u8]) -> SigSet {
    let mut words = [0u64; SIGSET_WORDS];
    for (i, slot) in words.iter_mut().enumerate() {
        let at = i * 8;
        *slot = u64::from_le_bytes(buf[at..at + 8].try_into().expect("eight bytes"));
    }
    SigSet(words)
}

fn encode_sigset(set: &SigSet) -> [u8; SIGSET_WORDS * 8] {
    let mut buf = [0u8; SIGSET_WORDS * 8];
    for (i, &word) in set.0.iter().enumerate() {
        buf[i * 8..(i + 1) * 8].copy_from_slice(&word.to_le_bytes());
    }
    buf
}

fn decode_sigaction(buf: &[u8]) -> SigAction {
    let mut mask = [0u64; SIGSET_WORDS];
    for (i, slot) in mask.iter_mut().enumerate() {
        let at = 8 + i * 8;
        *slot = u64::from_le_bytes(buf[at..at + 8].try_into().expect("eight bytes"));
    }
    let handler = u64::from_le_bytes(buf[0..8].try_into().expect("eight bytes")) as usize;
    let flags = u32::from_le_bytes(buf[8 + SIGSET_WORDS * 8..][..4].try_into().expect("four bytes"));
    let restorer_at = 8 + SIGSET_WORDS * 8 + 8;
    let restorer =
        u64::from_le_bytes(buf[restorer_at..][..8].try_into().expect("eight bytes")) as usize;

    SigAction {
        sa_handler: if handler == 0 {
            SigHandler::Default
        } else if handler == 1 {
            SigHandler::Ignore
        } else {
            SigHandler::Handler(handler)
        },
        sa_mask: SigSet(mask),
        sa_flags: flags,
        sa_restorer: restorer,
    }
}

fn encode_sigaction(action: &SigAction) -> [u8; SIGACTION_LEN] {
    let mut buf = [0u8; SIGACTION_LEN];
    let handler = match action.sa_handler {
        SigHandler::Default => 0usize,
        SigHandler::Ignore => 1,
        SigHandler::Handler(addr) => addr,
    };
    buf[0..8].copy_from_slice(&(handler as u64).to_le_bytes());
    buf[8..8 + SIGSET_WORDS * 8].copy_from_slice(&encode_sigset(&action.sa_mask));
    let flags_at = 8 + SIGSET_WORDS * 8;
    buf[flags_at..flags_at + 4].copy_from_slice(&action.sa_flags.to_le_bytes());
    buf[flags_at + 8..flags_at + 16].copy_from_slice(&(action.sa_restorer as u64).to_le_bytes());
    buf
}

/// `rt_sigaction(2)`.
pub fn sys_rt_sigaction(
    sig: i32,
    act_ptr: usize,
    old_act_ptr: usize,
    sigsetsize: usize,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let sig = Signal::from_u32(sig as u32).ok_or(Errno::EINVAL)?;

    if sigsetsize != SIGSET_WORDS * 8 {
        // Refusing a size the kernel cannot interpret is the only safe answer:
        // anything else means reading a `sigset_t` the caller sized differently.
        return Err(Errno::EINVAL);
    }

    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let vmar = proc.vmspace();

    let mut new_action = if act_ptr == 0 {
        None
    } else {
        Some(decode_sigaction(&read_bytes(&vmar, act_ptr, SIGACTION_LEN)?))
    };

    // `sa_restorer` is ignored. Linux fills it in from the vDSO; there is no vDSO
    // here, so `pretcode` always points at the trampoline the kernel mapped into
    // the address space at exec. A program that supplies its own restorer -- or,
    // as a freestanding one must, supplies nothing -- is served either way.
    if let Some(action) = new_action {
        let mut action = action;
        action.sa_restorer = 0;
        new_action = Some(action);
    }

    let old = {
        let mut sigacts = proc.sigacts.lock();
        let old = sigacts.get(sig);
        if let Some(action) = new_action {
            sigacts.set(sig, action);
        }
        old
    };

    if old_act_ptr != 0 {
        write_bytes(&vmar, old_act_ptr, &encode_sigaction(&old))?;
    }

    Ok(SIGSET_WORDS * 8)
}

/// `rt_sigprocmask(2)`.
pub fn sys_rt_sigprocmask(
    how: i32,
    set_ptr: usize,
    old_set_ptr: usize,
    sigsetsize: usize,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let td = Thread::current().ok_or(Errno::ESRCH)?;

    if sigsetsize != SIGSET_WORDS * 8 {
        return Err(Errno::EINVAL);
    }

    let vmar = td.proc().ok_or(Errno::ESRCH)?.vmspace();

    let mut inner = td.inner.lock();
    let previous = inner.sigmask;

    // A mask that claims to block SIGKILL is a mask the kernel will not keep, so
    // the unblockable bits are dropped from what is stored as well as refused.
    let requested = if set_ptr == 0 {
        SigSet::empty()
    } else {
        decode_sigset(&read_bytes(&vmar, set_ptr, SIGSET_WORDS * 8)?)
    };

    let updated = match how {
        SIG_BLOCK => {
            let mut m = previous;
            for sig in all_signals() {
                if requested.contains(sig) {
                    m.add(sig);
                }
            }
            m
        }
        SIG_UNBLOCK => {
            let mut m = previous;
            for sig in all_signals() {
                if requested.contains(sig) {
                    m.remove(sig);
                }
            }
            m
        }
        SIG_SETMASK => requested,
        _ => return Err(Errno::EINVAL),
    };

    inner.sigmask = updated;

    if old_set_ptr != 0 {
        write_bytes(&vmar, old_set_ptr, &encode_sigset(&previous))?;
    }

    // Opening the mask may have made a queued signal deliverable, so the trap has
    // to be armed again: `handle_ast` stands down while a signal is blocked, and
    // nothing would put it back.
    inner.flags.insert(ThreadFlags::TDF_ASTPENDING);
    drop(inner);

    Ok(SIGSET_WORDS * 8)
}

/// The three `how` values of `sigprocmask`.
const SIG_BLOCK: i32 = 0;
const SIG_UNBLOCK: i32 = 1;
const SIG_SETMASK: i32 = 2;

fn all_signals() -> impl Iterator<Item = Signal> {
    (1..=crate::api::signal::SIGNAL_MAX).filter_map(Signal::from_u32)
}

/// `rt_sigreturn(2)`.
///
/// The handler has already finished; this puts the register file back the way the
/// `ucontext` says it was.
///
/// The dispatch path must not write `rax` afterwards, and [`crate::syscall`]
/// special-cases this number for exactly that reason.
pub fn sys_rt_sigreturn(ctx: &mut UserContext) -> Result<usize> {
    let td = Thread::current().ok_or(Errno::ESRCH)?;

    {
        let inner = td.inner.lock();
        if !inner.flags.contains(ThreadFlags::IN_HANDLER) {
            // A thread that is not in a handler has no frame to return from, and
            // the address it would restore is whatever it passes in.
            return Err(Errno::EFAULT);
        }
    }

    td.return_from_handler(ctx)?;
    Ok(0)
}

/// `kill(2)`.
pub fn sys_kill(pid: i32, sig: i32, _ctx: &mut UserContext) -> Result<usize> {
    let sig = Signal::from_u32(sig as u32).ok_or(Errno::EINVAL)?;

    // Signal 0 is the existence probe: it validates everything and delivers
    // nothing.
    let probe_only = sig.as_u32() == 0;

    if pid > 0 {
        let proc = crate::proc::tree::allproc_find(
            crate::proc::tree::Pid::from_u32(pid as u32).ok_or(Errno::EINVAL)?,
        )
        .ok_or(Errno::ESRCH)?;
        if probe_only {
            return Ok(0);
        }
        send_to(&proc, sig, 0)?;
        return Ok(0);
    }

    // `pid == 0` means the caller's own process group, and a negative `pid` means
    // the group led by `-pid`. It does *not* mean every process: sending to the
    // kernel's background threads because they happened to share the group is both
    // wrong and unhelpful.
    let caller = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let wanted_group = if pid == 0 {
        caller.pgid()
    } else {
        crate::proc::tree::Pid::from_u32(pid.unsigned_abs()).ok_or(Errno::ESRCH)?
    };

    let targets: Vec<_> = crate::proc::tree::allproc_iter()
        .filter(|p| p.pgid() == wanted_group)
        .collect();

    let mut found = false;
    for proc in targets.iter() {
        found |= send_to(proc, sig, caller.pid.as_u32()).is_ok();
    }
    if !found {
        return Err(Errno::ESRCH);
    }
    Ok(0)
}

fn send_to(proc: &Proc, sig: Signal, sender: u32) -> Result<()> {
    if sig.as_u32() == 0 {
        return Ok(());
    }
    proc.send_signal(SigInfo::from_user(sig, sender), sender)
}

/// `tgkill(2)`: signal one thread of a process.
pub fn sys_tgkill(tgid: i32, tid: i32, sig: i32, _ctx: &mut UserContext) -> Result<usize> {
    let sig = Signal::from_u32(sig as u32).ok_or(Errno::EINVAL)?;
    let proc = crate::proc::tree::allproc_find(
        crate::proc::tree::Pid::from_u32(tgid as u32).ok_or(Errno::EINVAL)?,
    )
    .ok_or(Errno::ESRCH)?;

    let tid = crate::proc::thread::Tid::from_u32(tid as u32).ok_or(Errno::EINVAL)?;
    let target = proc
        .inner
        .lock()
        .threads
        .iter()
        .find(|t| t.tid == tid)
        .cloned()
        .ok_or(Errno::ESRCH)?;

    target.post_signal(SigInfo::from_user(sig, Thread::current_proc().map(|p| p.pid.as_u32()).unwrap_or(0)));
    Ok(0)
}

/// `sigaltstack(2)`.
pub fn sys_sigaltstack(ss_ptr: usize, old_ss_ptr: usize, _ctx: &mut UserContext) -> Result<usize> {
    let td = Thread::current().ok_or(Errno::ESRCH)?;
    let proc = td.proc().ok_or(Errno::ESRCH)?;
    let vmar = proc.vmspace();

    let previous = td.inner.lock().altstack;

    if old_ss_ptr != 0 {
        write_bytes(&vmar, old_ss_ptr, &encode_sigstack(&previous))?;
    }

    // A null pointer disables the alternate stack, which is the documented way to
    // say so; there is no separate flag argument.
    if ss_ptr != 0 {
        let requested = decode_sigstack(&read_bytes(&vmar, ss_ptr, size_of::<SigStack>())?);
        let effective = if requested.ss_sp == 0 || requested.ss_size == 0 {
            SigStack {
                ss_flags: crate::api::signal::ss_flags::DISABLED,
                ..SigStack::default()
            }
        } else {
            requested
        };
        td.inner.lock().altstack = effective;
    }

    Ok(0)
}

fn decode_sigstack(buf: &[u8]) -> SigStack {
    SigStack {
        ss_sp: u64::from_le_bytes(buf[0..8].try_into().expect("eight bytes")) as usize,
        ss_flags: u32::from_le_bytes(buf[8..12].try_into().expect("four bytes")),
        _pad: 0,
        ss_size: u64::from_le_bytes(buf[16..24].try_into().expect("eight bytes")) as usize,
    }
}

fn encode_sigstack(stack: &SigStack) -> [u8; 24] {
    let mut buf = [0u8; 24];
    buf[0..8].copy_from_slice(&(stack.ss_sp as u64).to_le_bytes());
    buf[8..12].copy_from_slice(&stack.ss_flags.to_le_bytes());
    buf[16..24].copy_from_slice(&(stack.ss_size as u64).to_le_bytes());
    buf
}

/// `arch_prctl(2)`, for the x86-64 code word only.
///
/// This is how a process sets its TLS base. It matters more than it looks: a C
/// runtime reads its thread pointer out of `fs:0` before it does anything else,
/// so without it no libc can start.
pub fn sys_arch_prctl(option: i32, addr: usize, _ctx: &mut UserContext) -> Result<usize> {
    const ARCH_SET_FS: i32 = 0x1002;
    const ARCH_GET_FS: i32 = 0x1003;

    let td = Thread::current().ok_or(Errno::ESRCH)?;

    match option {
        ARCH_SET_FS => {
            // Fault the page in now: pointing `fs` at a page that is not resident
            // works right up until the first access and then takes SIGSEGV inside
            // code that cannot report it.
            let vmar = td.proc().ok_or(Errno::ESRCH)?.vmspace();
            vmar.populate_range(&(addr..addr + 1))?;
            td.set_fs_base(addr);
            Ok(0)
        }
        // `dispatch` writes the return value into `rax`, which is exactly where
        // this one has to go, so it needs no special case.
        ARCH_GET_FS => Ok(td.fs_base()),
        _ => Err(Errno::EINVAL),
    }
}

fn read_bytes(vmar: &crate::vm::vmar::Vmar, addr: usize, len: usize) -> Result<Vec<u8>> {
    let mut buf = alloc::vec![0u8; len];
    let mut writer = VmWriter::from(&mut buf[..]);
    let mut reader = vmar.vm_space().reader(addr, len).map_err(|_| Errno::EFAULT)?;
    reader.read_fallible(&mut writer).map_err(|_| Errno::EFAULT)?;
    Ok(buf)
}

fn write_bytes(vmar: &crate::vm::vmar::Vmar, addr: usize, bytes: &[u8]) -> Result<()> {
    let mut writer = vmar
        .vm_space()
        .writer(addr, bytes.len())
        .map_err(|_| Errno::EFAULT)?;
    let mut reader = ostd::mm::VmReader::from(bytes);
    writer
        .write_fallible(&mut reader)
        .map_err(|_| Errno::EFAULT)?;
    Ok(())
}
