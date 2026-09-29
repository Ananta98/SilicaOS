// SPDX-License-Identifier: GPL-2.0

//! `mprotect(2)`.

use core::ops::Range;

use ostd::mm::Vaddr;

use crate::{errno::Result, vm::perms::VmPerms};

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
            crate::return_errno!(EFAULT, "the range {range:#x?} is not in the user address space");
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

#[cfg(ktest)]
mod tests {
    use ostd::mm::PAGE_SIZE;
    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        errno::Errno,
        vm::{flags::MmapFlags, vmar::Vmar},
    };

    const AT: Vaddr = 0x4000_0000;

    fn vmar_with(prot: VmPerms) -> alloc::sync::Arc<Vmar> {
        let vmar = Vmar::new();
        vmar.mmap_anonymous(AT, 4 * PAGE_SIZE, prot, MmapFlags::PRIVATE)
            .unwrap();
        vmar
    }

    #[ktest]
    fn permissions_can_be_narrowed_and_restored() {
        let rw = VmPerms::READ | VmPerms::WRITE;
        let vmar = vmar_with(rw);
        let perms_of = |addr: Vaddr| crate::vm::tests::perms_at(&vmar, addr);

        assert_eq!(perms_of(AT).granted(), rw);
        assert_eq!(perms_of(AT).may(), rw);

        vmar.mprotect(VmPerms::READ, AT..AT + 2 * PAGE_SIZE).unwrap();

        // The first half is read-only now, and the second half is untouched.
        // The ceiling stays where `mmap` put it in both, which is what lets the
        // first half be widened again.
        assert_eq!(perms_of(AT).granted(), VmPerms::READ);
        assert_eq!(perms_of(AT).may(), rw);
        assert_eq!(perms_of(AT + 2 * PAGE_SIZE).granted(), rw);

        // Widening back is allowed, because `mmap` granted write permission.
        vmar.mprotect(rw, AT..AT + 2 * PAGE_SIZE).unwrap();
        assert_eq!(perms_of(AT).granted(), rw);
        assert_eq!(perms_of(AT).may(), rw);
        // The two halves are identical again, so they are one record once more.
        assert_eq!(vmar.mappings_in(AT..AT + 4 * PAGE_SIZE).len(), 1);
    }

    #[ktest]
    fn a_hole_is_enomem_but_the_mapped_part_is_still_protected() {
        let rw = VmPerms::READ | VmPerms::WRITE;
        let vmar = Vmar::new();
        for page in [0, 4, 8] {
            vmar.mmap_anonymous(AT + page * PAGE_SIZE, PAGE_SIZE, rw, MmapFlags::PRIVATE)
                .unwrap();
        }
        let perms_of = |addr: Vaddr| crate::vm::tests::perms_at(&vmar, addr);

        // Two of the five requested pages are mapped, so the call reports
        // `ENOMEM`...
        assert_eq!(
            vmar.mprotect(VmPerms::READ, AT..AT + 5 * PAGE_SIZE).unwrap_err(),
            Errno::ENOMEM
        );
        // ...but both mapped pages inside the range were still protected, which
        // is what `mprotect` promises, and the page past the range is untouched.
        assert_eq!(perms_of(AT).granted(), VmPerms::READ);
        assert_eq!(perms_of(AT + 4 * PAGE_SIZE).granted(), VmPerms::READ);
        assert_eq!(perms_of(AT + 8 * PAGE_SIZE).granted(), rw);
        // The ceiling is intact everywhere, so either of them can be widened.
        assert_eq!(perms_of(AT).may(), rw);
    }

    #[ktest]
    fn bad_arguments_are_rejected() {
        let vmar = vmar_with(VmPerms::READ);
        assert_eq!(vmar.mprotect(VmPerms::READ, AT..AT).unwrap_err(), Errno::EINVAL);
        assert_eq!(
            vmar.mprotect(VmPerms::READ, AT + 8..AT + PAGE_SIZE).unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            vmar.mprotect(VmPerms::MAY_EXEC, AT..AT + PAGE_SIZE).unwrap_err(),
            Errno::EINVAL
        );
    }
}
