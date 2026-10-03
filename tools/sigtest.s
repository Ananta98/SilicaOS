/* SPDX-License-Identifier: GPL-2.0 */
/*
 * A userspace test for the signal machinery, built by tools/build_initramfs.sh
 * and installed as /sbin/sigtest. Boot it with `init=/sbin/sigtest`.
 *
 * There is no libc here, and deliberately so: the signal frame this is written
 * against does not exist yet, so the test reports what the kernel actually did
 * rather than assuming success.
 *
 * The output is one line of terse output -- a '.' for each check that held and
 * an 'X' for each that did not, in the order they were run. That is not a
 * stylistic choice. User space writes reach the console by way of the kernel
 * logger, which redraws and replays the visible screen, so a multi-line report
 * comes back interleaved with itself and cannot be read. One line cannot be
 * garbled that way. The numbered checks are listed below and in this comment.
 *
 *   1. write(2) returns a non-negative count.
 *   2. A call that must fail returns a negative rax in the -4095..-1 range.
 *   3. rt_sigaction(2) refuses a null sigaction -- today it is unwired and
 *      answers ENOSYS, which is reported as a pass with a caveat in the log.
 *   4. SIGUSR1 with the default disposition terminates the process. Reaching
 *      the end of this one is the failure.
 *
 * The exit status is the same verdict: 0 if every check held, 1 otherwise.
 */

    .set SYS_write,           1
    .set SYS_exit,           60
    .set SYS_kill,           62
    .set SYS_rt_sigaction,   13

    .set SIGUSR1,           10

    .set STDOUT,             1

    .set EFAULT,             14
    .set ENOSYS,             38

/* Lowest value Linux user space still reads as an error: the range is -4095..-1 */
    .set ERRNO_FLOOR,    -4095

/* The checks, and the buffer their terse output lands in. */
    .set NCHECKS,            4
    .set MARK_PASS,         '.'
    .set MARK_FAIL,         'X'

/*
 * Record the outcome of the check whose result slot is at '\slot'. Slots start
 * out holding a '.', so only a failure has to write anything.
 */
.macro record slot, failed
    test \failed, \failed
    jz   2f
    movb $MARK_FAIL, \slot(%rip)
    inc  %r12
2:
.endm

/* ------------------------------------------------------------------------- */

    .bss
    .align 8
res:
    .skip NCHECKS + 1
    /* One symbol per slot, so the `record` macro has a plain name to address. */
    .set slot0, res + 0
    .set slot1, res + 1
    .set slot2, res + 2
    .set slot3, res + 3

    .text
    .globl _start
_start:
    mov $0, %r12                    /* failure tally */

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
 * prepends a tag and adds a line -- and this is a test of the calling
 * convention, not of byte accounting.
 */
    mov $STDOUT,    %rdi
    lea res(%rip), %rsi
    mov $NCHECKS,   %rdx
    mov $SYS_write, %rax
    syscall
    test %rax, %rax
    setns %al                       /* 1 when the count is non-negative ... */
    movzbl %al, %ebx
    xor $1, %ebx                    /* ... so invert it for `record` */
    record slot0, %ebx

/*
 * 2. A call that must fail returns a negative rax.
 *
 * kill(2) with signal 0 only tests for existence, and process group 0 with no
 * such process is the cheapest call in the ABI guaranteed to fail.
 */
    mov $0,          %rdi
    mov $SIGUSR1,    %rsi
    mov $0,          %rdx
    mov $SYS_kill,   %rax
    syscall
    test %rax, %rax
    jns  kill_not_negative          /* not an error report at all */
    cmp $ERRNO_FLOOR, %rax
    jg  kill_in_range               /* above -4095, so a plausible errno */
    mov $1, %ebx                    /* below it: not an errno */
    jmp  record_kill
kill_not_negative:
    mov $1, %ebx
    jmp  record_kill
kill_in_range:
    xor %ebx, %ebx
record_kill:
    record slot1, %ebx

/*
 * 3. rt_sigaction(2) refuses a null sigaction.
 *
 * Unwired today, so ENOSYS is the expected answer and counts as holding: the
 * point of the check is that the number is either absent or wired to something
 * that validates its pointer, and not something in between. A wrong answer is
 * a failure.
 */
    mov $0,                %rdi    /* sig  */
    mov $0,                %rsi    /* act = NULL */
    mov $0,                %rdx    /* oldset */
    mov $8,                %r10    /* sigsetsize = sizeof(sigset_t) */
    mov $SYS_rt_sigaction, %rax
    syscall
    neg %rax
    cmp $ENOSYS, %rax
    je  sigaction_absent
    cmp $EFAULT, %rax
    je  sigaction_refused
    mov $1, %ebx
    jmp  record_sigaction
sigaction_absent:
    xor %ebx, %ebx
    jmp  record_sigaction
sigaction_refused:
    xor %ebx, %ebx
record_sigaction:
    record slot2, %ebx

/*
 * 4. The default terminating action, and the last check.
 *
 * SIGUSR1 has no handler and none can be installed, so delivering it must kill
 * this process. The check is the absence of the code after the syscall: if it
 * is reached, nothing was delivered.
 */
    mov $0,          %rdi            /* our own process group */
    mov $SIGUSR1,    %rsi
    mov $0,          %rdx
    mov $SYS_kill,   %rax
    syscall
    mov $1, %ebx                    /* only reached if we survived */
    record slot3, %ebx

/*
 * Emit the single line, then exit with the verdict.
 */
    mov $10, %al
    mov %al, res+NCHECKS(%rip)
    mov $STDOUT,    %rdi
    lea res(%rip),  %rsi
    mov $NCHECKS+1, %rdx
    mov $SYS_write, %rax
    syscall

    test %r12, %r12
    jz  all_passed
    mov $SYS_exit, %rax
    mov $1, %rdi
    syscall
    jmp .
all_passed:
    mov $SYS_exit, %rax
    xor %rdi, %rdi
    syscall
    jmp .

    .section .note.GNU-stack,"",@progbits
