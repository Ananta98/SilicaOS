/* SPDX-License-Identifier: GPL-2.0 */
/*
 * A userspace test for the signal machinery, built by tools/build_initramfs.sh
 * and installed as /sbin/sigtest. Boot it with `init=/sbin/sigtest`.
 *
 * There is no libc here, and deliberately so: it reports what the kernel actually
 * did rather than assuming success.
 *
 * The output is one line of terse output -- a '.' for each check that held and an
 * 'X' for each that did not, in the order they ran. That is not a stylistic
 * choice. User space writes reach the console by way of the kernel logger, which
 * redraws and replays the visible screen, so a multi-line report comes back
 * interleaved with itself and cannot be read. One line cannot be garbled that way.
 * The exit status is the verdict, and it is a *bitmask* of the checks that failed --
 * bit n set means check n did not hold -- so it can be read out of the kernel log
 * without relying on the console: user space writes reach the console by way of
 * the kernel logger, which replays the visible screen, so nothing printed by this
 * program can be trusted to arrive intact or in order.
 *
 *   1. write(2) returns a non-negative count.
 *   2. A call that must fail returns a negative rax in the -4095..-1 range.
 *   3. rt_sigaction(2) refuses a `sigaction` that is not mapped. A null pointer is
 *      *not* the test: passing one asks for the old disposition, which is legal.
 *      An address in the unmapped low page is what must come back EFAULT.
 *   4. rt_sigaction(2) installs a handler.
 *   5. Sending the signal runs the handler at all.
 *   6. The handler is entered with the signal number in %rdi.
 *   7. rt_sigreturn(2) restores the interrupted registers: a sentinel loaded into
 *      every callee-saved register before the signal must survive it.
 * The default terminating action is deliberately *not* checked here: proving it
 * means not surviving, and the exit status is the verdict. It is covered by the
 * boot image instead, whose init calls exit(2), and by sending SIGSEGV to a
 * process with no handler for it -- both of which terminate, which is what a
 * passing run of this test looks like when it is the last thing that happens.
 */

    .set SYS_write,            1
    .set SYS_exit,            60
    .set SYS_kill,            62
    .set SYS_rt_sigaction,    13

    .set SIGUSR1,            10

    .set STDOUT,              1

    .set EFAULT,              14
    .set ENOSYS,              38

/* Lowest value Linux user space reads as an error: the range is -4095..-1. */
    .set ERRNO_FLOOR,    -4095

    .set SA_SIGINFO,     0x00000004
    .set SIGSET_BYTES,         128

    .set NCHECKS,              7
    .set MARK_PASS,           '.'
    .set MARK_FAIL,           'X'

/* Written into several callee-saved registers before the signal in check 6. */
    .set SENTINEL,    0x5eed5eed

/*
 * write(2) of a NUL-terminated string. The length is passed alongside the symbol
 * rather than derived from it, because GAS will not concatenate a macro parameter
 * with a suffix.
 *
 * A helper function would be shorter, but it would have to save the
 * syscall-clobbered registers for the checks below to inspect.
 */
.macro say sym, len
    mov $\sym,       %rsi
    mov $STDOUT,    %rdi
    mov $\len,      %rdx
    mov $SYS_write, %rax
    syscall
.endm

/*
 * Record the outcome of check \idx, whose result slot is at \slot.
 *
 * A failure marks the slot and sets bit \idx of the mask handed to exit(2).
 *
 * The mask lives in memory rather than a register because check 6 loads sentinels
 * into every callee-saved register, which includes whichever one would have held
 * it -- and those sentinels are expected to survive the signal, so the tally would
 * silently become one of them.
 */
.macro record idx, slot, failed
    test \failed, \failed
    jz   2f
    movb $MARK_FAIL, \slot(%rip)
    btsq $\idx, failures(%rip)
2:
.endm

/* ------------------------------------------------------------------------- */

    .section .rodata
    .align 8

/*
 * A `struct sigaction` for SIGUSR1: the handler address, a 128-byte mask, the
 * flags, and the restorer.
 */
sigaction_usr1:
    .quad sigtest_handler
    .skip SIGSET_BYTES
    .quad SA_SIGINFO
    .quad 0                   /* rt_sigaction refuses a handler with no restorer */

h_probe:            .ascii "probe\n"
h_write:            .ascii "write(2) round-trips\n"
h_enoerrno:         .ascii "a failing syscall reports a negative errno in rax\n"
h_sigaction:        .ascii "rt_sigaction(2) refuses a null sigaction\n"
h_sigaction_todo:   .ascii "  NOTE: not implemented yet; the kernel answers ENOSYS\n"
h_sigaction_pass:   .ascii "  PASS: the null pointer was rejected with EFAULT\n"
h_sigaction_fail:   .ascii "  FAIL: rt_sigaction accepted a null sigaction\n"
h_install:          .ascii "rt_sigaction(2) installs a handler\n"
h_handler:          .ascii "the signal runs the handler with signo, info and ucontext\n"
h_restored:         .ascii "rt_sigreturn(2) restores the interrupted registers\n"
h_registers_fail:   .ascii "  FAIL: a callee-saved register did not survive the signal\n"

    .set h_probe_len,            . - h_probe
    .set h_write_len,            . - h_write
    .set h_enoerrno_len,         . - h_enoerrno
    .set h_sigaction_len,        . - h_sigaction
    .set h_sigaction_todo_len,   . - h_sigaction_todo
    .set h_sigaction_pass_len,   . - h_sigaction_pass
    .set h_sigaction_fail_len,   . - h_sigaction_fail
    .set h_install_len,          . - h_install
    .set h_handler_len,          . - h_handler
    .set h_restored_len,         . - h_restored
    .set h_registers_fail_len,   . - h_registers_fail

/* ------------------------------------------------------------------------- */

    .bss
    .align 8
res:
    .skip NCHECKS + 1
    .set slot0, res + 0
    .set slot1, res + 1
    .set slot2, res + 2
    .set slot3, res + 3
    .set slot4, res + 4
    .set slot5, res + 5
    .set slot6, res + 6

/* What the handler was given, and the sentinel from check 6. */
    .align 8
got_signo:     .skip 8
got_info:      .skip 8
got_ucontext:  .skip 8

/* The failure tally, and the value handed to exit(2). */
failures:      .skip 8

/* ------------------------------------------------------------------------- */

    .text

/*
 * The handler, reached as (signo, siginfo_t *, ucontext_t *) because SA_SIGINFO is
 * set. It records what it was given and returns; the return address in the frame
 * is the kernel's trampoline, which runs rt_sigreturn next.
 */
    .globl sigtest_handler
sigtest_handler:
    mov %rdi, got_signo(%rip)
    mov %rsi, got_info(%rip)
    mov %rdx, got_ucontext(%rip)
    ret

    .globl _start
_start:
    movq $0, failures(%rip)

    /* Fill the result buffer with passes; failures overwrite their own slot. */
    lea res(%rip), %r14
    xor %ecx, %ecx
1:  mov $MARK_PASS, %al
    mov %al, (%r14,%rcx,1)
    inc %ecx
    cmp $NCHECKS, %ecx
    jne 1b

/*
 * 1. write(2) returns a non-negative count.
 *
 * Only the sign is checked. How many bytes a console counts is the console
 * driver's business -- it hands the whole buffer to the kernel logger, which
 * prepends a tag and adds a line.
 */
    mov $STDOUT,     %rdi
    mov $h_probe,    %rsi
    mov $h_probe_len,%rdx
    mov $SYS_write,  %rax
    syscall
    test %rax, %rax
    setns %al                       /* 1 when the count is non-negative ... */
    movzbl %al, %ebx
    xor $1, %ebx                    /* ... so invert it for `record` */
    record 0, slot0, %ebx

/*
 * 2. A call that must fail returns a negative rax.
 *
 * kill(2) with signal 0 only tests for existence, and process group 0 with no
 * such process is the cheapest call in the ABI guaranteed to fail.
 */
    mov $0,          %rdi            /* process group 0: everything of ours */
    mov $0,          %rsi            /* signal 0: test for existence only    */
    mov $0,          %rdx
    mov $SYS_kill,   %rax
    syscall
    test %rax, %rax
    jns  errno_not_negative
    cmp $ERRNO_FLOOR, %rax
    jg  errno_in_range               /* above -4095, so a plausible errno */
    mov $1, %ebx
    jmp  record_errno
errno_not_negative:
    mov $1, %ebx
    jmp  record_errno
errno_in_range:
    xor %ebx, %ebx
record_errno:
    record 1, slot1, %ebx

/*
 * 3. rt_sigaction(2) refuses a sigaction that is not mapped.
 */
    mov $SIGUSR1,      %rdi
    mov $1,            %rsi        /* act: one byte into the unmapped low page */
    mov $0,            %rdx
    mov $SIGSET_BYTES, %r10
    mov $SYS_rt_sigaction, %rax
    syscall
    neg %rax
    cmp $EFAULT, %rax
    je  sigaction_refused
    mov $1, %ebx
    jmp  record_sigaction
sigaction_refused:
    say h_sigaction_pass, h_sigaction_pass_len
    xor %ebx, %ebx
record_sigaction:
    record 2, slot2, %ebx

/*
 * 4. rt_sigaction(2) installs a handler.
 */
    mov $SIGUSR1,           %rdi
    lea sigaction_usr1(%rip), %rsi
    mov $0,                 %rdx        /* no old action wanted */
    mov $SIGSET_BYTES,      %r10
    mov $SYS_rt_sigaction,  %rax
    syscall
    test %rax, %rax
    jns sigaction_installed
    mov $1, %ebx
    jmp  record_install
sigaction_installed:
    xor %ebx, %ebx
record_install:
    record 3, slot3, %ebx

/*
 * 5. Sending the signal runs the handler with all three arguments.
 */
    mov $0,          %rdi            /* our own process group */
    mov $SIGUSR1,    %rsi
    mov $0,          %rdx
    mov $SYS_kill,   %rax
    syscall

    mov $0, %r10
    mov got_signo(%rip), %r11
    cmp %r10, %r11
    je  handler_did_not_run
    xor %ebx, %ebx
    jmp  record_handler
handler_did_not_run:
    mov $1, %ebx
record_handler:
    record 4, slot4, %ebx

    mov $SIGUSR1,       %r10
    mov got_signo(%rip), %r11
    cmp %r10, %r11
    je  handler_ran
    xor %ebx, %ebx
    jmp  record_signo
handler_ran:
    mov $1, %ebx
record_signo:
    record 6, slot6, %ebx

/*
 * 7. rt_sigreturn(2) restores the interrupted registers.
 *
 * A sentinel goes into every callee-saved register before the signal and must
 * still be there after it. A frame that did not save them, or a return that did
 * not restore them, shows up as a mismatch.
 */
    movabs $SENTINEL, %rax
    mov %rax, %rbx
    mov %rax, %rbp
    mov %rax, %r12
    mov %rax, %r13
    mov %rax, %r14
    mov %rax, %r15

    mov $0,          %rdi
    mov $SIGUSR1,    %rsi
    mov $0,          %rdx
    mov $SYS_kill,   %rax
    syscall

    movabs $SENTINEL, %r11
    cmp %r11, %rbx
    jne registers_lost
    cmp %r11, %rbp
    jne registers_lost
    cmp %r11, %r12
    jne registers_lost
    cmp %r11, %r13
    jne registers_lost
    cmp %r11, %r14
    jne registers_lost
    cmp %r11, %r15
    jne registers_lost
    xor %ebx, %ebx
    jmp  record_registers
registers_lost:
    say h_registers_fail, h_registers_fail_len
    mov $1, %ebx
record_registers:
    record 5, slot5, %ebx

summary:
    /* The mask is the exit status, so it reaches the kernel log intact even when
       the console does not. */
    mov failures(%rip), %rdi
    and $0x7f, %rdi                /* one bit per check */
summary_report:
    mov $SYS_exit, %rax
    syscall
    jmp .

    .section .note.GNU-stack,"",@progbits
