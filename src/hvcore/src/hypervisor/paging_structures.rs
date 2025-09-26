use core::ptr::addr_of;

use alloc::boxed::Box;
use x86::bits64::paging::{BASE_PAGE_SHIFT, BASE_PAGE_SIZE, LARGE_PAGE_SIZE};

use super::{platform_ops, support::zeroed_box};

#[derive(Debug, derive_deref::Deref, derive_deref::DerefMut)]
pub struct PagingStructures {
    ptr: Box<PagingStructuresRaw>,
}

impl Default for PagingStructures {
    fn default() -> Self {
        Self::new()
    }
}

impl PagingStructures {
    pub fn new() -> Self {
        Self {
            ptr: zeroed_box::<PagingStructuresRaw>(),
        }
    }
}

#[derive(Debug)]
#[repr(C, align(4096))]
pub struct PagingStructuresRaw {
    pub(crate) pml4: Pml4,
    pub(crate) pdpt: Pdpt,
    pub(crate) pd: [Pd; 512],
    pub(crate) pt: Pt,
    pub(crate) pt_apic: Pt,
}

impl PagingStructuresRaw {
    pub fn build_identity(&mut self) {
        build_identity_internal(self, false);
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C, align(4096))]
pub(crate) struct Table {
    pub(crate) entries: [Entry; 512],
}

pub(crate) use Table as Pml4;
pub(crate) use Table as Pdpt;
pub(crate) use Table as Pd;
pub(crate) use Table as Pt;

bitfield::bitfield! {
    #[derive(Clone, Copy)]
    pub struct Entry(u64);
    impl Debug;
    pub present, set_present: 0;
    pub writable, set_writable: 1;
    pub user, set_user: 2;
    pub large, set_large: 7;
    pub pfn, set_pfn: 51, 12;
}

pub(crate) fn build_identity_internal(ps: &mut PagingStructuresRaw, npt: bool) {
    let ops = platform_ops::get();
    let user = npt;

    let pml4 = &mut ps.pml4;
    pml4.entries[0].set_present(true);
    pml4.entries[0].set_writable(true);
    pml4.entries[0].set_user(user);
    pml4.entries[0].set_pfn(ops.pa(addr_of!(ps.pdpt) as _) >> BASE_PAGE_SHIFT);

    let mut pa = 0;
    for (i, pdpte) in ps.pdpt.entries.iter_mut().enumerate() {
        pdpte.set_present(true);
        pdpte.set_writable(true);
        pdpte.set_user(user);
        pdpte.set_pfn(ops.pa(addr_of!(ps.pd[i]) as _) >> BASE_PAGE_SHIFT);
        for pde in &mut ps.pd[i].entries {
            // The first 2MB is mapped with 4KB pages if it is not for NPT. This
            // is to make the zero page non-present and cause #PF in case of null
            // pointer access. Helps debugging. All other pages are 2MB mapped.
            if pa == 0 && !npt {
                pde.set_present(true);
                pde.set_writable(true);
                pde.set_user(user);
                pde.set_pfn(ops.pa(addr_of!(ps.pt) as _) >> BASE_PAGE_SHIFT);
                for pte in &mut ps.pt.entries {
                    pte.set_present(true);
                    pte.set_writable(true);
                    pte.set_user(user);
                    pte.set_pfn(pa >> BASE_PAGE_SHIFT);
                    pa += BASE_PAGE_SIZE as u64;
                }
                // Make the null page invalid to detect null pointer access.
                ps.pt.entries[0].set_present(false);
            } else {
                pde.set_present(true);
                pde.set_writable(true);
                pde.set_user(user);
                pde.set_large(true);
                pde.set_pfn(pa >> BASE_PAGE_SHIFT);
                pa += LARGE_PAGE_SIZE as u64;
            }
        }
    }
}
