//! This module implements enablement of Intel VMX.

use alloc::boxed::Box;
use derive_more::Debug;

use crate::hypervisor::{
    host::Extension,
    intel::guest::{get_adjusted_cr0, get_adjusted_cr4},
    platform_ops,
    support::zeroed_box,
    x86_instructions::{cr0, cr0_write, cr4, cr4_write, rdmsr, wrmsr},
};

#[derive(Default)]
pub(crate) struct Vmx {
    vmxon_region: Vmxon,
}

impl Extension for Vmx {
    fn enable(&mut self) {
        // The current CR0, CR4 and IA32_FEATURE_CONTROL MSR may not satisfy the
        // requirements for enabling VMX. Update them as required,
        cr0_write(get_adjusted_cr0(cr0()));
        cr4_write(get_adjusted_cr4(cr4()));
        Self::update_feature_control_msr();

        // Then, execute the VMXON instruction. Successful execution of the
        // instruction puts the processor into the operation mode called "VMX
        // root operation" allowing the use of the other VMX instructions.
        vmxon(&mut self.vmxon_region);
    }
}

impl Vmx {
    /// Updates an MSR to satisfy the requirement for entering VMX operation.
    fn update_feature_control_msr() {
        const IA32_FEATURE_CONTROL_LOCK_BIT_FLAG: u64 = 1 << 0;
        const IA32_FEATURE_CONTROL_ENABLE_VMX_OUTSIDE_SMX_FLAG: u64 = 1 << 2;

        // If the lock bit is cleared, set it along with the VMXON-outside-SMX
        // operation bit. Without those two bits, the VMXON instruction fails. They
        // are normally set but not always, for example, Bochs with OVMF does not.
        // See: 23.7 ENABLING AND ENTERING VMX OPERATION
        let feature_control = rdmsr(x86::msr::IA32_FEATURE_CONTROL);
        if (feature_control & IA32_FEATURE_CONTROL_LOCK_BIT_FLAG) == 0 {
            wrmsr(
                x86::msr::IA32_FEATURE_CONTROL,
                feature_control
                    | IA32_FEATURE_CONTROL_ENABLE_VMX_OUTSIDE_SMX_FLAG
                    | IA32_FEATURE_CONTROL_LOCK_BIT_FLAG,
            );
        }
    }
}

/// Logical representation of a VMXON region.
#[derive(derive_deref::Deref, derive_deref::DerefMut)]
struct Vmxon {
    ptr: Box<VmxonRaw>,
}

impl Default for Vmxon {
    fn default() -> Self {
        // The VMXON instruction requires 4KB of a region called "VMXON region".
        // This is a per-logical core data structure and only used for the VMXON
        // instruction.
        let mut vmxon = zeroed_box::<VmxonRaw>();

        // "Before executing VMXON, software should write the VMCS revision identifier
        //  (see Section 25.2) to the VMXON region."
        // See: 25.11.5 VMXON Region
        //
        // "(...) Software can discover the VMCS revision identifier that a processor
        //  uses by reading the VMX capability MSR IA32_VMX_BASIC (see Appendix A.1)."
        // See: 25.2 FORMAT OF THE VMCS REGION"
        vmxon.revision_id = rdmsr(x86::msr::IA32_VMX_BASIC) as _;

        Self { ptr: vmxon }
    }
}

/// The region of memory that the logical processor uses to support VMX operation.
///
/// See: 25.11.5 VMXON Region
#[derive(Debug)]
#[repr(C, align(4096))]
struct VmxonRaw {
    revision_id: u32,
    #[debug(skip)]
    data: [u8; 4092],
}

/// The wrapper of the VMXON instruction.
fn vmxon(vmxon_region: &mut VmxonRaw) {
    let va = vmxon_region as *const _;
    let pa = platform_ops::get().pa(va as *const _);
    unsafe { x86::bits64::vmx::vmxon(pa).unwrap() };
}
