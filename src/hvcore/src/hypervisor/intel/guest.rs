//! This module implements a guest management.

use core::{arch::global_asm, ptr::addr_of};

use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
};
use derive_more::Debug;
use spin::Lazy;
use x86::{
    bits64::{paging::BASE_PAGE_SIZE, rflags::RFlags},
    controlregs::{Cr0, Cr4},
    debugregs::{dr0_write, dr1_write, dr2_write, dr3_write, dr6_write, dr7_write, Dr6, Dr7},
    segmentation::{
        cs, ds, es, fs, gs, ss, CodeSegmentType, DataSegmentType, SystemDescriptorTypes64,
    },
    vmx::vmcs,
};

use crate::hypervisor::{
    host::{Guest, InstructionInfo, VmExitReason},
    platform_ops,
    registers::Registers,
    segment::SegmentDescriptor,
    support::{zeroed_box, Page},
    x86_instructions::{cr0, cr3, cr4, lar, ldtr, lsl, rdmsr, sgdt, sidt, tr, write_cr2},
    SHARED_HOST_DATA,
};

use super::epts::Epts;

/// Representation of a guest.
pub(crate) struct VmxGuest {
    id: usize,
    registers: Registers,
    vmcs: Vmcs,
}

impl Guest for VmxGuest {
    fn new(id: usize) -> Self {
        // The processor is now in VMX root operation. This means that the processor
        // can execute other VMX instructions and almost ready for configuring a VMCS
        // with the VMREAD and VMWRITE instructions. Before doing so, we need to make
        // a VMCS "clear", "active" and "current". Otherwise, the VMREAD and VMWRITE
        // instructions do not know which VMCS to operate on and fail. For visualization
        // of VMCS state transitions,
        // See: Figure 25-1. States of VMCS X
        //
        // Firstly, "clear" the VMCS using the VMCLEAR instruction. See `Vmcs::new`.
        //
        // "the VMCLEAR instruction initializes any implementation-specific
        //  information in the VMCS region referenced by its operand. (...),
        //  software should execute VMCLEAR on a VMCS region before making the
        //  corresponding VMCS active with VMPTRLD for the first time."
        // See: 25.11.3 Initializing a VMCS
        Self {
            id,
            registers: Registers::default(),
            vmcs: Vmcs::new(),
        }
    }

    fn activate(&mut self) {
        // To make the VMCS "active" and "current" execute the VMPTRLD instruction.
        // This instruction requires that the revision identifier is initialized,
        // which was done in `Vmcs::new`.
        //
        // "Software should write the VMCS revision identifier to the VMCS region
        //  before using that region for a VMCS. (...) VMPTRLD fails if (...) a
        //  VMCS region whose VMCS revision identifier differs from that used by
        //  the processor."
        // See: 25.2 FORMAT OF THE VMCS REGION
        vmptrld(&mut self.vmcs);

        // The processor now have an associated VMCS (called a current VMCS) and
        // able to execute the VMREAD and VMWRITE instructions. Let us program it.
    }

    fn initialize(&mut self, registers: &Registers) {
        self.registers = *registers;
        self.initialize_control();
        self.initialize_guest();
        self.initialize_host();
    }

    fn run(&mut self) -> VmExitReason {
        const VMX_EXIT_REASON_INIT: u16 = 3;
        const VMX_EXIT_REASON_SIPI: u16 = 4;
        const VMX_EXIT_REASON_CPUID: u16 = 10;
        const VMX_EXIT_REASON_RDMSR: u16 = 31;
        const VMX_EXIT_REASON_WRMSR: u16 = 32;
        const VMX_EXIT_REASON_XSETBV: u16 = 55;

        vmwrite(vmcs::guest::RIP, self.registers.rip);
        vmwrite(vmcs::guest::RSP, self.registers.rsp);
        vmwrite(vmcs::guest::RFLAGS, self.registers.rflags);

        // Execute the guest until VM-exit occurs.
        log::trace!("Entering the guest");
        let flags = unsafe { run_vmx_guest(&mut self.registers) };
        if let Err(err) = vmx_succeed(RFlags::from_raw(flags)) {
            panic!("{err}");
        }
        log::trace!("Exited the guest");

        self.registers.rip = vmread(vmcs::guest::RIP);
        self.registers.rsp = vmread(vmcs::guest::RSP);
        self.registers.rflags = vmread(vmcs::guest::RFLAGS);

        // Return VM-exit reason.
        match vmread(vmcs::ro::EXIT_REASON) as u16 {
            VMX_EXIT_REASON_INIT => {
                self.handle_init_signal();
                VmExitReason::InitSignal
            }
            VMX_EXIT_REASON_SIPI => {
                self.handle_sipi_signal();
                VmExitReason::StartupIpi
            }
            VMX_EXIT_REASON_CPUID => VmExitReason::Cpuid(InstructionInfo {
                next_rip: self.registers.rip + vmread(vmcs::ro::VMEXIT_INSTRUCTION_LEN),
            }),
            VMX_EXIT_REASON_RDMSR => VmExitReason::Rdmsr(InstructionInfo {
                next_rip: self.registers.rip + vmread(vmcs::ro::VMEXIT_INSTRUCTION_LEN),
            }),
            VMX_EXIT_REASON_WRMSR => VmExitReason::Wrmsr(InstructionInfo {
                next_rip: self.registers.rip + vmread(vmcs::ro::VMEXIT_INSTRUCTION_LEN),
            }),
            VMX_EXIT_REASON_XSETBV => VmExitReason::XSetBv(InstructionInfo {
                next_rip: self.registers.rip + vmread(vmcs::ro::VMEXIT_INSTRUCTION_LEN),
            }),
            _ => {
                log::error!("{:#x?}", self.vmcs);
                panic!(
                    "Unhandled VM-exit reason: {:?}",
                    vmread(vmcs::ro::EXIT_REASON)
                )
            }
        }
    }

    fn regs(&mut self) -> &mut Registers {
        &mut self.registers
    }
}

impl VmxGuest {
    /// Initializes the control fields of the VMCS.
    fn initialize_control(&self) {
        // - Set HOST_ADDRESS_SPACE_SIZE to run the host on the 64bit mode.
        // - Set IA32E_MODE_GUEST to run the guest on the 64bit mode.
        vmwrite(
            vmcs::control::VMEXIT_CONTROLS,
            Self::adjust_vmx_control(
                VmxControl::VmExit,
                vmcs::control::ExitControls::HOST_ADDRESS_SPACE_SIZE.bits() as _,
            ),
        );
        vmwrite(
            vmcs::control::VMENTRY_CONTROLS,
            Self::adjust_vmx_control(
                VmxControl::VmEntry,
                vmcs::control::EntryControls::IA32E_MODE_GUEST.bits() as _,
            ),
        );

        // Nothing to enable in the PINBASED_EXEC_CONTROLS.
        vmwrite(
            vmcs::control::PINBASED_EXEC_CONTROLS,
            Self::adjust_vmx_control(VmxControl::PinBased, 0),
        );

        // The processor-based VM-execution controls govern the handling of
        // synchronous events, mainly those caused by the execution of specific
        // instructions.
        //
        // - MSR bitmaps are used; this is not to cause VM-exit as much as possible.
        //   We are setting the MSR bitmaps that are all cleared. This prevents
        //   VM-exits from occurring when 0x0 - 0x1fff and 0xc0000000 - 0xc0001fff
        //   are accessed. VM-exit still occurs if outside the range is accessed,
        //   and it is not possible to prevent this.
        //
        // - The secondary processor-based controls are used; this is to:
        //   - Enable EPT and unrestricted guest to allow a real-mode guest, which
        //     is required for the UEFI version where a guest runs in real-mode
        //     after INIT-SIPI-SIPI.
        //   - Let the guest executes RDTSCP, INVPCID and the XSAVE/XRSTORS family
        //     instructions. Those instructions are used in Windows 10+. If those
        //     are not set, attempt to execute them causes #UD, which results in
        //     a bug check.
        vmwrite(
            vmcs::control::PRIMARY_PROCBASED_EXEC_CONTROLS,
            Self::adjust_vmx_control(
                VmxControl::ProcessorBased,
                (vmcs::control::PrimaryControls::USE_MSR_BITMAPS
                    | vmcs::control::PrimaryControls::SECONDARY_CONTROLS)
                    .bits() as _,
            ),
        );
        vmwrite(
            vmcs::control::SECONDARY_PROCBASED_EXEC_CONTROLS,
            Self::adjust_vmx_control(
                VmxControl::ProcessorBased2,
                (vmcs::control::SecondaryControls::ENABLE_EPT
                    | vmcs::control::SecondaryControls::UNRESTRICTED_GUEST
                    | vmcs::control::SecondaryControls::ENABLE_RDTSCP
                    | vmcs::control::SecondaryControls::ENABLE_INVPCID
                    | vmcs::control::SecondaryControls::ENABLE_XSAVES_XRSTORS)
                    .bits() as _,
            ),
        );

        let msr_bitmaps_va = SHARED_GUEST_DATA.msr_bitmaps.as_ref() as *const _;
        let msr_bitmaps_pa = platform_ops::get().pa(msr_bitmaps_va as *const _);
        vmwrite(vmcs::control::MSR_BITMAPS_ADDR_FULL, msr_bitmaps_pa);
        vmwrite(vmcs::control::EPTP_FULL, SHARED_GUEST_DATA.epts.eptp().0);
    }

    /// Initializes the guest-state fields of the VMCS.
    // It does so by copying the current register values with the assumption that
    // those have not changed since `Registers::capture_current` is called.
    fn initialize_guest(&self) {
        let idtr = sidt();
        let gdtr = sgdt();

        vmwrite(vmcs::guest::ES_SELECTOR, es().bits());
        vmwrite(vmcs::guest::CS_SELECTOR, cs().bits());
        vmwrite(vmcs::guest::SS_SELECTOR, ss().bits());
        vmwrite(vmcs::guest::DS_SELECTOR, ds().bits());
        vmwrite(vmcs::guest::FS_SELECTOR, fs().bits());
        vmwrite(vmcs::guest::GS_SELECTOR, gs().bits());
        vmwrite(vmcs::guest::TR_SELECTOR, tr().bits());
        vmwrite(vmcs::guest::LDTR_SELECTOR, ldtr().bits());

        vmwrite(vmcs::guest::ES_LIMIT, lsl(es()));
        vmwrite(vmcs::guest::CS_LIMIT, lsl(cs()));
        vmwrite(vmcs::guest::SS_LIMIT, lsl(ss()));
        vmwrite(vmcs::guest::DS_LIMIT, lsl(ds()));
        vmwrite(vmcs::guest::FS_LIMIT, lsl(fs()));
        vmwrite(vmcs::guest::GS_LIMIT, lsl(gs()));
        vmwrite(vmcs::guest::TR_LIMIT, lsl(tr()));

        vmwrite(
            vmcs::guest::ES_ACCESS_RIGHTS,
            Self::access_rights(lar(es())),
        );
        vmwrite(
            vmcs::guest::CS_ACCESS_RIGHTS,
            Self::access_rights(lar(cs())),
        );
        vmwrite(
            vmcs::guest::SS_ACCESS_RIGHTS,
            Self::access_rights(lar(ss())),
        );
        vmwrite(
            vmcs::guest::DS_ACCESS_RIGHTS,
            Self::access_rights(lar(ds())),
        );
        vmwrite(
            vmcs::guest::FS_ACCESS_RIGHTS,
            Self::access_rights(lar(fs())),
        );
        vmwrite(
            vmcs::guest::GS_ACCESS_RIGHTS,
            Self::access_rights(lar(gs())),
        );
        vmwrite(
            vmcs::guest::TR_ACCESS_RIGHTS,
            Self::access_rights(lar(tr())),
        );
        vmwrite(vmcs::guest::LDTR_ACCESS_RIGHTS, Self::access_rights(0));

        vmwrite(vmcs::guest::FS_BASE, rdmsr(x86::msr::IA32_FS_BASE));
        vmwrite(vmcs::guest::GS_BASE, rdmsr(x86::msr::IA32_GS_BASE));
        vmwrite(
            vmcs::guest::TR_BASE,
            SegmentDescriptor::try_from_gdtr(&gdtr, tr())
                .unwrap()
                .base(),
        );

        vmwrite(vmcs::guest::GDTR_BASE, gdtr.base as u64);
        vmwrite(vmcs::guest::GDTR_LIMIT, gdtr.limit);
        vmwrite(vmcs::guest::IDTR_BASE, idtr.base as u64);
        vmwrite(vmcs::guest::IDTR_LIMIT, idtr.limit);

        vmwrite(
            vmcs::guest::IA32_SYSENTER_CS,
            rdmsr(x86::msr::IA32_SYSENTER_CS),
        );
        vmwrite(
            vmcs::guest::IA32_SYSENTER_EIP,
            rdmsr(x86::msr::IA32_SYSENTER_EIP),
        );
        vmwrite(
            vmcs::guest::IA32_SYSENTER_ESP,
            rdmsr(x86::msr::IA32_SYSENTER_ESP),
        );

        // "If the "VMCS shadowing" VM-execution control is 1, (...). Otherwise,
        //  software should set this field to FFFFFFFF_FFFFFFFFH to avoid VM-entry
        //  failures."
        // See: 25.4.2 Guest Non-Register State
        vmwrite(vmcs::guest::LINK_PTR_FULL, u64::MAX);

        vmwrite(vmcs::guest::CR0, cr0().bits() as u64);
        vmwrite(vmcs::guest::CR3, cr3());
        vmwrite(vmcs::guest::CR4, cr4().bits() as u64);
        vmwrite(vmcs::guest::RSP, self.registers.rsp);
        vmwrite(vmcs::guest::RIP, self.registers.rip);
        vmwrite(vmcs::guest::RFLAGS, self.registers.rflags);
    }

    /// Initializes the host-state fields of the VMCS.
    fn initialize_host(&self) {
        let shared_host = SHARED_HOST_DATA.get().unwrap();

        // Use a custom CR3 if specified. Otherwise, use the current.
        let cr3 = if let Some(host_pt) = &shared_host.pt {
            addr_of!(*host_pt.as_ref()) as u64
        } else {
            cr3()
        };

        // Use a custom GDT, TR, and TSS if specified. Otherwise, use the current.
        let (gdt_base, tr, tss_base) = if let Some(host_gdt_and_tss) = &shared_host.gdts {
            let gdt_base = addr_of!(host_gdt_and_tss[self.id].gdt[0]) as u64;
            let tr = host_gdt_and_tss[self.id].tr.unwrap();
            let tss = host_gdt_and_tss[self.id].tss.as_ref().unwrap();
            let tss_base = tss as *const _ as u64;
            (gdt_base, tr, tss_base)
        } else {
            let gdtr = sgdt();
            let tr = tr();
            let tss_base = SegmentDescriptor::try_from_gdtr(&gdtr, tr).unwrap().base();
            (gdtr.base as u64, tr, tss_base)
        };

        // Use a custom IDT if specified. Otherwise, use the current.
        let idt_base = if let Some(host_idt) = &shared_host.idt {
            host_idt.idtr().base as u64
        } else {
            let idtr = sidt();
            idtr.base as u64
        };

        // The lower 3 bits of the selectors must be zero.
        // "In the selector field for each of CS, SS, DS, ES, FS, GS, and TR,
        //  the RPL (bits 1:0) and the TI flag (bit 2) must be 0."
        // See: 27.2.3 Checks on Host Segment and Descriptor-Table Registers
        vmwrite(vmcs::host::ES_SELECTOR, es().bits() & !0b111);
        vmwrite(vmcs::host::CS_SELECTOR, cs().bits() & !0b111);
        vmwrite(vmcs::host::SS_SELECTOR, ss().bits() & !0b111);
        vmwrite(vmcs::host::DS_SELECTOR, ds().bits() & !0b111);
        vmwrite(vmcs::host::FS_SELECTOR, fs().bits() & !0b111);
        vmwrite(vmcs::host::GS_SELECTOR, gs().bits() & !0b111);
        vmwrite(vmcs::host::TR_SELECTOR, tr.bits() & !0b111);

        vmwrite(vmcs::host::CR0, cr0().bits() as u64);
        vmwrite(vmcs::host::CR3, cr3);
        vmwrite(vmcs::host::CR4, cr4().bits() as u64);

        vmwrite(vmcs::host::FS_BASE, rdmsr(x86::msr::IA32_FS_BASE));
        vmwrite(vmcs::host::GS_BASE, rdmsr(x86::msr::IA32_GS_BASE));
        vmwrite(vmcs::host::TR_BASE, tss_base);
        vmwrite(vmcs::host::GDTR_BASE, gdt_base);
        vmwrite(vmcs::host::IDTR_BASE, idt_base);
    }

    /// Returns the VM control value that is adjusted in consideration with the
    /// VMX capability MSR.
    fn adjust_vmx_control(control: VmxControl, requested_value: u64) -> u64 {
        const IA32_VMX_BASIC_VMX_CONTROLS_FLAG: u64 = 1 << 55;

        // This determines the right VMX capability MSR based on the value of
        // IA32_VMX_BASIC. This is required to fullfil the following requirements:
        //
        // "It is necessary for software to consult only one of the capability MSRs
        //  to determine the allowed settings of the pin based VM-execution controls:"
        // See: A.3.1 Pin-Based VM-Execution Controls
        let vmx_basic = rdmsr(x86::msr::IA32_VMX_BASIC);
        let true_cap_msr_supported = (vmx_basic & IA32_VMX_BASIC_VMX_CONTROLS_FLAG) != 0;

        let cap_msr = match (control, true_cap_msr_supported) {
            (VmxControl::PinBased, true) => x86::msr::IA32_VMX_TRUE_PINBASED_CTLS,
            (VmxControl::PinBased, false) => x86::msr::IA32_VMX_PINBASED_CTLS,
            (VmxControl::ProcessorBased, true) => x86::msr::IA32_VMX_TRUE_PROCBASED_CTLS,
            (VmxControl::ProcessorBased, false) => x86::msr::IA32_VMX_PROCBASED_CTLS,
            (VmxControl::VmExit, true) => x86::msr::IA32_VMX_TRUE_EXIT_CTLS,
            (VmxControl::VmExit, false) => x86::msr::IA32_VMX_EXIT_CTLS,
            (VmxControl::VmEntry, true) => x86::msr::IA32_VMX_TRUE_ENTRY_CTLS,
            (VmxControl::VmEntry, false) => x86::msr::IA32_VMX_ENTRY_CTLS,
            // There is no TRUE MSR for IA32_VMX_PROCBASED_CTLS2. Just use
            // IA32_VMX_PROCBASED_CTLS2 unconditionally.
            (VmxControl::ProcessorBased2, _) => x86::msr::IA32_VMX_PROCBASED_CTLS2,
            (VmxControl::ProcessorBased3, _) => {
                const IA32_VMX_PROCBASED_CTLS3: u32 = 0x492;

                let allowed1 = rdmsr(IA32_VMX_PROCBASED_CTLS3);
                let effective_value = requested_value & allowed1;
                assert!(
                    effective_value | requested_value == effective_value,
                    "One or more requested features are not supported: {effective_value:#x?} : {requested_value:#x?} "
                );
                return effective_value;
            }
        };

        // Each bit of the following VMCS values might have to be set or cleared
        // according to the value indicated by the VMX capability MSRs.
        //  - pin-based VM-execution controls,
        //  - primary processor-based VM-execution controls,
        //  - secondary processor-based VM-execution controls.
        //
        // The VMX capability MSR is composed of two 32bit values, the lower 32bits
        // indicate bits can be 0, and the higher 32bits indicates bits can be 1.
        // In other words, if those bits are "cleared", corresponding bits MUST BE 1
        // and MUST BE 0 respectively. The below summarizes the interpretation:
        //
        //        Lower bits (allowed 0) Higher bits (allowed 1) Meaning
        // Bit X  1                      1                       The bit X is flexible
        // Bit X  1                      0                       The bit X is fixed to 0
        // Bit X  0                      1                       The bit X is fixed to 1
        //
        // The following code enforces this logic by setting bits that must be 1,
        // and clearing bits that must be 0.
        //
        // See: A.3.1 Pin-Based VM-Execution Controls
        // See: A.3.2 Primary Processor-Based VM-Execution Controls
        // See: A.3.3 Secondary Processor-Based VM-Execution Controls
        let capabilities = rdmsr(cap_msr);
        let allowed0 = capabilities as u32;
        let allowed1 = (capabilities >> 32) as u32;
        let requested_value = u32::try_from(requested_value).unwrap();
        let mut effective_value = requested_value;
        effective_value |= allowed0;
        effective_value &= allowed1;
        assert!(
            effective_value | requested_value == effective_value,
            "One or more requested features are not supported for {control:?}: {effective_value:#x?} vs {requested_value:#x?}"
        );
        u64::from(effective_value)
    }

    /// Returns access rights in the format VMCS expects.
    fn access_rights(access_rights: u32) -> u32 {
        const VMX_SEGMENT_ACCESS_RIGHTS_UNUSABLE_FLAG: u32 = 1 << 16;

        // "In general, a segment register is unusable if it has been loaded with a
        //  null selector."
        // See: 25.4.1 Guest Register State
        if access_rights == 0 {
            return VMX_SEGMENT_ACCESS_RIGHTS_UNUSABLE_FLAG;
        }

        // Convert the native access right to the format for VMX. Those two formats
        // are almost identical except that first 8 bits of the native format does
        // not exist in the VMX format, and that few fields are undefined in the
        // native format but reserved to be zero in the VMX format.
        (access_rights >> 8) & 0b1111_0000_1111_1111
    }

    /// Handles VM-exit due to the INIT signal.
    // This function initializes the processor to the state after INIT as described
    // in the Intel SDM.
    // See: Table 9-1. IA-32 and Intel 64 Processor States Following Power-up,
    //      Reset, or INIT
    fn handle_init_signal(&mut self) {
        self.registers.rflags = RFlags::FLAGS_A1.bits();
        vmwrite(vmcs::guest::RFLAGS, self.registers.rflags);

        self.registers.rip = 0xfff0;
        vmwrite(vmcs::guest::RIP, self.registers.rip);

        write_cr2(0);
        vmwrite(vmcs::guest::CR3, 0u64);
        vmwrite(vmcs::control::CR0_READ_SHADOW, 0u64);
        vmwrite(vmcs::control::CR4_READ_SHADOW, 0u64);

        // Actual guest CR0 and CR4 must fulfill requirements for VMX. Apply those.
        vmwrite(
            vmcs::guest::CR0,
            get_adjusted_guest_cr0(Cr0::CR0_EXTENSION_TYPE).bits() as u64,
        );
        vmwrite(
            vmcs::guest::CR4,
            get_adjusted_guest_cr4(Cr4::empty()).bits() as u64,
        );

        let mut access_rights = VmxSegmentAccessRights(0);
        access_rights.set_segment_type(CodeSegmentType::ExecuteReadAccessed as u32);
        access_rights.set_descriptor_type(true);
        access_rights.set_present(true);

        vmwrite(vmcs::guest::CS_SELECTOR, 0xf000u64);
        vmwrite(vmcs::guest::CS_BASE, 0xffff_0000u64);
        vmwrite(vmcs::guest::CS_LIMIT, 0xffffu64);
        vmwrite(vmcs::guest::CS_ACCESS_RIGHTS, access_rights.0);

        access_rights.set_segment_type(DataSegmentType::ReadWriteAccessed as u32);
        vmwrite(vmcs::guest::SS_SELECTOR, 0u64);
        vmwrite(vmcs::guest::SS_BASE, 0u64);
        vmwrite(vmcs::guest::SS_LIMIT, 0xffffu64);
        vmwrite(vmcs::guest::SS_ACCESS_RIGHTS, access_rights.0);

        vmwrite(vmcs::guest::DS_SELECTOR, 0u64);
        vmwrite(vmcs::guest::DS_BASE, 0u64);
        vmwrite(vmcs::guest::DS_LIMIT, 0xffffu64);
        vmwrite(vmcs::guest::DS_ACCESS_RIGHTS, access_rights.0);

        vmwrite(vmcs::guest::ES_SELECTOR, 0u64);
        vmwrite(vmcs::guest::ES_BASE, 0u64);
        vmwrite(vmcs::guest::ES_LIMIT, 0xffffu64);
        vmwrite(vmcs::guest::ES_ACCESS_RIGHTS, access_rights.0);

        vmwrite(vmcs::guest::FS_SELECTOR, 0u64);
        vmwrite(vmcs::guest::FS_BASE, 0u64);
        vmwrite(vmcs::guest::FS_LIMIT, 0xffffu64);
        vmwrite(vmcs::guest::FS_ACCESS_RIGHTS, access_rights.0);

        vmwrite(vmcs::guest::GS_SELECTOR, 0u64);
        vmwrite(vmcs::guest::GS_BASE, 0u64);
        vmwrite(vmcs::guest::GS_LIMIT, 0xffffu64);
        vmwrite(vmcs::guest::GS_ACCESS_RIGHTS, access_rights.0);

        let extended_model_id = x86::cpuid::CpuId::new()
            .get_feature_info()
            .unwrap()
            .extended_model_id();
        self.registers.rdx = 0x600 | ((extended_model_id as u64) << 16);
        self.registers.rax = 0x0;
        self.registers.rbx = 0x0;
        self.registers.rcx = 0x0;
        self.registers.rsi = 0x0;
        self.registers.rdi = 0x0;
        self.registers.rbp = 0x0;

        self.registers.rsp = 0x0;
        vmwrite(vmcs::guest::RSP, self.registers.rsp);

        vmwrite(vmcs::guest::GDTR_BASE, 0u64);
        vmwrite(vmcs::guest::GDTR_LIMIT, 0xffffu64);
        vmwrite(vmcs::guest::IDTR_BASE, 0u64);
        vmwrite(vmcs::guest::IDTR_LIMIT, 0xffffu64);

        access_rights.set_segment_type(SystemDescriptorTypes64::LDT as u32);
        access_rights.set_descriptor_type(false);
        vmwrite(vmcs::guest::LDTR_SELECTOR, 0u64);
        vmwrite(vmcs::guest::LDTR_BASE, 0u64);
        vmwrite(vmcs::guest::LDTR_LIMIT, 0xffffu64);
        vmwrite(vmcs::guest::LDTR_ACCESS_RIGHTS, access_rights.0);

        access_rights.set_segment_type(SystemDescriptorTypes64::TssBusy as u32);
        vmwrite(vmcs::guest::TR_SELECTOR, 0u64);
        vmwrite(vmcs::guest::TR_BASE, 0u64);
        vmwrite(vmcs::guest::TR_LIMIT, 0xffffu64);
        vmwrite(vmcs::guest::TR_ACCESS_RIGHTS, access_rights.0);

        unsafe {
            dr0_write(0);
            dr1_write(0);
            dr2_write(0);
            dr3_write(0);
            dr6_write(Dr6::from_bits_unchecked(0xffff0ff0));
            dr7_write(Dr7(0x400));
        };

        self.registers.r8 = 0;
        self.registers.r9 = 0;
        self.registers.r10 = 0;
        self.registers.r11 = 0;
        self.registers.r12 = 0;
        self.registers.r13 = 0;
        self.registers.r14 = 0;
        self.registers.r15 = 0;

        vmwrite(vmcs::guest::IA32_EFER_FULL, 0u64);
        vmwrite(vmcs::guest::FS_BASE, 0u64);
        vmwrite(vmcs::guest::GS_BASE, 0u64);

        let mut vmentry_controls = vmread(vmcs::control::VMENTRY_CONTROLS);
        vmentry_controls &= !(vmcs::control::EntryControls::IA32E_MODE_GUEST.bits() as u64);
        vmwrite(vmcs::control::VMENTRY_CONTROLS, vmentry_controls);

        // "All the processors on the system bus (...) execute the multiple processor
        //  (MP) initialization protocol. ... The application (non-BSP) processors
        //  (APs) go into a Wait For Startup IPI (SIPI) state while the BSP is executing
        //  initialization code."
        // See: 10.1 INITIALIZATION OVERVIEW
        //
        // "Upon receiving an INIT ..., the processor responds by beginning the
        //  initialization process of the processor core and the local APIC. The state
        //  of the local APIC following an INIT reset is the same as it is after a
        //  power-up or hardware reset ... . This state is also referred to at the
        //  "wait-for-SIPI" state."
        // See: 10.4.7.3 Local APIC State After an INIT Reset ("Wait-for-SIPI" State)
        vmwrite(
            vmcs::guest::ACTIVITY_STATE,
            GuestActivityState::WaitForSipi as u32,
        );
    }

    /// Handles VM-exit due to the Startup-IPI (SIPI) signal.
    fn handle_sipi_signal(&mut self) {
        // "For a start-up IPI (SIPI), the exit qualification contains the SIPI
        //  vector information in bits 7:0. Bits 63:8 of the exit qualification are
        //  cleared to 0."
        // See: 27.2.1 Basic VM-Exit Information
        let vector = vmread(vmcs::ro::EXIT_QUALIFICATION);

        // "At the end of the boot-strap procedure, the BSP sets ... broadcasts a
        //  SIPI message to all the APs in the system. Here, the SIPI message contains
        //  a vector to the BIOS AP initialization code (at 000VV000H, where VV is the
        //  vector contained in the SIPI message)."
        // See: 8.4.3 MP Initialization Protocol Algorithm for MP Systems
        vmwrite(vmcs::guest::CS_SELECTOR, vector << 8);
        vmwrite(vmcs::guest::CS_BASE, vector << 12);
        self.registers.rip = 0;
        vmwrite(vmcs::guest::RIP, self.registers.rip);

        // Done. Note that the 2nd SIPI will be ignored if that occurs after this.
        // "If a logical processor is not in the wait-for-SIPI activity state when a
        //  SIPI arrives, no VM exit occurs and the SIPI is discarded"
        // See: 25.2 OTHER CAUSES OF VM EXITS
        vmwrite(
            vmcs::guest::ACTIVITY_STATE,
            GuestActivityState::Active as u32,
        );
    }
}

struct SharedGuestData {
    msr_bitmaps: Box<Page>,
    epts: Box<Epts>,
}

static SHARED_GUEST_DATA: Lazy<SharedGuestData> = Lazy::new(|| {
    let mut epts = zeroed_box::<Epts>();
    epts.build_identify();

    SharedGuestData {
        msr_bitmaps: zeroed_box::<Page>(),
        epts,
    }
});

extern "C" {
    /// Runs the guest until VM-exit occurs.
    fn run_vmx_guest(registers: &mut Registers) -> u64;
}
global_asm!(include_str!("../capture_registers.inc"));
global_asm!(include_str!("run_guest.S"));

#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
enum VmxControl {
    PinBased,
    ProcessorBased,
    ProcessorBased2,
    ProcessorBased3,
    VmExit,
    VmEntry,
}

/// Represents the activity state of a logical processor in VMX operation.
#[allow(dead_code)]
#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum GuestActivityState {
    /// The logical processor is executing instructions normally.
    Active = 0,

    /// The logical processor is inactive because it executed the HLT instruction.
    Hlt = 1,

    /// The logical processor is inactive because it incurred a triple fault
    /// or some other serious error.
    Shutdown = 2,

    /// The logical processor is inactive because it is waiting for a startup-IPI (SIPI).
    WaitForSipi = 3,
}

/// Returns the CR0 value after the FIXED0 and FIXED1 MSR values are applied
/// for the guest.
fn get_adjusted_guest_cr0(cr0: Cr0) -> Cr0 {
    // Adjust the CR0 register according to the fixed0 and fixed1 MSR values.
    let mut new_cr0 = get_adjusted_cr0(cr0);

    // Read the secondary processor-based VM-execution controls to check for UnrestrictedGuest support.
    let secondary_proc_based_ctls2 = vmread(vmcs::control::SECONDARY_PROCBASED_EXEC_CONTROLS);
    let unrestricted_guest = secondary_proc_based_ctls2 as u32
        & vmcs::control::SecondaryControls::UNRESTRICTED_GUEST.bits()
        != 0;

    if unrestricted_guest {
        // if the guest is unrestricted, only set these bits if the guest requested them to be set
        new_cr0 &= !(Cr0::CR0_PROTECTED_MODE | Cr0::CR0_ENABLE_PAGING);
        new_cr0 |= cr0 & (Cr0::CR0_PROTECTED_MODE | Cr0::CR0_ENABLE_PAGING);
    }

    new_cr0
}

/// Returns the CR4 value after the FIXED0 and FIXED1 MSR values are applied
/// for the guest.
fn get_adjusted_guest_cr4(cr4: Cr4) -> Cr4 {
    get_adjusted_cr4(cr4)
}

/// Returns the CR0 value after the FIXED0 and FIXED1 MSR values are applied.
pub(crate) fn get_adjusted_cr0(cr0: Cr0) -> Cr0 {
    // In order to enter VMX operation, some bits in CR0 (and CR4) have to be
    // set or cleared as indicated by the FIXED0 and FIXED1 MSRs. The rule is
    // summarized as below (taking CR0 as an example):
    //
    //        IA32_VMX_CR0_FIXED0 IA32_VMX_CR0_FIXED1 Meaning
    // Bit X  1                   (Always 1)          The bit X of CR0 is fixed to 1
    // Bit X  0                   1                   The bit X of CR0 is flexible
    // Bit X  (Always 0)          0                   The bit X of CR0 is fixed to 0
    //
    // Some UEFI implementations do not fullfil those requirements for CR0 and
    // need adjustments. The requirements for CR4 are always satisfied as far
    // as the author has experimented (although not guaranteed).
    //
    // See: A.7 VMX-FIXED BITS IN CR0
    // See: A.8 VMX-FIXED BITS IN CR4
    let fixed0 = unsafe { Cr0::from_bits_unchecked(rdmsr(x86::msr::IA32_VMX_CR0_FIXED0) as _) };
    let fixed1 = unsafe { Cr0::from_bits_unchecked(rdmsr(x86::msr::IA32_VMX_CR0_FIXED1) as _) };
    (cr0 & fixed1) | fixed0
}

/// Returns the CR4 value after the FIXED0 and FIXED1 MSR values are applied.
pub(crate) fn get_adjusted_cr4(cr4: Cr4) -> Cr4 {
    let fixed0 = unsafe { Cr4::from_bits_unchecked(rdmsr(x86::msr::IA32_VMX_CR4_FIXED0) as _) };
    let fixed1 = unsafe { Cr4::from_bits_unchecked(rdmsr(x86::msr::IA32_VMX_CR4_FIXED1) as _) };
    (cr4 & fixed1) | fixed0
}

bitfield::bitfield! {
    /// Represents the VMX Segment Access Rights, as detailed in Intel's Software Developer's Manual,
    /// specifically in Section 25.4.1 Guest Register State.
    ///
    /// This struct encapsulates the access rights associated with a segment selector in a VMX operation,
    /// which includes properties such as the segment type, privilege level, and presence. These rights are
    /// crucial for the proper setup and control of guest and host segments in virtualization environments.
    ///
    /// Reference: Intel® 64 and IA-32 Architectures Software Developer's Manual.
    #[derive(Clone, Copy)]
    struct VmxSegmentAccessRights(u32);
    impl Debug;

    /// Extracts or sets the segment type (bits 3:0). This field specifies the type of segment or gate descriptor,
    /// including data, code, system segments, etc. The exact meaning of these bits varies based on the descriptor
    /// type (system, code, or data).
    segment_type, set_segment_type: 3, 0;

    /// Indicates the descriptor type (bit 4). A value of 0 signifies a system descriptor (like LDT or TSS),
    /// while 1 signifies a code or data descriptor. This distinction affects the interpretation of other fields
    /// in the descriptor.
    descriptor_type, set_descriptor_type: 4;

    /// Represents the Descriptor Privilege Level (DPL, bits 6:5). This specifies the privilege level of the segment,
    /// ranging from 0 (highest privilege, kernel) to 3 (lowest privilege, user applications).
    descriptor_privilege_level, set_descriptor_privilege_level: 6, 5;

    /// Indicates whether the segment is present (bit 7). If this bit is cleared, any attempt to access the segment
    /// results in a segment not present exception (#NP). This bit is used to control loading of segments that
    /// might not be currently available in memory.
    present, set_present: 7;

    /// Reserved bits (11:8). These bits are reserved and should not be modified. They are present for alignment
    /// and future compatibility.

    /// Available for use by system software (bit 12). This bit is available for use by system software and does not
    /// have a defined meaning in the VMX operation. It can be used by hypervisors to store additional information.
    available, set_available: 12;

    /// Indicates 64-bit mode active (for CS only, bit 13). For the CS segment, setting this bit indicates that
    /// the segment is running in 64-bit mode (long mode). This bit is ignored for other segment types.
    long_mode, set_long_mode: 13;

    /// Default operation size (D/B, bit 14). For code segments, this bit controls the default operation size
    /// (0 for 16-bit, 1 for 32-bit). For stack segments (SS), it controls the stack pointer size.
    default_big, set_default_big: 14;

    /// Granularity (bit 15). When set, the segment limit is scaled by 4K, allowing for larger segments.
    /// This bit is used in conjunction with the segment limit field to determine the actual size of the segment.
    granularity, set_granularity: 15;

    /// Indicates if the segment is unusable (bit 16). If set, the segment cannot be used for memory access.
    /// An unusable segment is typically one that has been loaded with a null selector.
    unusable, set_unusable: 16;

    // Reserved bits (31:17). These bits are reserved for future use and should always be cleared to ensure
    // compatibility with future processors.
}

#[derive(derive_deref::Deref, derive_deref::DerefMut)]
struct Vmcs {
    ptr: Box<VmcsRaw>,
}

impl Vmcs {
    fn new() -> Self {
        let mut vmcs = zeroed_box::<VmcsRaw>();
        vmcs.revision_id = rdmsr(x86::msr::IA32_VMX_BASIC) as _;
        vmclear(&mut vmcs);
        Self { ptr: vmcs }
    }
}

/// The region of memory that the logical processor uses to represent a virtual
/// CPU. Called virtual-machine control data structure (VMCS).
///
/// See: 25.2 FORMAT OF THE VMCS REGION
#[derive(Debug)]
#[repr(C, align(4096))]
struct VmcsRaw {
    revision_id: u32,
    abort_indicator: u32,
    #[debug(skip)]
    data: [u8; 4088],
}
const _: () = assert!(core::mem::size_of::<VmcsRaw>() == BASE_PAGE_SIZE);

/// The wrapper of the VMCLEAR instruction.
fn vmclear(vmcs_region: &mut VmcsRaw) {
    let va = vmcs_region as *const _;
    let pa = platform_ops::get().pa(va as *const _);
    unsafe { x86::bits64::vmx::vmclear(pa).unwrap() };
}

/// The wrapper of the VMPTRLD instruction.
fn vmptrld(vmcs_region: &mut VmcsRaw) {
    let va = vmcs_region as *const _;
    let pa = platform_ops::get().pa(va as *const _);
    unsafe { x86::bits64::vmx::vmptrld(pa).unwrap() }
}

/// The wrapper of the VMREAD instruction.
fn vmread(encoding: u32) -> u64 {
    unsafe { x86::bits64::vmx::vmread(encoding) }.unwrap()
}

/// The wrapper of the VMWRITE instruction.
fn vmwrite<T: Into<u64>>(encoding: u32, value: T)
where
    u64: From<T>,
{
    let value = u64::from(value);
    unsafe { x86::bits64::vmx::vmwrite(encoding, value) }
        .unwrap_or_else(|_| panic!("Could not write {value:x?} to {encoding:x?}"));
}

/// Checks that the latest VMX instruction succeeded.
///
/// See: 31.2 CONVENTIONS
fn vmx_succeed(flags: RFlags) -> Result<(), String> {
    if flags.contains(RFlags::FLAGS_ZF) {
        // See: 31.4 VM INSTRUCTION ERROR NUMBERS
        Err(format!(
            "VmFailValid with {}",
            vmread(vmcs::ro::VM_INSTRUCTION_ERROR)
        ))
    } else if flags.contains(RFlags::FLAGS_CF) {
        Err("VmFailInvalid".to_string())
    } else {
        Ok(())
    }
}

// VMCS encodings not defined in the x86 crate.
const VMCS_CONTROL_HLAT_PREFIX_SIZE: u32 = 0x6;
const VMCS_CONTROL_LAST_PID_POINTER_INDEX: u32 = 0x8;
const VMCS_GUEST_UINV: u32 = 0x814;
const VMCS_CONTROL_TERTIARY_PROCESSOR_BASED_VM_EXECUTION_CONTROLS: u32 = 0x2034;
const VMCS_CONTROL_ENCLV_EXITING_BITMAP: u32 = 0x2036;
const VMCS_CONTROL_LOW_PASID_DIRECTORY_ADDRESS: u32 = 0x2038;
const VMCS_CONTROL_HIGH_PASID_DIRECTORY_ADDRESS: u32 = 0x203A;
const VMCS_CONTROL_SHARED_EPT_POINTER: u32 = 0x203C;
const VMCS_CONTROL_PCONFIG_EXITING_BITMAP: u32 = 0x203E;
const VMCS_CONTROL_HLATP: u32 = 0x2040;
const VMCS_CONTROL_PID_POINTER_TABLE_ADDRESS: u32 = 0x2042;
const VMCS_CONTROL_SECONDARY_VM_EXIT_CONTROLS: u32 = 0x2044;
const VMCS_CONTROL_IA32_SPEC_CTRL_MASK: u32 = 0x204A;
const VMCS_CONTROL_IA32_SPEC_CTRL_SHADOW: u32 = 0x204C;
const VMCS_GUEST_IA32_LBR_CTL: u32 = 0x2816;
const VMCS_GUEST_IA32_PKRS: u32 = 0x2818;
const VMCS_HOST_IA32_PKRS: u32 = 0x2C06;
const VMCS_CONTROL_INSTRUCTION_TIMEOUT_CONTROL: u32 = 0x4024;
const VMCS_GUEST_IA32_S_CET: u32 = 0x6828;
const VMCS_GUEST_SSP: u32 = 0x682A;
const VMCS_GUEST_IA32_INTERRUPT_SSP_TABLE_ADDR: u32 = 0x682C;
const VMCS_HOST_IA32_S_CET: u32 = 0x6C18;
const VMCS_HOST_SSP: u32 = 0x6C1A;
const VMCS_HOST_IA32_INTERRUPT_SSP_TABLE_ADDR: u32 = 0x6C1C;

impl core::fmt::Debug for Vmcs {
    #[rustfmt::skip]
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, format: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        /// The wrapper of the VMREAD instruction. Returns zero on error.
        fn vmread_relaxed(encoding: u32) -> u64 {
            unsafe { x86::bits64::vmx::vmread(encoding) }.unwrap_or(0)
        }

        // Dump the current VMCS. Not that this is not exhaustive.
        format.debug_struct("Vmcs")
        .field("Current VMCS                                   ", &addr_of!(self.revision_id))
        .field("Revision ID                                    ", &self.revision_id)

        // 16-Bit Guest-State Fields
        .field("Guest ES Selector                              ", &vmread_relaxed(vmcs::guest::ES_SELECTOR))
        .field("Guest CS Selector                              ", &vmread_relaxed(vmcs::guest::CS_SELECTOR))
        .field("Guest SS Selector                              ", &vmread_relaxed(vmcs::guest::SS_SELECTOR))
        .field("Guest DS Selector                              ", &vmread_relaxed(vmcs::guest::DS_SELECTOR))
        .field("Guest FS Selector                              ", &vmread_relaxed(vmcs::guest::FS_SELECTOR))
        .field("Guest GS Selector                              ", &vmread_relaxed(vmcs::guest::GS_SELECTOR))
        .field("Guest LDTR Selector                            ", &vmread_relaxed(vmcs::guest::LDTR_SELECTOR))
        .field("Guest TR Selector                              ", &vmread_relaxed(vmcs::guest::TR_SELECTOR))
        .field("Guest interrupt status                         ", &vmread_relaxed(vmcs::guest::INTERRUPT_STATUS))
        .field("PML index                                      ", &vmread_relaxed(vmcs::guest::PML_INDEX))
        .field("Guest UINV                                     ", &vmread_relaxed(VMCS_GUEST_UINV))

        // 64-Bit Guest-State Fields
        .field("VMCS link pointer                              ", &vmread_relaxed(vmcs::guest::LINK_PTR_FULL))
        .field("Guest IA32_DEBUGCTL                            ", &vmread_relaxed(vmcs::guest::IA32_DEBUGCTL_FULL))
        .field("Guest IA32_PAT                                 ", &vmread_relaxed(vmcs::guest::IA32_PAT_FULL))
        .field("Guest IA32_EFER                                ", &vmread_relaxed(vmcs::guest::IA32_EFER_FULL))
        .field("Guest IA32_PERF_GLOBAL_CTRL                    ", &vmread_relaxed(vmcs::guest::IA32_PERF_GLOBAL_CTRL_FULL))
        .field("Guest PDPTE0                                   ", &vmread_relaxed(vmcs::guest::PDPTE0_FULL))
        .field("Guest PDPTE1                                   ", &vmread_relaxed(vmcs::guest::PDPTE1_FULL))
        .field("Guest PDPTE2                                   ", &vmread_relaxed(vmcs::guest::PDPTE2_FULL))
        .field("Guest PDPTE3                                   ", &vmread_relaxed(vmcs::guest::PDPTE3_FULL))
        .field("Guest IA32_BNDCFGS                             ", &vmread_relaxed(vmcs::guest::IA32_BNDCFGS_FULL))
        .field("Guest IA32_RTIT_CTL                            ", &vmread_relaxed(vmcs::guest::IA32_RTIT_CTL_FULL))
        .field("Guest IA32_LBR_CTL                             ", &vmread_relaxed(VMCS_GUEST_IA32_LBR_CTL))
        .field("Guest IA32_PKRS                                ", &vmread_relaxed(VMCS_GUEST_IA32_PKRS))

        // 32-Bit Guest-State Fields
        .field("Guest ES Limit                                 ", &vmread_relaxed(vmcs::guest::ES_LIMIT))
        .field("Guest CS Limit                                 ", &vmread_relaxed(vmcs::guest::CS_LIMIT))
        .field("Guest SS Limit                                 ", &vmread_relaxed(vmcs::guest::SS_LIMIT))
        .field("Guest DS Limit                                 ", &vmread_relaxed(vmcs::guest::DS_LIMIT))
        .field("Guest FS Limit                                 ", &vmread_relaxed(vmcs::guest::FS_LIMIT))
        .field("Guest GS Limit                                 ", &vmread_relaxed(vmcs::guest::GS_LIMIT))
        .field("Guest LDTR Limit                               ", &vmread_relaxed(vmcs::guest::LDTR_LIMIT))
        .field("Guest TR Limit                                 ", &vmread_relaxed(vmcs::guest::TR_LIMIT))
        .field("Guest GDTR limit                               ", &vmread_relaxed(vmcs::guest::GDTR_LIMIT))
        .field("Guest IDTR limit                               ", &vmread_relaxed(vmcs::guest::IDTR_LIMIT))
        .field("Guest ES access rights                         ", &vmread_relaxed(vmcs::guest::ES_ACCESS_RIGHTS))
        .field("Guest CS access rights                         ", &vmread_relaxed(vmcs::guest::CS_ACCESS_RIGHTS))
        .field("Guest SS access rights                         ", &vmread_relaxed(vmcs::guest::SS_ACCESS_RIGHTS))
        .field("Guest DS access rights                         ", &vmread_relaxed(vmcs::guest::DS_ACCESS_RIGHTS))
        .field("Guest FS access rights                         ", &vmread_relaxed(vmcs::guest::FS_ACCESS_RIGHTS))
        .field("Guest GS access rights                         ", &vmread_relaxed(vmcs::guest::GS_ACCESS_RIGHTS))
        .field("Guest LDTR access rights                       ", &vmread_relaxed(vmcs::guest::LDTR_ACCESS_RIGHTS))
        .field("Guest TR access rights                         ", &vmread_relaxed(vmcs::guest::TR_ACCESS_RIGHTS))
        .field("Guest interruptibility state                   ", &vmread_relaxed(vmcs::guest::INTERRUPTIBILITY_STATE))
        .field("Guest activity state                           ", &vmread_relaxed(vmcs::guest::ACTIVITY_STATE))
        .field("Guest SMBASE                                   ", &vmread_relaxed(vmcs::guest::SMBASE))
        .field("Guest IA32_SYSENTER_CS                         ", &vmread_relaxed(vmcs::guest::IA32_SYSENTER_CS))
        .field("VMX-preemption timer value                     ", &vmread_relaxed(vmcs::guest::VMX_PREEMPTION_TIMER_VALUE))

        // Natural-Width Guest-State Fields
        .field("Guest CR0                                      ", &vmread_relaxed(vmcs::guest::CR0))
        .field("Guest CR3                                      ", &vmread_relaxed(vmcs::guest::CR3))
        .field("Guest CR4                                      ", &vmread_relaxed(vmcs::guest::CR4))
        .field("Guest ES Base                                  ", &vmread_relaxed(vmcs::guest::ES_BASE))
        .field("Guest CS Base                                  ", &vmread_relaxed(vmcs::guest::CS_BASE))
        .field("Guest SS Base                                  ", &vmread_relaxed(vmcs::guest::SS_BASE))
        .field("Guest DS Base                                  ", &vmread_relaxed(vmcs::guest::DS_BASE))
        .field("Guest FS Base                                  ", &vmread_relaxed(vmcs::guest::FS_BASE))
        .field("Guest GS Base                                  ", &vmread_relaxed(vmcs::guest::GS_BASE))
        .field("Guest LDTR base                                ", &vmread_relaxed(vmcs::guest::LDTR_BASE))
        .field("Guest TR base                                  ", &vmread_relaxed(vmcs::guest::TR_BASE))
        .field("Guest GDTR base                                ", &vmread_relaxed(vmcs::guest::GDTR_BASE))
        .field("Guest IDTR base                                ", &vmread_relaxed(vmcs::guest::IDTR_BASE))
        .field("Guest DR7                                      ", &vmread_relaxed(vmcs::guest::DR7))
        .field("Guest RSP                                      ", &vmread_relaxed(vmcs::guest::RSP))
        .field("Guest RIP                                      ", &vmread_relaxed(vmcs::guest::RIP))
        .field("Guest RFLAGS                                   ", &vmread_relaxed(vmcs::guest::RFLAGS))
        .field("Guest pending debug exceptions                 ", &vmread_relaxed(vmcs::guest::PENDING_DBG_EXCEPTIONS))
        .field("Guest IA32_SYSENTER_ESP                        ", &vmread_relaxed(vmcs::guest::IA32_SYSENTER_ESP))
        .field("Guest IA32_SYSENTER_EIP                        ", &vmread_relaxed(vmcs::guest::IA32_SYSENTER_EIP))
        .field("Guest IA32_S_CET                               ", &vmread_relaxed(VMCS_GUEST_IA32_S_CET))
        .field("Guest SSP                                      ", &vmread_relaxed(VMCS_GUEST_SSP))
        .field("Guest IA32_INTERRUPT_SSP_TABLE_ADDR            ", &vmread_relaxed(VMCS_GUEST_IA32_INTERRUPT_SSP_TABLE_ADDR))

        // 16-Bit Host-State Fields
        .field("Host ES Selector                               ", &vmread_relaxed(vmcs::host::ES_SELECTOR))
        .field("Host CS Selector                               ", &vmread_relaxed(vmcs::host::CS_SELECTOR))
        .field("Host SS Selector                               ", &vmread_relaxed(vmcs::host::SS_SELECTOR))
        .field("Host DS Selector                               ", &vmread_relaxed(vmcs::host::DS_SELECTOR))
        .field("Host FS Selector                               ", &vmread_relaxed(vmcs::host::FS_SELECTOR))
        .field("Host GS Selector                               ", &vmread_relaxed(vmcs::host::GS_SELECTOR))
        .field("Host TR Selector                               ", &vmread_relaxed(vmcs::host::TR_SELECTOR))

        // 64-Bit Host-State Fields
        .field("Host IA32_PAT                                  ", &vmread_relaxed(vmcs::host::IA32_PAT_FULL))
        .field("Host IA32_EFER                                 ", &vmread_relaxed(vmcs::host::IA32_EFER_FULL))
        .field("Host IA32_PERF_GLOBAL_CTRL                     ", &vmread_relaxed(vmcs::host::IA32_PERF_GLOBAL_CTRL_FULL))
        .field("Host IA32_PKRS                                 ", &vmread_relaxed(VMCS_HOST_IA32_PKRS))

        // 32-Bit Host-State Fields
        .field("Host IA32_SYSENTER_CS                          ", &vmread_relaxed(vmcs::host::IA32_SYSENTER_CS))

        // Natural-Width Host-State Fields
        .field("Host CR0                                       ", &vmread_relaxed(vmcs::host::CR0))
        .field("Host CR3                                       ", &vmread_relaxed(vmcs::host::CR3))
        .field("Host CR4                                       ", &vmread_relaxed(vmcs::host::CR4))
        .field("Host FS Base                                   ", &vmread_relaxed(vmcs::host::FS_BASE))
        .field("Host GS Base                                   ", &vmread_relaxed(vmcs::host::GS_BASE))
        .field("Host TR base                                   ", &vmread_relaxed(vmcs::host::TR_BASE))
        .field("Host GDTR base                                 ", &vmread_relaxed(vmcs::host::GDTR_BASE))
        .field("Host IDTR base                                 ", &vmread_relaxed(vmcs::host::IDTR_BASE))
        .field("Host IA32_SYSENTER_ESP                         ", &vmread_relaxed(vmcs::host::IA32_SYSENTER_ESP))
        .field("Host IA32_SYSENTER_EIP                         ", &vmread_relaxed(vmcs::host::IA32_SYSENTER_EIP))
        .field("Host RSP                                       ", &vmread_relaxed(vmcs::host::RSP))
        .field("Host RIP                                       ", &vmread_relaxed(vmcs::host::RIP))
        .field("Host IA32_S_CET                                ", &vmread_relaxed(VMCS_HOST_IA32_S_CET))
        .field("Host SSP                                       ", &vmread_relaxed(VMCS_HOST_SSP))
        .field("Host IA32_INTERRUPT_SSP_TABLE_ADDR             ", &vmread_relaxed(VMCS_HOST_IA32_INTERRUPT_SSP_TABLE_ADDR))

        // 16-Bit Control Fields
        .field("Virtual-processor identifier                   ", &vmread_relaxed(vmcs::control::VPID))
        .field("Posted-interrupt notification vector           ", &vmread_relaxed(vmcs::control::POSTED_INTERRUPT_NOTIFICATION_VECTOR))
        .field("EPTP index                                     ", &vmread_relaxed(vmcs::control::EPTP_INDEX))
        .field("HLAT prefix size                               ", &vmread_relaxed(VMCS_CONTROL_HLAT_PREFIX_SIZE))
        .field("Last PID-pointer index                         ", &vmread_relaxed(VMCS_CONTROL_LAST_PID_POINTER_INDEX))

        // 64-Bit Control Fields
        .field("Address of I/O bitmap A                        ", &vmread_relaxed(vmcs::control::IO_BITMAP_A_ADDR_FULL))
        .field("Address of I/O bitmap B                        ", &vmread_relaxed(vmcs::control::IO_BITMAP_B_ADDR_FULL))
        .field("Address of MSR bitmaps                         ", &vmread_relaxed(vmcs::control::MSR_BITMAPS_ADDR_FULL))
        .field("VM-exit MSR-store address                      ", &vmread_relaxed(vmcs::control::VMEXIT_MSR_STORE_ADDR_FULL))
        .field("VM-exit MSR-load address                       ", &vmread_relaxed(vmcs::control::VMEXIT_MSR_LOAD_ADDR_FULL))
        .field("VM-entry MSR-load address                      ", &vmread_relaxed(vmcs::control::VMENTRY_MSR_LOAD_ADDR_FULL))
        .field("Executive-VMCS pointer                         ", &vmread_relaxed(vmcs::control::EXECUTIVE_VMCS_PTR_FULL))
        .field("PML address                                    ", &vmread_relaxed(vmcs::control::PML_ADDR_FULL))
        .field("TSC offset                                     ", &vmread_relaxed(vmcs::control::TSC_OFFSET_FULL))
        .field("Virtual-APIC address                           ", &vmread_relaxed(vmcs::control::VIRT_APIC_ADDR_FULL))
        .field("APIC-access address                            ", &vmread_relaxed(vmcs::control::APIC_ACCESS_ADDR_FULL))
        .field("Posted-interrupt descriptor address            ", &vmread_relaxed(vmcs::control::POSTED_INTERRUPT_DESC_ADDR_FULL))
        .field("VM-function controls                           ", &vmread_relaxed(vmcs::control::VM_FUNCTION_CONTROLS_FULL))
        .field("EPT pointer                                    ", &vmread_relaxed(vmcs::control::EPTP_FULL))
        .field("EOI-exit bitmap 0                              ", &vmread_relaxed(vmcs::control::EOI_EXIT0_FULL))
        .field("EOI-exit bitmap 1                              ", &vmread_relaxed(vmcs::control::EOI_EXIT1_FULL))
        .field("EOI-exit bitmap 2                              ", &vmread_relaxed(vmcs::control::EOI_EXIT2_FULL))
        .field("EOI-exit bitmap 3                              ", &vmread_relaxed(vmcs::control::EOI_EXIT3_FULL))
        .field("EPTP-list address                              ", &vmread_relaxed(vmcs::control::EPTP_LIST_ADDR_FULL))
        .field("VMREAD-bitmap address                          ", &vmread_relaxed(vmcs::control::VMREAD_BITMAP_ADDR_FULL))
        .field("VMWRITE-bitmap address                         ", &vmread_relaxed(vmcs::control::VMWRITE_BITMAP_ADDR_FULL))
        .field("Virtualization-exception information address   ", &vmread_relaxed(vmcs::control::VIRT_EXCEPTION_INFO_ADDR_FULL))
        .field("XSS-exiting bitmap                             ", &vmread_relaxed(vmcs::control::XSS_EXITING_BITMAP_FULL))
        .field("ENCLS-exiting bitmap                           ", &vmread_relaxed(vmcs::control::ENCLS_EXITING_BITMAP_FULL))
        .field("Sub-page-permission-table pointer              ", &vmread_relaxed(vmcs::control::SUBPAGE_PERM_TABLE_PTR_FULL))
        .field("TSC multiplier                                 ", &vmread_relaxed(vmcs::control::TSC_MULTIPLIER_FULL))
        .field("Tertiary processor-based VM-execution controls ", &vmread_relaxed(VMCS_CONTROL_TERTIARY_PROCESSOR_BASED_VM_EXECUTION_CONTROLS))
        .field("ENCLV-exiting bitmap                           ", &vmread_relaxed(VMCS_CONTROL_ENCLV_EXITING_BITMAP))
        .field("Low PASID directory address                    ", &vmread_relaxed(VMCS_CONTROL_LOW_PASID_DIRECTORY_ADDRESS))
        .field("High PASID directory address                   ", &vmread_relaxed(VMCS_CONTROL_HIGH_PASID_DIRECTORY_ADDRESS))
        .field("Shared EPT pointer                             ", &vmread_relaxed(VMCS_CONTROL_SHARED_EPT_POINTER))
        .field("PCONFIG-exiting bitmap                         ", &vmread_relaxed(VMCS_CONTROL_PCONFIG_EXITING_BITMAP))
        .field("HLATP                                          ", &vmread_relaxed(VMCS_CONTROL_HLATP))
        .field("PID-pointer table address                      ", &vmread_relaxed(VMCS_CONTROL_PID_POINTER_TABLE_ADDRESS))
        .field("Secondary VM-exit controls                     ", &vmread_relaxed(VMCS_CONTROL_SECONDARY_VM_EXIT_CONTROLS))
        .field("IA32_SPEC_CTRL mask                            ", &vmread_relaxed(VMCS_CONTROL_IA32_SPEC_CTRL_MASK))
        .field("IA32_SPEC_CTRL shadow                          ", &vmread_relaxed(VMCS_CONTROL_IA32_SPEC_CTRL_SHADOW))

        // 32-Bit Control Fields
        .field("Pin-based VM-execution controls                ", &vmread_relaxed(vmcs::control::PINBASED_EXEC_CONTROLS))
        .field("Primary processor-based VM-execution controls  ", &vmread_relaxed(vmcs::control::PRIMARY_PROCBASED_EXEC_CONTROLS))
        .field("Exception bitmap                               ", &vmread_relaxed(vmcs::control::EXCEPTION_BITMAP))
        .field("Page-fault error-code mask                     ", &vmread_relaxed(vmcs::control::PAGE_FAULT_ERR_CODE_MASK))
        .field("Page-fault error-code match                    ", &vmread_relaxed(vmcs::control::PAGE_FAULT_ERR_CODE_MATCH))
        .field("CR3-target count                               ", &vmread_relaxed(vmcs::control::CR3_TARGET_COUNT))
        .field("Primary VM-exit controls                       ", &vmread_relaxed(vmcs::control::VMEXIT_CONTROLS))
        .field("VM-exit MSR-store count                        ", &vmread_relaxed(vmcs::control::VMEXIT_MSR_STORE_COUNT))
        .field("VM-exit MSR-load count                         ", &vmread_relaxed(vmcs::control::VMEXIT_MSR_LOAD_COUNT))
        .field("VM-entry controls                              ", &vmread_relaxed(vmcs::control::VMENTRY_CONTROLS))
        .field("VM-entry MSR-load count                        ", &vmread_relaxed(vmcs::control::VMENTRY_MSR_LOAD_COUNT))
        .field("VM-entry interruption-information field        ", &vmread_relaxed(vmcs::control::VMENTRY_INTERRUPTION_INFO_FIELD))
        .field("VM-entry exception error code                  ", &vmread_relaxed(vmcs::control::VMENTRY_EXCEPTION_ERR_CODE))
        .field("VM-entry instruction length                    ", &vmread_relaxed(vmcs::control::VMENTRY_INSTRUCTION_LEN))
        .field("TPR threshold                                  ", &vmread_relaxed(vmcs::control::TPR_THRESHOLD))
        .field("Secondary processor-based VM-execution controls", &vmread_relaxed(vmcs::control::SECONDARY_PROCBASED_EXEC_CONTROLS))
        .field("PLE_Gap                                        ", &vmread_relaxed(vmcs::control::PLE_GAP))
        .field("PLE_Window                                     ", &vmread_relaxed(vmcs::control::PLE_WINDOW))
        .field("Instruction-timeout control                    ", &vmread_relaxed(VMCS_CONTROL_INSTRUCTION_TIMEOUT_CONTROL))

        // Natural-Width Control Fields
        .field("CR0 guest/host mask                            ", &vmread_relaxed(vmcs::control::CR0_GUEST_HOST_MASK))
        .field("CR4 guest/host mask                            ", &vmread_relaxed(vmcs::control::CR4_GUEST_HOST_MASK))
        .field("CR0 read shadow                                ", &vmread_relaxed(vmcs::control::CR0_READ_SHADOW))
        .field("CR4 read shadow                                ", &vmread_relaxed(vmcs::control::CR4_READ_SHADOW))
        .field("CR3-target value 0                             ", &vmread_relaxed(vmcs::control::CR3_TARGET_VALUE0))
        .field("CR3-target value 1                             ", &vmread_relaxed(vmcs::control::CR3_TARGET_VALUE1))
        .field("CR3-target value 2                             ", &vmread_relaxed(vmcs::control::CR3_TARGET_VALUE2))
        .field("CR3-target value 3                             ", &vmread_relaxed(vmcs::control::CR3_TARGET_VALUE3))

        // 16-Bit Read-Only Data Fields

        // 64-Bit Read-Only Data Fields
        .field("Guest-physical address                         ", &vmread_relaxed(vmcs::ro::GUEST_PHYSICAL_ADDR_FULL))

        // 32-Bit Read-Only Data Fields
        .field("VM-instruction error                           ", &vmread_relaxed(vmcs::ro::VM_INSTRUCTION_ERROR))
        .field("Exit reason                                    ", &vmread_relaxed(vmcs::ro::EXIT_REASON))
        .field("VM-exit interruption information               ", &vmread_relaxed(vmcs::ro::VMEXIT_INTERRUPTION_INFO))
        .field("VM-exit interruption error code                ", &vmread_relaxed(vmcs::ro::VMEXIT_INTERRUPTION_ERR_CODE))
        .field("IDT-vectoring information field                ", &vmread_relaxed(vmcs::ro::IDT_VECTORING_INFO))
        .field("IDT-vectoring error code                       ", &vmread_relaxed(vmcs::ro::IDT_VECTORING_ERR_CODE))
        .field("VM-exit instruction length                     ", &vmread_relaxed(vmcs::ro::VMEXIT_INSTRUCTION_LEN))
        .field("VM-exit instruction information                ", &vmread_relaxed(vmcs::ro::VMEXIT_INSTRUCTION_INFO))

        // Natural-Width Read-Only Data Fields
        .field("Exit qualification                             ", &vmread_relaxed(vmcs::ro::EXIT_QUALIFICATION))
        .field("I/O RCX                                        ", &vmread_relaxed(vmcs::ro::IO_RCX))
        .field("I/O RSI                                        ", &vmread_relaxed(vmcs::ro::IO_RSI))
        .field("I/O RDI                                        ", &vmread_relaxed(vmcs::ro::IO_RDI))
        .field("I/O RIP                                        ", &vmread_relaxed(vmcs::ro::IO_RIP))
        .field("Guest-linear address                           ", &vmread_relaxed(vmcs::ro::GUEST_LINEAR_ADDR))
        .finish_non_exhaustive()
    }
}
