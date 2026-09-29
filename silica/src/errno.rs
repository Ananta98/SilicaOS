// SPDX-License-Identifier: GPL-2.0

//! POSIX error numbers.
//!
//! Subsystems report failures as an [`Errno`], whose discriminants are the
//! numeric values that a syscall must hand back to user space. Keeping the
//! numbers here (rather than reusing [`ostd::Error`], which is far coarser)
//! is what lets the virtual memory layer distinguish, say, [`Errno::EEXIST`]
//! from [`Errno::ENOMEM`], as `mmap(2)` requires.

use core::fmt::{self, Debug, Display, Formatter};

/// A POSIX error number.
///
/// The discriminants are the Linux `asm-generic/errno.h` values, which is what
/// user space observes on the supported platforms.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Errno {
    /// Operation not permitted.
    EPERM = 1,
    /// No such file or directory.
    ENOENT = 2,
    /// No such process.
    ESRCH = 3,
    /// Interrupted system call.
    EINTR = 4,
    /// Input/output error.
    EIO = 5,
    /// Bad file descriptor.
    EBADF = 9,
    /// Resource temporarily unavailable.
    EAGAIN = 11,
    /// Cannot allocate memory.
    ENOMEM = 12,
    /// Permission denied.
    EACCES = 13,
    /// Bad address.
    EFAULT = 14,
    /// Device or resource busy.
    EBUSY = 16,
    /// File exists.
    EEXIST = 17,
    /// No such device.
    ENODEV = 19,
    /// Invalid argument.
    EINVAL = 22,
    /// Too many open files in system.
    ENFILE = 23,
    /// Too many open files.
    EMFILE = 24,
    /// No space left on device.
    ENOSPC = 28,
    /// Read-only file system.
    EROFS = 30,
    /// Numerical result out of range.
    ERANGE = 34,
    /// File name too long.
    ENAMETOOLONG = 36,
    /// Function not implemented.
    ENOSYS = 38,
    /// Too many levels of symbolic links.
    ELOOP = 40,
    /// Value too large for defined data type.
    EOVERFLOW = 75,
    /// Operation not supported.
    ENOTSUP = 95,
}

impl Errno {
    /// Returns the numeric value that user space observes.
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Returns the name of the error, as it is spelled in the C headers.
    ///
    /// The derived [`Debug`] already prints this, but a log line that wants the
    /// name next to a number wants it as a value.
    pub const fn name(self) -> &'static str {
        match self {
            Self::EPERM => "EPERM",
            Self::ENOENT => "ENOENT",
            Self::ESRCH => "ESRCH",
            Self::EINTR => "EINTR",
            Self::EIO => "EIO",
            Self::EBADF => "EBADF",
            Self::EAGAIN => "EAGAIN",
            Self::ENOMEM => "ENOMEM",
            Self::EACCES => "EACCES",
            Self::EFAULT => "EFAULT",
            Self::EBUSY => "EBUSY",
            Self::EEXIST => "EEXIST",
            Self::ENODEV => "ENODEV",
            Self::EINVAL => "EINVAL",
            Self::ENFILE => "ENFILE",
            Self::EMFILE => "EMFILE",
            Self::ENOSPC => "ENOSPC",
            Self::EROFS => "EROFS",
            Self::ERANGE => "ERANGE",
            Self::ENAMETOOLONG => "ENAMETOOLONG",
            Self::ENOSYS => "ENOSYS",
            Self::ELOOP => "ELOOP",
            Self::EOVERFLOW => "EOVERFLOW",
            Self::ENOTSUP => "ENOTSUP",
        }
    }

    /// Returns a short human-readable description.
    pub const fn description(self) -> &'static str {
        match self {
            Self::EPERM => "operation not permitted",
            Self::ENOENT => "no such file or directory",
            Self::ESRCH => "no such process",
            Self::EINTR => "interrupted system call",
            Self::EIO => "input/output error",
            Self::EBADF => "bad file descriptor",
            Self::EAGAIN => "resource temporarily unavailable",
            Self::ENOMEM => "cannot allocate memory",
            Self::EACCES => "permission denied",
            Self::EFAULT => "bad address",
            Self::EBUSY => "device or resource busy",
            Self::EEXIST => "file exists",
            Self::ENODEV => "no such device",
            Self::EINVAL => "invalid argument",
            Self::ENFILE => "too many open files in system",
            Self::EMFILE => "too many open files",
            Self::ENOSPC => "no space left on device",
            Self::EROFS => "read-only file system",
            Self::ERANGE => "numerical result out of range",
            Self::ENAMETOOLONG => "file name too long",
            Self::ENOSYS => "function not implemented",
            Self::ELOOP => "too many levels of symbolic links",
            Self::EOVERFLOW => "value too large for defined data type",
            Self::ENOTSUP => "operation not supported",
        }
    }
}

impl Display for Errno {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.description())
    }
}

impl From<Errno> for i32 {
    fn from(errno: Errno) -> i32 {
        errno.as_i32()
    }
}

/// Maps OSTD's coarse error type onto the closest POSIX error number.
///
/// OSTD reports that a page table operation failed as a single
/// [`ostd::Error::AccessDenied`]; the caller has already validated the
/// arguments, so that really does mean the operation was not permitted.
impl From<ostd::Error> for Errno {
    fn from(err: ostd::Error) -> Errno {
        match err {
            ostd::Error::InvalidArgs => Errno::EINVAL,
            ostd::Error::NoMemory => Errno::ENOMEM,
            ostd::Error::PageFault => Errno::EFAULT,
            ostd::Error::AccessDenied => Errno::EACCES,
            ostd::Error::IoError => Errno::EIO,
            ostd::Error::NotEnoughResources => Errno::ENOSPC,
            ostd::Error::Overflow => Errno::EOVERFLOW,
        }
    }
}

/// A [`Result`](core::result::Result) whose default error is an [`Errno`].
pub type Result<T, E = Errno> = core::result::Result<T, E>;

/// Returns early with an [`Errno`].
///
/// The optional trailing arguments are logged, so that the reason for a failure
/// is not lost even though the errno itself carries only a number.
///
/// ```ignore
/// return_errno!(EINVAL, "offset {:#x} is not page-aligned", offset);
/// return_errno!(ENOMEM);
/// ```
#[macro_export]
macro_rules! return_errno {
    ($errno:ident) => {
        return ::core::result::Result::Err($crate::errno::Errno::$errno)
    };
    ($errno:ident, $($arg:tt)+) => {{
        ::ostd::error!("{}: {}", stringify!($errno), ::core::format_args!($($arg)+));
        return ::core::result::Result::Err($crate::errno::Errno::$errno);
    }};
}
