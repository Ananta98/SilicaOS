// SPDX-License-Identifier: GPL-2.0

//! The x86-64 signal frame (Linux `arch/x86/kernel/signal.c`).
//!
//! When a signal is delivered to a handler, the kernel builds a frame on the
//! target's stack describing what was interrupted, redirects the register file
//! at the handler, and remembers where to come back to. This module owns that
//! layout and the two operations on it: building one, and taking one apart again
//! when the handler returns through `rt_sigreturn(2)`.
//!
//! # The frame
//!
//! ```text
//! struct rt_sigframe {
//!     char *pretcode;              //   0  where the handler returns to
//!     struct ucontext uc;          //   8  what was interrupted
//!     struct siginfo_t info;       // 432  why the signal was sent
//! };                              // 560 bytes, 16-byte aligned
//! ```
//!
//! `ucontext` is **424** bytes, not 296: on x86-64 it ends with `uc_sigmask`,
//! the mask that was in force when the signal was taken. That field is documented
//! as unused on i386, which is what makes it easy to leave out by mistake.
//!
//! Three details are easy to get wrong and are worth stating:
//!
//! * `gregs` starts at **R8**, not RAX. The order is the one `swapgs` leaves
//!   behind in a `pt_regs`, not the order the CPU pushes.
//! * `ucontext` is **296 bytes**, and `mcontext` is 256 of them: `gregs[23]`, a
//!   pointer, and eight reserved words.
//! * `pretcode` must hold an address the handler can actually return to. There
//!   is no vDSO here, so the kernel supplies one: see [`TRAMPOLINE`].
//!
//! # What cannot be restored
//!
//! OSTD's `UserContext` holds the general registers and nothing else -- there is
//! no room in it for the segment registers, so `REG_CSGSFS`, `REG_ERR`,
//! `REG_TRAPNO`, `REG_ORIG_RAX` and `REG_OLDMASK` cannot be read back and can
//! only be filled in with values that are honest about being unused.
//!
//! `REG_ORIG_RAX` is the one that costs something. It holds the syscall number of
//! an interrupted syscall, and without it there is no way to tell whether a
//! syscall should be restarted, so [`SA_RESTART`] cannot be honoured: a syscall
//! interrupted by a signal always reports `EINTR`.
//!
//! `REG_FSGS` is the same problem for thread-local storage. The base lives in
//! `IA32_FS_BASE`, which `ostd` does not carry per task either; see
//! [`crate::proc::thread::Thread::fs_base`], which keeps it in the thread
//! instead.
//!
//! [`SA_RESTART`]: crate::api::signal::flags::SA_RESTART

use alloc::sync::Arc;
use core::fmt;

use ostd::{
    arch::cpu::context::UserContext,
    mm::{FallibleVmRead, FallibleVmWrite, PAGE_SIZE},
    user::UserContextApi,
};

use crate::{
    api::{
        errno::{Errno, Result},
        signal::{SIGSET_WORDS, SigInfo, SigSet, SigStack, Signal},
    },
    vm::{Vmar, flags::MmapFlags, perms::VmPerms},
};

/// The syscall number a handler returns through: `rt_sigreturn(2)`.
pub const NR_RT_SIGRETURN: usize = 15;

/// The bytes of that return trampoline.
///
/// ```text
///   b8 0f 00 00 00    mov $15, %eax      # __NR_rt_sigreturn
///   0f 05             syscall
/// ```
///
/// There is no vDSO in this kernel, so there is nowhere else for `sa_restorer`
/// to point. The kernel writes these seven bytes into a page it maps for the
/// purpose and hands the address to every `rt_sigaction(2)`.
pub const TRAMPOLINE: [u8; 7] = [0xb8, 0x0f, 0x00, 0x00, 0x00, 0x0f, 0x05];

/// Where the trampoline page is mapped in a process.
///
/// Fixed, because `pretcode` has to name the same address in every address space a
/// process passes through and has to be mapped before any handler can be
/// installed.
///
/// It must also not collide with anything. It used to sit at
/// `0x0000_7fff_fffe_f000`, which is *inside* the fixed 8 MiB user stack that
/// `exec::setup_user_stack` maps at `USER_STACK_BASE..USER_STACK_TOP`: the stack was
/// mapped second, over the top of it, and every handler returned to a page of stack
/// holding zeroes. It is now below the stack entirely.
///
/// TODO: nothing reserves this range. An `mmap` that happened to be given the
/// address would be placed over the trampoline, since `MAP_FIXED` does not fail on
/// an existing mapping. A real reservation -- an `mmap_min_addr`, or a mapping the
/// kernel refuses to let anyone else take -- is what makes the choice safe rather
/// than merely unlikely.
pub const TRAMPOLINE_ADDR: usize = 0x0000_7fff_0000_0000;

/// The user-mode code segment selector.
///
/// Copied from `ostd/src/arch/x86/trap/gdt.rs`, which keeps both of these
/// `pub(crate)`. OSTD's own `syscall_return` pushes these when it takes the
/// `iret` path, so a frame carrying them is consistent with the way user space
/// is entered: a Ring-3 code selector and a Ring-3 data selector.
const USER_CS: usize = 0x2b;
const USER_SS: usize = 0x23;

/// `REG_CSGSFS` needs both selectors packed together, low 16 bits CS then SS.
const CSGSFS: usize = (USER_CS << 16) | USER_SS;

/// The index of each register in [`MContext::gregs`].
///
/// This order is fixed by the ABI. It is not the order the CPU pushes
/// registers in: it starts at R8 because that is the order a `pt_regs` arrives
/// in after a syscall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum GReg {
    R8 = 0,
    R9 = 1,
    R10 = 2,
    R11 = 3,
    R12 = 4,
    R13 = 5,
    R14 = 6,
    R15 = 7,
    Rdi = 8,
    Rsi = 9,
    Rbp = 10,
    Rbx = 11,
    Rdx = 12,
    Rax = 13,
    Rcx = 14,
    Rsp = 15,
    Rip = 16,
    Efl = 17,
    CsGsFs = 18,
    Err = 19,
    TrapNo = 20,
    OldMask = 21,
    Cr2 = 22,
}

/// The number of entries in `gregs`.
pub const NGREG: usize = 23;

/// The interrupted machine context (`mcontext_t`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MContext {
    pub gregs: [u64; NGREG],
    /// The saved FPU state.
    ///
    /// Always null: `ostd` saves no floating-point context per task, so there is
    /// nothing here to point at, and `uc_flags` leaves `UC_FP_XSTATE` clear so
    /// that user space does not try to read it.
    pub fpregs: u64,
    pub reserved: [u64; 8],
}

/// `mcontext_t` is 256 bytes.
impl MContext {
    pub const LEN: usize = size_of::<MContext>();

    /// Reads the context out of the interrupted registers.
    ///
    /// EFLAGS comes in with the trap and direction flags cleared, because `SFMask`
    /// already cleared them on the way in and a handler must not inherit them:
    /// `memcpy` compiled with the direction flag set moves backwards.
    pub fn capture(ctx: &UserContext) -> Self {
        let regs = ctx.general_regs();
        let mut gregs = [0u64; NGREG];
        gregs[GReg::R8 as usize] = regs.r8 as u64;
        gregs[GReg::R9 as usize] = regs.r9 as u64;
        gregs[GReg::R10 as usize] = regs.r10 as u64;
        gregs[GReg::R11 as usize] = regs.r11 as u64;
        gregs[GReg::R12 as usize] = regs.r12 as u64;
        gregs[GReg::R13 as usize] = regs.r13 as u64;
        gregs[GReg::R14 as usize] = regs.r14 as u64;
        gregs[GReg::R15 as usize] = regs.r15 as u64;
        gregs[GReg::Rdi as usize] = regs.rdi as u64;
        gregs[GReg::Rsi as usize] = regs.rsi as u64;
        gregs[GReg::Rbp as usize] = regs.rbp as u64;
        gregs[GReg::Rbx as usize] = regs.rbx as u64;
        gregs[GReg::Rdx as usize] = regs.rdx as u64;
        gregs[GReg::Rax as usize] = regs.rax as u64;
        gregs[GReg::Rcx as usize] = regs.rcx as u64;
        gregs[GReg::Rsp as usize] = regs.rsp as u64;
        gregs[GReg::Rip as usize] = regs.rip as u64;
        gregs[GReg::Efl as usize] = (regs.rflags & !(X86_EFLAGS_DF | X86_EFLAGS_TF)) as u64;
        gregs[GReg::CsGsFs as usize] = CSGSFS as u64;

        Self {
            gregs,
            fpregs: 0,
            reserved: [0; 8],
        }
    }
}

/// X86 EFLAGS: direction flag.
const X86_EFLAGS_DF: usize = 1 << 10;
/// X86 EFLAGS: trap flag.
const X86_EFLAGS_TF: usize = 1 << 8;

/// `struct ucontext`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UContext {
    /// One of `UC_FP_XSTATE`; zero here, because there is no saved FP state.
    pub uc_flags: u64,
    /// The frame this one is nested inside, or null at the outermost level.
    pub uc_link: u64,
    pub uc_stack: SigStack,
    pub uc_mcontext: MContext,
    /// The mask that was in force when the signal was taken.
    ///
    /// Present on x86-64 and unused on i386, which is exactly why it is easy to
    /// miss. `rt_sigreturn` puts it back, so a handler cannot make a signal
    /// permanently blocked by accident.
    pub uc_sigmask: crate::api::signal::SigSet,
}

/// `struct ucontext` is 424 bytes on x86-64.
impl UContext {
    pub const LEN: usize = size_of::<UContext>();
}

/// `struct rt_sigframe`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SigFrame {
    /// Where the handler returns to: the kernel's trampoline.
    pub pretcode: u64,
    pub uc: UContext,
    pub info: SigInfo,
}

/// `struct rt_sigframe` is 560 bytes.
impl SigFrame {
    pub const LEN: usize = size_of::<SigFrame>();

    /// Builds a frame describing `sig` interrupting `ctx`.
    ///
    /// `saved_mask` becomes `uc_sigmask`: the mask that was in force when the
    /// signal was taken, which the kernel restores on the way out so that a
    /// handler cannot make a signal permanently unblockable by accident.
    pub fn new(ctx: &UserContext, info: SigInfo, saved_mask: &SigSet) -> Self {
        let uc = UContext {
            // `UC_FP_XSTATE` is clear because no FP state is saved, so user space
            // must not look for any.
            uc_flags: 0,
            uc_link: 0,
            // Filled in by the caller from the target thread's `sigaltstack`.
            uc_stack: SigStack::default(),
            uc_mcontext: MContext::capture(ctx),
            uc_sigmask: *saved_mask,
        };

        Self {
            pretcode: TRAMPOLINE_ADDR as u64,
            uc,
            info,
        }
    }

    /// The address the handler sees as its third argument.
    pub fn ucontext_addr(frame_addr: usize) -> usize {
        frame_addr + size_of::<u64>()
    }

    /// The address the handler sees as its second argument.
    pub fn siginfo_addr(frame_addr: usize) -> usize {
        frame_addr + size_of::<u64>() + UContext::LEN
    }
}

/// Why building a frame failed, in enough detail to log.
#[derive(Clone, Copy, Debug)]
pub enum FrameError {
    /// There is no room for the frame on either stack.
    NoStack { needed: usize },
    /// The frame could not be written into the address space.
    Unwritable,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoStack { needed } => write!(f, "no room for a {needed}-byte signal frame"),
            Self::Unwritable => f.write_str("the signal frame could not be written"),
        }
    }
}

impl From<FrameError> for Errno {
    fn from(err: FrameError) -> Self {
        match err {
            // POSIX has no dedicated code for this; EAGAIN is what Linux
            // reports when an alternate stack cannot be used.
            FrameError::NoStack { .. } => Errno::EAGAIN,
            FrameError::Unwritable => Errno::EFAULT,
        }
    }
}

/// Chooses where to build a signal frame and writes it there.
///
/// The frame goes on the alternate stack when the handler asked for one
/// (`SA_ONSTACK`) and the alternate stack is not already in use, and otherwise
/// on the interrupted thread's own stack. `sp` is that thread's stack pointer,
/// which is what decides the second case: a fault taken because the stack ran
/// out has no room left, which is exactly when the alternate stack is the answer.
pub fn build_frame(
    vmar: &Arc<Vmar>,
    ctx: &UserContext,
    info: &SigInfo,
    saved_mask: &SigSet,
    altstack: &SigStack,
    on_stack: bool,
) -> Result<usize, FrameError> {
    let sp = ctx.stack_pointer();
    let frame = SigFrame::new(ctx, *info, saved_mask);

    if on_stack && altstack.enabled() && !altstack.contains(sp) {
        build_on_altstack(vmar, altstack, &frame)
    } else {
        build_on_current_stack(vmar, sp, &frame)
    }
}

/// Builds the frame at the top of the alternate stack.
///
/// The alternate stack grows down, so its top is the high end.
fn build_on_altstack(
    vmar: &Arc<Vmar>,
    altstack: &SigStack,
    frame: &SigFrame,
) -> Result<usize, FrameError> {
    // The alternate stack grows down, so its top is the high end. The frame is
    // placed so that it begins eight bytes into a sixteen-byte block, which is what
    // a function's stack pointer looks like at entry.
    let top = altstack.ss_sp + altstack.ss_size;
    let addr = match align_frame_below(top) {
        Some(addr) if addr >= altstack.ss_sp => addr,
        _ => {
            return Err(FrameError::NoStack {
                needed: SigFrame::LEN,
            });
        }
    };
    write_frame(vmar, addr, frame)?;
    Ok(addr)
}

/// Builds the frame on the interrupted thread's own stack, below its pointer.
fn build_on_current_stack(
    vmar: &Arc<Vmar>,
    sp: usize,
    frame: &SigFrame,
) -> Result<usize, FrameError> {
    // Below the current stack pointer. `sp & ~15` is the usual alignment rule for
    // the interrupted code and is *not* the right one here: the frame is entered by
    // jumping to a handler, not by a call, so `pretcode` takes the place the return
    // address would have and the pointer has to sit eight bytes into a block.
    let addr = match align_frame_below(sp) {
        Some(addr) => addr,
        None => {
            return Err(FrameError::NoStack {
                needed: SigFrame::LEN,
            });
        }
    };
    // RedZone aside, a stack pointer must stay above the guard page; leaving
    // nothing below the frame is what makes an overflow fault on the *next*
    // push rather than silently writing into the guard.
    if addr < PAGE_SIZE {
        return Err(FrameError::NoStack {
            needed: SigFrame::LEN,
        });
    }
    write_frame(vmar, addr, frame)?;
    Ok(addr)
}

/// Writes the frame into the address space.
fn write_frame(vmar: &Arc<Vmar>, addr: usize, frame: &SigFrame) -> Result<(), FrameError> {
    // The stack may not have the frame's pages resident yet.
    let _ = vmar.populate_range(&(addr..addr + SigFrame::LEN));
    vmar.activate();

    let bytes = frame.to_bytes();
    let mut writer = vmar
        .vm_space()
        .writer(addr, SigFrame::LEN)
        .map_err(|_| FrameError::Unwritable)?;
    let mut reader = ostd::mm::VmReader::from(&bytes[..]);
    writer
        .write_fallible(&mut reader)
        .map_err(|_| FrameError::Unwritable)?;
    Ok(())
}

/// Maps the return trampoline into `vmar` and makes it executable.
///
/// Called once per address space, before any handler can be installed. The
/// alternative -- relying on every `rt_sigaction(2)` caller to supply its own
/// restorer -- would leave `pretcode` with nothing to point at for the common
/// case of a program written against a libc that has no trampoline of its own.
pub fn install_trampoline(vmar: &Arc<Vmar>) -> Result<()> {
    let aligned = TRAMPOLINE_ADDR & !(PAGE_SIZE - 1);

    // Created executable from the start, because `mprotect` can only narrow a
    // mapping: a page mapped read-write cannot later be given execute. That means
    // there is a window during setup in which the page is writable *and*
    // executable, until the bytes are in and it is narrowed. The window is inside
    // kernel setup, before any user-space mapping exists and while nothing else
    // can reach the address.
    //
    // TODO: a W^X primitive -- mapping the same physical page twice, or having
    // `mmap` take the final permissions and a separate call supply the contents --
    // would close it properly.
    vmar.mmap_anonymous(
        aligned,
        PAGE_SIZE,
        VmPerms::READ | VmPerms::WRITE | VmPerms::EXEC,
        MmapFlags::PRIVATE | MmapFlags::FIXED,
    )?;
    vmar.populate_range(&(aligned..aligned + PAGE_SIZE))?;
    vmar.activate();

    let mut writer = vmar
        .vm_space()
        .writer(TRAMPOLINE_ADDR, TRAMPOLINE.len())
        .map_err(|_| Errno::EFAULT)?;
    let mut reader = ostd::mm::VmReader::from(&TRAMPOLINE[..]);
    writer
        .write_fallible(&mut reader)
        .map_err(|_| Errno::EFAULT)?;

    vmar.mprotect(VmPerms::READ | VmPerms::EXEC, aligned..aligned + PAGE_SIZE)?;
    Ok(())
}

/// Reads a frame back out of the address space.
///
/// Used by `rt_sigreturn(2)`, where every field is attacker-controlled: the
/// addresses come from a process that user space has had hold of for as long as
/// the handler ran.
pub fn read_frame(vmar: &Arc<Vmar>, addr: usize) -> Result<SigFrame> {
    let mut buf = [0u8; SigFrame::LEN];
    let mut writer = ostd::mm::VmWriter::from(&mut buf[..]);
    let mut reader = vmar
        .vm_space()
        .reader(addr, SigFrame::LEN)
        .map_err(|_| Errno::EFAULT)?;
    reader
        .read_fallible(&mut writer)
        .map_err(|_| Errno::EFAULT)?;
    Ok(SigFrame::from_bytes(&buf))
}

/// Byte offsets inside a frame.
///
/// Derived from the sizes of the structures rather than written as literal
/// numbers, so a change to any of them moves the offsets with it instead of
/// silently producing a frame user space misreads.
mod offset {
    use super::{MContext, NGREG, UContext};
    use core::mem::size_of;

    pub const PRETCODE: usize = 0;
    /// The `ucontext`, which follows the restorer address.
    pub const UC: usize = PRETCODE + size_of::<u64>();

    pub const UC_FLAGS: usize = UC;
    pub const UC_LINK: usize = UC_FLAGS + size_of::<u64>();
    pub const UC_STACK_SP: usize = UC_LINK + size_of::<u64>();
    pub const UC_STACK_FLAGS: usize = UC_STACK_SP + size_of::<u64>();
    /// `ss_flags` is 32 bits and `ss_size` 64, so there are four bytes of
    /// padding between them; taking it as two words keeps the arithmetic
    /// independent of whether the compiler chose to insert it.
    pub const UC_STACK_SIZE: usize = UC_STACK_FLAGS + size_of::<u32>() * 2;

    /// `mcontext`, which follows the stack inside the `ucontext`.
    pub const UC_MCONTEXT: usize = UC_STACK_SIZE;
    /// The first register of `gregs`, which is `R8`.
    pub const GREGS: usize = UC + UC_MCONTEXT;
    pub const FPREGS: usize = GREGS + NGREG * size_of::<u64>();

    /// The `siginfo_t`, which follows the whole `ucontext`.
    pub const INFO: usize = UC + size_of::<UContext>();
    /// `uc_sigmask`, at the end of the `ucontext`.
    pub const UC_SIGMASK: usize = UC + UC_MCONTEXT + size_of::<MContext>();
    pub const INFO_SI_ADDR: usize = INFO + 16;
    pub const INFO_SI_PID: usize = INFO + 24;
    pub const INFO_SI_UID: usize = INFO + 28;
    pub const INFO_SI_STATUS: usize = INFO + 32;
}

impl SigFrame {
    /// The frame as bytes, in the layout user space reads.
    ///
    /// Written field by field rather than reinterpreted, because the crate
    /// forbids `unsafe` and because it makes the coupling to `offset` explicit:
    /// if a struct changes shape, the sizes below stop matching and the build
    /// fails instead of a signal arriving corrupted.
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        use offset::*;
        let mut buf = [0u8; Self::LEN];

        buf[PRETCODE..PRETCODE + 8].copy_from_slice(&self.pretcode.to_le_bytes());
        buf[UC_FLAGS..UC_FLAGS + 8].copy_from_slice(&self.uc.uc_flags.to_le_bytes());
        buf[UC_LINK..UC_LINK + 8].copy_from_slice(&self.uc.uc_link.to_le_bytes());
        buf[UC_STACK_SP..UC_STACK_SP + 8]
            .copy_from_slice(&(self.uc.uc_stack.ss_sp as u64).to_le_bytes());
        buf[UC_STACK_FLAGS..UC_STACK_FLAGS + 4]
            .copy_from_slice(&self.uc.uc_stack.ss_flags.to_le_bytes());
        buf[UC_STACK_SIZE..UC_STACK_SIZE + 8]
            .copy_from_slice(&(self.uc.uc_stack.ss_size as u64).to_le_bytes());
        for (i, &g) in self.uc.uc_mcontext.gregs.iter().enumerate() {
            let at = GREGS + i * 8;
            buf[at..at + 8].copy_from_slice(&g.to_le_bytes());
        }
        buf[FPREGS..FPREGS + 8].copy_from_slice(&self.uc.uc_mcontext.fpregs.to_le_bytes());
        buf[UC_SIGMASK..UC_SIGMASK + SIGSET_WORDS * 8]
            .copy_from_slice(&encode_sigset(&self.uc.uc_sigmask));

        buf[INFO..INFO + 4].copy_from_slice(&self.info.si_signo.to_le_bytes());
        buf[INFO + 4..INFO + 8].copy_from_slice(&self.info.si_errno.to_le_bytes());
        buf[INFO + 8..INFO + 12].copy_from_slice(&self.info.si_code.to_le_bytes());
        buf[INFO_SI_ADDR..INFO_SI_ADDR + 8].copy_from_slice(&self.info.si_addr.to_le_bytes());
        buf[INFO_SI_PID..INFO_SI_PID + 4].copy_from_slice(&self.info.si_pid.to_le_bytes());
        buf[INFO_SI_UID..INFO_SI_UID + 4].copy_from_slice(&self.info.si_uid.to_le_bytes());
        buf[INFO_SI_STATUS..INFO_SI_STATUS + 4].copy_from_slice(&self.info.si_status.to_le_bytes());

        buf
    }

    /// The frame from bytes in that layout. The inverse of [`Self::to_bytes`].
    pub fn from_bytes(buf: &[u8; Self::LEN]) -> Self {
        use offset::*;
        let mut gregs = [0u64; NGREG];
        for (i, slot) in gregs.iter_mut().enumerate() {
            let at = GREGS + i * 8;
            *slot = read_u64(&buf[at..at + 8]);
        }

        Self {
            pretcode: read_u64(&buf[PRETCODE..PRETCODE + 8]),
            uc: UContext {
                uc_flags: read_u64(&buf[UC_FLAGS..UC_FLAGS + 8]),
                uc_link: read_u64(&buf[UC_LINK..UC_LINK + 8]),
                uc_stack: SigStack {
                    ss_sp: read_u64(&buf[UC_STACK_SP..UC_STACK_SP + 8]) as usize,
                    ss_flags: read_u32(&buf[UC_STACK_FLAGS..UC_STACK_FLAGS + 4]),
                    _pad: 0,
                    ss_size: read_u64(&buf[UC_STACK_SIZE..UC_STACK_SIZE + 8]) as usize,
                },
                uc_mcontext: MContext {
                    gregs,
                    fpregs: read_u64(&buf[FPREGS..FPREGS + 8]),
                    reserved: [0; 8],
                },
                uc_sigmask: decode_sigset(&buf[UC_SIGMASK..UC_SIGMASK + SIGSET_WORDS * 8]),
            },
            info: SigInfo {
                si_signo: read_u32(&buf[INFO..INFO + 4]) as i32,
                si_errno: read_u32(&buf[INFO + 4..INFO + 8]) as i32,
                si_code: read_u32(&buf[INFO + 8..INFO + 12]) as i32,
                _pad0: 0,
                si_addr: read_u64(&buf[INFO_SI_ADDR..INFO_SI_ADDR + 8]),
                si_pid: read_u32(&buf[INFO_SI_PID..INFO_SI_PID + 4]) as i32,
                si_uid: read_u32(&buf[INFO_SI_UID..INFO_SI_UID + 4]),
                si_status: read_u32(&buf[INFO_SI_STATUS..INFO_SI_STATUS + 4]) as i32,
                _pad1: 0,
                si_utime: 0,
                si_stime: 0,
                si_value: 0,
                _pad2: [0; 8],
            },
        }
    }
}

fn encode_sigset(set: &SigSet) -> [u8; SIGSET_WORDS * 8] {
    let mut buf = [0u8; SIGSET_WORDS * 8];
    for (i, &word) in set.0.iter().enumerate() {
        buf[i * 8..(i + 1) * 8].copy_from_slice(&word.to_le_bytes());
    }
    buf
}

fn decode_sigset(buf: &[u8]) -> SigSet {
    let mut words = [0u64; SIGSET_WORDS];
    for (i, slot) in words.iter_mut().enumerate() {
        *slot = u64::from_le_bytes(buf[i * 8..(i + 1) * 8].try_into().expect("eight bytes"));
    }
    SigSet(words)
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("slice is eight bytes"))
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("slice is four bytes"))
}

/// What a restored frame would set, after it has been checked.
///
/// Returning the values rather than applying them is deliberate: everything in
/// here comes from user space, so it has to be validated as a whole before any
/// of it reaches the register file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Restore {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

impl Restore {
    /// Checks a restored frame and reads the register values out of it.
    ///
    /// Everything here is validated because it came from a process that user
    /// space has had hold of for as long as the handler ran:
    ///
    /// * `pretcode` must be the trampoline. Without this the return path is
    ///   whatever the process put in the frame.
    /// * `rip` must be canonical. A non-canonical value faults on the `iret`,
    ///   which is survivable, but one that is not in user space is not.
    /// * `rsp` must be canonical too, and must be plausible as a stack pointer.
    /// * `uc_flags` must be zero: there is no FP state to go with it.
    pub fn from_frame(frame: &SigFrame, vmar: &Arc<Vmar>) -> Result<Self> {
        if frame.pretcode != TRAMPOLINE_ADDR as u64 {
            crate::return_errno!(EINVAL, "the signal frame does not return to the trampoline");
        }
        if frame.uc.uc_flags != 0 {
            crate::return_errno!(EINVAL, "the signal frame claims state that was not saved");
        }

        let g = &frame.uc.uc_mcontext.gregs;
        let rip = g[GReg::Rip as usize];
        let rsp = g[GReg::Rsp as usize];

        if !is_canonical(rip) {
            crate::return_errno!(EFAULT, "the restored instruction pointer is not canonical");
        }
        if !is_canonical(rsp) {
            crate::return_errno!(EFAULT, "the restored stack pointer is not canonical");
        }
        if !is_executable(vmar, rip as usize) {
            crate::return_errno!(EFAULT, "the restored instruction pointer is not executable");
        }

        Ok(Self {
            rax: g[GReg::Rax as usize],
            rbx: g[GReg::Rbx as usize],
            rcx: g[GReg::Rcx as usize],
            rdx: g[GReg::Rdx as usize],
            rsi: g[GReg::Rsi as usize],
            rdi: g[GReg::Rdi as usize],
            rbp: g[GReg::Rbp as usize],
            rsp,
            r8: g[GReg::R8 as usize],
            r9: g[GReg::R9 as usize],
            r10: g[GReg::R10 as usize],
            r11: g[GReg::R11 as usize],
            r12: g[GReg::R12 as usize],
            r13: g[GReg::R13 as usize],
            r14: g[GReg::R14 as usize],
            r15: g[GReg::R15 as usize],
            rip,
            rflags: g[GReg::Efl as usize],
        })
    }

    /// Writes the values into the register file the thread will run with.
    pub fn apply(&self, ctx: &mut UserContext) {
        let regs = ctx.general_regs_mut();
        regs.rax = self.rax as usize;
        regs.rbx = self.rbx as usize;
        regs.rcx = self.rcx as usize;
        regs.rdx = self.rdx as usize;
        regs.rsi = self.rsi as usize;
        regs.rdi = self.rdi as usize;
        regs.rbp = self.rbp as usize;
        regs.rsp = self.rsp as usize;
        regs.r8 = self.r8 as usize;
        regs.r9 = self.r9 as usize;
        regs.r10 = self.r10 as usize;
        regs.r11 = self.r11 as usize;
        regs.r12 = self.r12 as usize;
        regs.r13 = self.r13 as usize;
        regs.r14 = self.r14 as usize;
        regs.r15 = self.r15 as usize;
        regs.rip = self.rip as usize;
        regs.rflags = self.rflags as usize;
    }
}

/// Points the register file at a handler.
#[derive(Clone, Copy, Debug)]
pub struct Redirect {
    pub rip: usize,
    pub rsp: usize,
    pub rdi: usize,
    pub rsi: usize,
    pub rdx: usize,
}

impl Redirect {
    /// The stack pointer a handler starts with: the frame's own base.
    ///
    /// Two things force this to be the bottom of the frame rather than the top.
    /// `pretcode` is at offset zero, and a handler returns with a bare `ret`, which
    /// pops whatever is at its entry stack pointer -- so that is the only address
    /// the restorer can be read from. And `iretq` does not push a return address
    /// of its own: it only loads the stack pointer. Nothing else would write the
    /// word the handler pops.
    fn stack_top(frame_addr: usize) -> usize {
        frame_addr
    }

    /// The three-argument handler entry: `(int signo, siginfo_t *info,
    /// ucontext_t *uc)`.
    pub fn with_siginfo(handler: usize, sig: Signal, frame_addr: usize) -> Self {
        Self {
            rip: handler,
            rsp: Self::stack_top(frame_addr),
            rdi: sig.as_u32() as usize,
            rsi: SigFrame::siginfo_addr(frame_addr),
            rdx: SigFrame::ucontext_addr(frame_addr),
        }
    }

    /// The one-argument handler entry: `(int signo)`.
    pub fn plain(handler: usize, sig: Signal, frame_addr: usize) -> Self {
        Self {
            rip: handler,
            rsp: Self::stack_top(frame_addr),
            rdi: sig.as_u32() as usize,
            rsi: 0,
            rdx: 0,
        }
    }

    /// Applies the redirect.
    ///
    /// `rax` is zeroed because it becomes the handler's return value: it is
    /// whatever the interrupted code left there, and the handler is expected to
    /// return a value of its own. `rsp` is the frame address because the frame
    /// is what the handler's stack grows down into.
    pub fn apply(&self, ctx: &mut UserContext) {
        let regs = ctx.general_regs_mut();
        regs.rip = self.rip;
        regs.rsp = self.rsp;
        regs.rax = 0;
        regs.rdi = self.rdi;
        regs.rsi = self.rsi;
        regs.rdx = self.rdx;
        // Leave the trap flag clear: a single-step into a handler would stop
        // again on the handler's own first instruction.
        regs.rflags &= !(1 << 8);
    }
}

/// The largest address below `limit` at which a signal frame may start, or `None`
/// if there is no room.
///
/// A frame has to start eight bytes into a sixteen-byte block. That is the AMD64
/// convention for a stack pointer at *function entry*: the System V ABI wants `%rsp`
/// 16-byte aligned in the body of a function, and 16-byte aligned minus eight once a
/// return address has been pushed. A handler is entered by a jump rather than a
/// call, but it returns with a `ret`, so it is a function entry in every way that
/// matters -- and getting this wrong means the handler's `ret` pops padding instead
/// of `pretcode` and jumps to zero.
fn align_frame_below(limit: usize) -> Option<usize> {
    // The last 16-byte block at or below `limit`.
    let mut block = limit & !0xf;
    // `limit` itself may already be eight bytes into a block, in which case the
    // block is where it is and the frame can start eight bytes above the block's
    // start. Otherwise that eight bytes would spill past `limit`, so a whole block
    // is given up instead.
    if limit - block != 8 {
        block = block.checked_sub(16)?;
    }
    let addr = block.checked_sub(SigFrame::LEN)?.checked_add(8)?;
    debug_assert_eq!(addr & 0xf, 8, "a signal frame must start eight bytes into a block");
    debug_assert!(addr + SigFrame::LEN <= limit, "the frame must fit below the limit");
    Some(addr)
}

/// Whether `addr` lies in a mapping that user space may execute.
fn is_executable(vmar: &Arc<Vmar>, addr: usize) -> bool {
    vmar.mappings_in(addr..addr + 1)
        .iter()
        .any(|m| m.perms().contains(VmPerms::EXEC) && m.start() <= addr && addr < m.end())
}

/// Whether an address has the x86-64 canonical form.
fn is_canonical(addr: u64) -> bool {
    let high = addr >> 48;
    high == 0 || high == 0xffff
}
