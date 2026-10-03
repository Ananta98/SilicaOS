// SPDX-License-Identifier: GPL-2.0

//! `mprotect(2)`.

use core::ops::Range;

use ostd::mm::Vaddr;

use crate::{api::errno::Result, vm::perms::VmPerms};

use super::{Vmar, check_page_aligned_range, is_mappable_range};

impl Vmar {
    /// Replaces the permissions in force over `range`.
    ///
    /// Permissions may only ever be narrowed within the ceiling that `mmap`
    /// fixed, so a mapping created with `PROT_READ` fails this with `EACCES`
    /// rather than being widened.
    pub fn mprotect(&self, prot: VmPerms, range: Range<Vaddr>) -> Result<()> {
        check_page_aligned_range(&range)?;
        if prot != prot.granted() {
            crate::return_errno!(EINVAL, "{prot:?} is not a valid protection");
        }
        if !is_mappable_range(&range) {
            crate::return_errno!(
                EFAULT,
                "the range {range:#x?} is not in the user address space"
            );
        }

        // Check every mapping up front, so that an `EACCES` leaves the address
        // space completely untouched.
        {
            let inner = self.inner.read();
            for mapping in inner.mappings.iter_in(&range) {
                if !mapping.perms().allows(prot) {
                    crate::return_errno!(
                        EACCES,
                        "{prot:?} exceeds what the mapping at {:#x} may be protected to ({:?})",
                        mapping.start(),
                        mapping.perms().may()
                    );
                }
            }
        }

        let mut inner = self.inner.write();
        let vm_space = &self.vm_space;
        let outcome = inner.carve(&range, |mut piece, _| {
            piece.set_perms(piece.perms().with_granted(prot));
            piece.protect(vm_space, piece.perms());
            Some(piece)
        });

        // A hole in the range is reported as `ENOMEM`, but the part that was
        // mapped has still been protected, which is what `mprotect` promises.
        if outcome.mapped < range.len() {
            crate::return_errno!(
                ENOMEM,
                "{:#x} of the {}-byte range are not mapped",
                range.len() - outcome.mapped,
                range.len()
            );
        }
        Ok(())
    }
}
