use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::collections::BTreeMap;
use spin::RwLock;

use crate::hypervisor::platform_ops;

type ApicId = u8;
type ProcessorId = usize;
pub(crate) static APIC_ID_MAP: RwLock<BTreeMap<ApicId, ProcessorId>> = RwLock::new(BTreeMap::new());
pub(crate) static PROCESSOR_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Gets an APIC ID.
pub(crate) fn get() -> ApicId {
    // See: (AMD) CPUID Fn0000_0001_EBX LocalApicId, LogicalProcessorCount, CLFlush
    // See: (Intel) Table 3-8. Information Returned by CPUID Instruction
    (x86::cpuid::cpuid!(0x1).ebx >> 24) as _
}

pub(crate) fn init() {
    assert!(PROCESSOR_COUNT.load(Ordering::Relaxed) == 0);
    platform_ops::get().run_on_all_processors(|| {
        let mut map = APIC_ID_MAP.write();
        assert!(
            map.insert(get(), PROCESSOR_COUNT.fetch_add(1, Ordering::Relaxed))
                .is_none()
        );
    });
}

pub(crate) fn processor_id_from(apic_id: ApicId) -> Option<ProcessorId> {
    let map = APIC_ID_MAP.read();
    map.get(&apic_id).copied()
}
