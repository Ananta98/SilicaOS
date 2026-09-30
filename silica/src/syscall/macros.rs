// SPDX-License-Identifier: GPL-2.0

#[macro_export]
macro_rules! invoke_syscall_handler {
    ($handler:path, $args:expr, $ctx:expr) => { $handler($ctx) };
    ($handler:path, $args:expr, $ctx:expr, $t1:ty) => { $handler($args[0] as $t1, $ctx) };
    ($handler:path, $args:expr, $ctx:expr, $t1:ty, $t2:ty) => { $handler($args[0] as $t1, $args[1] as $t2, $ctx) };
    ($handler:path, $args:expr, $ctx:expr, $t1:ty, $t2:ty, $t3:ty) => { $handler($args[0] as $t1, $args[1] as $t2, $args[2] as $t3, $ctx) };
    ($handler:path, $args:expr, $ctx:expr, $t1:ty, $t2:ty, $t3:ty, $t4:ty) => { $handler($args[0] as $t1, $args[1] as $t2, $args[2] as $t3, $args[3] as $t4, $ctx) };
    ($handler:path, $args:expr, $ctx:expr, $t1:ty, $t2:ty, $t3:ty, $t4:ty, $t5:ty) => { $handler($args[0] as $t1, $args[1] as $t2, $args[2] as $t3, $args[3] as $t4, $args[4] as $t5, $ctx) };
    ($handler:path, $args:expr, $ctx:expr, $t1:ty, $t2:ty, $t3:ty, $t4:ty, $t5:ty, $t6:ty) => { $handler($args[0] as $t1, $args[1] as $t2, $args[2] as $t3, $args[3] as $t4, $args[4] as $t5, $args[5] as $t6, $ctx) };
}

#[macro_export]
macro_rules! impl_syscall_nums_and_dispatch_fn {
    (
        $(
            $name:ident = $num:expr => $(::)? $($handler:ident)::+ ( $($arg_ty:ty),* ) ;
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
                    $num => crate::invoke_syscall_handler!($($handler)::+, args, ctx $(, $arg_ty)*),
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
                    // Clear carry flag on success (FreeBSD/Linux ABI convention)
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
