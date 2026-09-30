// SPDX-License-Identifier: GPL-2.0

#[macro_export]
macro_rules! impl_syscall_nums_and_dispatch_fn {
    (
        $(
            $name:ident = $num:expr => $handler:path ;
        )*
    ) => {
        $(
            pub const $name: usize = $num;
        )*

        pub fn dispatch(ctx: &mut ostd::arch::cpu::context::UserContext) {
            let (sys_no, args) = {
                let regs = ctx.general_regs();
                (regs.rax, [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9])
            };

            let res = match sys_no {
                $(
                    $num => $handler(&args, ctx),
                )*
                _ => {
                    ostd::warn!("Unsupported syscall number: {sys_no}");
                    Err(crate::errno::Errno::ENOSYS)
                }
            };

            let regs = ctx.general_regs_mut();
            match res {
                Ok(val) => {
                    regs.rax = val;
                    // Clear carry flag on success (FreeBSD/Linux ABI convention, depending on specific OS logic, but let's keep it similar to before)
                    regs.rflags &= !(1 << 0);
                }
                Err(err) => {
                    regs.rax = err.to_posix_raw() as usize;
                    // Set carry flag on error
                    regs.rflags |= 1 << 0;
                }
            }
        }
    };
}
