/* SPDX-License-Identifier: GPL-2.0 */
/*
 * The initial user process.
 *
 * Built by tools/build_initramfs.sh with `as` + `ld -nostdlib -no-pie` and linked
 * into the initramfs as both /init and /sbin/init. There is no libc: the program
 * is four instructions that write a greeting and exit.
 *
 * The exit matters as much as the greeting. `exit(2)` is the only path that runs
 * proc::exit::exit1, and until the process exits there is nothing to reap it and
 * no way to observe process teardown from the kernel log.
 */

    .set SYS_write, 1
    .set SYS_exit,  60
    .set STDOUT,     1

    .section .rodata
msg:
    .ascii "SilicaOS: init running, exiting cleanly\n"
    .set msg_len, . - msg

    .text
    .globl _start
_start:
    /* write(STDOUT, msg, msg_len) */
    mov $SYS_write, %rax
    mov $STDOUT,    %rdi
    lea msg(%rip),  %rsi
    mov $msg_len,   %rdx
    syscall

    /* exit(0) */
    mov $SYS_exit,  %rax
    xor %rdi,       %rdi
    syscall

    /* Reached only if exit(2) returns, which it must not. */
halt_loop:
    jmp halt_loop

    .section .note.GNU-stack,"",@progbits
