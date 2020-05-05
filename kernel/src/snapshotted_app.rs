//! Fuzzable snapshotted application backed by an Intel VT-x VM

use core::mem::size_of;
use core::cell::Cell;
use core::convert::TryInto;
use core::sync::atomic::{AtomicU64, Ordering};
use core::alloc::Layout;
use alloc::vec::Vec;
use alloc::sync::Arc;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::borrow::Cow;
use alloc::collections::{BTreeMap};

use crate::mm;
use crate::time;
use crate::ept::{EPT_READ, EPT_WRITE, EPT_EXEC, EPT_USER_EXEC};
use crate::ept::EPT_MEMTYPE_WB;
use crate::net::{NetDevice, UdpBind, UdpAddress};
use crate::vtx::{Vm, VmExit, Register, Exception, FxSave};
use crate::net::netmapping::NetMapping;
use crate::core_locals::LockInterrupts;
use crate::paging::*;

use aht::Aht;
use falktp::{CoverageRecord, ServerMessage};
use noodle::*;
use lockcell::LockCell;
use atomicvec::AtomicVec;
use page_table::{PhysAddr, VirtAddr, PhysMem, PageType, Mapping};

/// Trait to allow conversion of slices of bytes to primitives and back
/// generically
pub unsafe trait Primitive: Default + Sized {
    fn cast(&self) -> &[u8];
    fn cast_mut(&mut self) -> &mut [u8];
}

macro_rules! primitive {
    ($ty:ty) => {
        unsafe impl Primitive for $ty {
            fn cast(&self) -> &[u8] {
                unsafe {
                    core::slice::from_raw_parts(
                        self as *const $ty as *const u8, size_of::<$ty>())
                }
            }
            
            fn cast_mut(&mut self) -> &mut [u8] {
                unsafe {
                    core::slice::from_raw_parts_mut(
                        self as *mut $ty as *mut u8, size_of::<$ty>())
                }
            }
        }
    }
}

primitive!(u8);
primitive!(u16);
primitive!(u32);
primitive!(u64);
primitive!(u128);
primitive!(i8);
primitive!(i16);
primitive!(i32);
primitive!(i64);
primitive!(i128);

/// Different types of paging modes
#[derive(Clone, Copy)]
pub enum PagingMode {
    /// 32-bit paging without PAE
    Bits32,

    /// 32-bit paging with PAE
    Bits32Pae,

    /// 4-level 64-bit paging
    Bits64,
}

/// Different x86 segments
#[derive(Clone, Copy)]
pub enum Segment {
    Es,
    Ds,
    Fs,
    Gs,
    Ss,
    Cs,
}

/// Different addresses for x86
#[derive(Clone, Copy)]
pub enum Address {
    /// Physical linear address
    PhysicalLinear {
        addr: u64
    },

    /// Physical address with a segment base and an offset
    PhysicalSegOff {
        seg: Segment,
        off: u64,
    },

    /// Virtual address with a segment base, offset, paging mode, and a page
    /// table
    Virtual {
        seg:  Segment,
        off:  u64,
        mode: PagingMode,
        cr3:  u64
    },

    /// Linear address with a paging mode and a page table
    Linear {
        addr: u64,
        mode: PagingMode,
        cr3:  u64
    },
}

/// Number of microseconds to wait before syncing worker statistics into the
/// `FuzzTarget`
///
/// This is used to reduce the frequency which workers sync with the master,
/// to cut down on the lock contention
const STATISTIC_SYNC_INTERVAL: u64 = 100_000;

/// A random number generator based off of xorshift64
pub struct Rng(Cell<u64>);

impl Rng {
    /// Create a new randomly seeded `Rng`
    pub fn new() -> Self {
        let rng = Rng(Cell::new(((core!().id as u64) << 48) | cpu::rdtsc()));
        for _ in 0..1000 { rng.rand(); }
        rng
    }

    /// Get the next random number from the random number generator
    pub fn rand(&self) -> usize {
        let orig_seed = self.0.get();

        let mut seed = orig_seed;
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 43;
        self.0.set(seed);

        orig_seed as usize
    }
}

/// Statistics collected about number of fuzz cases and VM exits
///
/// This structure is synced on `STATISTIC_SYNC_INTERVAL` from the workers to
/// the master `FuzzTarget`. This interval based syncing ensures that the
/// lock contention is kept low, regardless of number of fuzz cases or cores.
#[derive(Default, Debug)]
pub struct Statistics {
    /// Number of fuzz cases performed on the target
    fuzz_cases: u64,

    /// Number of cycles spent resetting the VM
    reset_cycles: u64,

    /// Total cycles spent fuzzing
    total_cycles: u64,

    /// Number of cycles spent inside the VM
    vm_cycles: u64,

    /// Number of VM exits
    vmexits: u64,
}

impl Statistics {
    /// Sync the statistics in `self` into `master`, resetting `self`'s
    /// statistics back to 0 such that the syncing cycle can repeat.
    fn sync_into(&mut self, master: &mut Statistics) {
        // Merge number of fuzz cases
        master.fuzz_cases += self.fuzz_cases;
        master.reset_cycles += self.reset_cycles;
        master.vm_cycles += self.vm_cycles;
        master.total_cycles += self.total_cycles;
        master.vmexits += self.vmexits;

        // Reset our statistics
        *self = Default::default();
    }
}

/// Network backed VM memory information
struct NetBacking<'a> {
    /// Raw guest physical memory backing the snasphot
    memory: NetMapping<'a>,

    /// Mapping of physical region base to offset into `memory` and the end
    /// (inclusive) of the region
    phys_ranges: BTreeMap<u64, (usize, u64)>,
}

pub struct Worker<'a, C> {
    /// Master worker that we are forked from
    master: Option<Arc<Self>>,

    /// Network mapped memory for the VM
    network_mem: Option<Arc<NetBacking<'a>>>,

    /// The fuzz session this worker belongs to
    session: Option<Arc<FuzzSession<'a, C>>>,
    
    /// Optional user-controlled global context
    pub global_context: Option<Arc<C>>,

    /// Raw virtual machine that this worker uses
    pub vm: Vm,
    
    /// Random number generator seed
    pub rng: Rng,

    /// Fuzz input for the fuzz case
    pub fuzz_input: Option<Vec<u8>>,

    /// Local worker statistics, to be merged into the fuzz session on an
    /// interval
    stats: Statistics,

    /// `rdtsc` time of the next statistic sync
    sync: u64,

    /// Unique worker identifier
    worker_id: u64,

    /// List of all modules
    /// Maps from base address to module, to end of module (inclusive) and the
    /// module name
    module_list: BTreeMap<u64, (u64, Arc<String>)>,
}

impl<'a, C> Worker<'a, C> {
    /// Create a new empty VM from network backed memory
    fn from_net(memory: Arc<NetBacking<'a>>) -> Self {
        Worker {
            master:         None,
            network_mem:    Some(memory),
            vm:             Vm::new(),
            rng:            Rng::new(),
            stats:          Statistics::default(),
            sync:           0,
            session:        None,
            worker_id:      !0,
            module_list:    BTreeMap::new(),
            fuzz_input:     None,
            global_context: None,
        }
    }
    
    /// Create a new VM forked from a master
    fn fork(session: Arc<FuzzSession<'a, C>>,
            master: Arc<Self>, worker_id: u64) -> Self {
        // Create a new VM with the masters guest registers as the current
        // register state
        let mut vm = Vm::new();
        vm.guest_regs.clone_from(&master.vm.guest_regs);

        // Create the new VM referencing the master
        Worker {
            master:         Some(master),
            network_mem:    None,
            vm:             vm,
            rng:            Rng::new(),
            stats:          Statistics::default(),
            sync:           0,
            global_context: session.global_context.clone(),
            session:        Some(session),
            worker_id:      worker_id,
            module_list:    BTreeMap::new(),
            fuzz_input:     Some(Vec::new()),
        }
    }

    /// Get a random existing input
    pub fn rand_input(&self) -> Option<&[u8]> {
        // Get access to the session
        let session = self.session.as_ref().unwrap();

        // Get the number of inputs in the database
        let inputs = session.inputs.len();

        if inputs > 0 {
            // Get a random input
            session.inputs.get(self.rng.rand() % inputs).map(|x| x.as_slice())
        } else {
            // No inputs in the DB yet
            None
        }
    }

    /// Perform a single fuzz case to completion
    pub fn fuzz_case(&mut self) -> VmExit {
        let fuzz_start = cpu::rdtsc();

        // Start a timer
        let it = cpu::rdtsc();

        // Get access to the session
        let session = self.session.as_ref().unwrap().clone();

        // Get access to the master
        let master = self.master.as_ref().expect("Cannot fuzz without master");

        // Load the original snapshot registers
        self.vm.guest_regs.clone_from(&master.vm.guest_regs);

        // Reset memory to its original state
        unsafe {
            self.vm.ept.for_each_dirty_page(|addr, page| {
                let orig_page = master.get_page(addr)
                    .expect("Dirtied page without master!?");

                // Get mutable access to the underlying page
                let psl = mm::slice_phys_mut(page, 4096);

                // Copy the original page into the modified copy of the page
                llvm_asm!(r#"
                  
                    mov rcx, 4096 / 8
                    rep movsq

                "# ::
                "{rdi}"(psl.as_ptr()),
                "{rsi}"(orig_page.0) :
                "memory", "rcx", "rdi", "rsi", "cc" : 
                "intel", "volatile");
            });
        }

        // Reset the VMCS state, this also invalidates the TLB entries since
        // we have now changed the paging structures with EPT above
        self.vm.reset();

        self.stats.reset_cycles += cpu::rdtsc() - it;

        // Invoke the injection callback
        if let Some(inject) = session.inject {
            inject(self);
        }

        // Counter of number of single steps we should perform
        let mut single_step = 0;

        // Compute the timeout
        let timeout = session.timeout.map(|x| time::future(x));

        let vmexit = 'vm_loop: loop {
            if cpu::rdtsc() >= timeout.unwrap_or(!0) {
                break 'vm_loop VmExit::Timeout;
            }

            // Check if single stepping is requested
            if single_step > 0 {
                // Enable single stepping
                self.vm.set_reg(Register::Rflags,
                    self.vm.reg(Register::Rflags) | (1 << 8));

                // Decrement number of single steps requested
                single_step -= 1;
            } else {
                // Disable single stepping
                self.vm.set_reg(Register::Rflags,
                    self.vm.reg(Register::Rflags) & !(1 << 8));
            }

            // Set the pre-emption timer for randomly breaking into the VM
            // to record coverage information
            self.vm.preemption_timer = Some((self.rng.rand() & 0xfff) as u32);

            // Run the VM until a VM exit
            let (vmexit, vm_cycles) = self.vm.run();
            self.stats.vmexits += 1;
            self.stats.vm_cycles += vm_cycles;

            match vmexit {
                VmExit::EptViolation { addr, write, .. } => {
                    if self.translate(addr, write).is_some() {
                        continue;
                    }
                }
                VmExit::ExternalInterrupt => {
                    // Host interrupt happened, ignore it
                    continue;
                }
                VmExit::ReadMsr { inst_len } => {
                    // Get the MSR ID we're reading
                    let msr = self.vm.reg(Register::Rcx);

                    // Get the MSR value
                    let val = match msr {
                        0xc000_0102 => self.vm.reg(Register::KernelGsBase),
                        _ => panic!("Unexpected MSR read {:#x} @ {:#x}\n",
                                    msr, self.vm.reg(Register::Rip)),
                    };

                    // Set the low and high parts of the result
                    self.vm.set_reg(Register::Rax, (val >>  0) as u32 as u64);
                    self.vm.set_reg(Register::Rdx, (val >> 32) as u32 as u64);

                    self.vm.set_reg(Register::Rip,
                        self.vm.reg(Register::Rip).wrapping_add(inst_len));
                    continue 'vm_loop;
                }
                VmExit::WriteMsr { inst_len } => {
                    // Get the MSR ID we're writing
                    let msr = self.vm.reg(Register::Rcx);

                    // Get the value we're writing
                    let val = (self.vm.reg(Register::Rdx) << 32) |
                        self.vm.reg(Register::Rax);

                    // Get the MSR value
                    match msr {
                        0xc000_0102 => {
                            self.vm.set_reg(Register::KernelGsBase, val);
                        }
                        _ => panic!("Unexpected MSR write {:#x} @ {:#x}\n",
                                    msr, self.vm.reg(Register::Rip)),
                    }

                    // Advance PC
                    self.vm.set_reg(Register::Rip,
                        self.vm.reg(Register::Rip).wrapping_add(inst_len));
                    continue 'vm_loop;
                }
                VmExit::WriteCr { cr, gpr, inst_len } => {
                    // Get the GPR source for the write
                    let gpr = match gpr {
                         0 => self.vm.reg(Register::Rax),
                         1 => self.vm.reg(Register::Rcx),
                         2 => self.vm.reg(Register::Rdx),
                         3 => self.vm.reg(Register::Rbx),
                         4 => self.vm.reg(Register::Rsp),
                         5 => self.vm.reg(Register::Rbp),
                         6 => self.vm.reg(Register::Rsi),
                         7 => self.vm.reg(Register::Rdi),
                         8 => self.vm.reg(Register::R8),
                         9 => self.vm.reg(Register::R9),
                        10 => self.vm.reg(Register::R10),
                        11 => self.vm.reg(Register::R11),
                        12 => self.vm.reg(Register::R12),
                        13 => self.vm.reg(Register::R13),
                        14 => self.vm.reg(Register::R14),
                        15 => self.vm.reg(Register::R15),
                        _ => panic!("Invalid GPR for write CR"),
                    };

                    // Update the CR
                    match cr {
                        0 => self.vm.set_reg(Register::Cr0, gpr),
                        3 => self.vm.set_reg(Register::Cr3, gpr),
                        4 => self.vm.set_reg(Register::Cr4, gpr),
                        _ => panic!("Invalid CR register for write CR"),
                    }
                    
                    // Advance RIP to the next instruction
                    self.vm.set_reg(Register::Rip,
                        self.vm.reg(Register::Rip).wrapping_add(inst_len));
                    continue 'vm_loop;
                }
                VmExit::ReadCr { cr, gpr, inst_len } => {
                    // Get the CR that should be read
                    let cr = match cr {
                        0 => self.vm.reg(Register::Cr0),
                        3 => self.vm.reg(Register::Cr3),
                        4 => self.vm.reg(Register::Cr4),
                        _ => panic!("Invalid CR register for read CR"),
                    };

                    match gpr {
                         0 => self.vm.set_reg(Register::Rax, cr),
                         1 => self.vm.set_reg(Register::Rcx, cr),
                         2 => self.vm.set_reg(Register::Rdx, cr),
                         3 => self.vm.set_reg(Register::Rbx, cr),
                         4 => self.vm.set_reg(Register::Rsp, cr),
                         5 => self.vm.set_reg(Register::Rbp, cr),
                         6 => self.vm.set_reg(Register::Rsi, cr),
                         7 => self.vm.set_reg(Register::Rdi, cr),
                         8 => self.vm.set_reg(Register::R8,  cr),
                         9 => self.vm.set_reg(Register::R9,  cr),
                        10 => self.vm.set_reg(Register::R10, cr),
                        11 => self.vm.set_reg(Register::R11, cr),
                        12 => self.vm.set_reg(Register::R12, cr),
                        13 => self.vm.set_reg(Register::R13, cr),
                        14 => self.vm.set_reg(Register::R14, cr),
                        15 => self.vm.set_reg(Register::R15, cr),
                        _ => panic!("Invalid GPR for read CR"),
                    }

                    // Advance RIP to the next instruction
                    self.vm.set_reg(Register::Rip,
                        self.vm.reg(Register::Rip).wrapping_add(inst_len));
                    continue 'vm_loop;
                }
                VmExit::Exception(Exception::DebugException) => {
                    let modoff =
                        self.resolve_module(self.vm.reg(Register::Rip));
                    if session.report_coverage(&CoverageRecord {
                        module: modoff.0.map(|x| Cow::Borrowed(x)),
                        offset: modoff.1,
                    }) {
                        single_step = 1000;
                        if let Some(input) = &self.fuzz_input {
                            if session.input_dedup.entry_or_insert(
                                    input, 0, || Box::new(())).inserted() {
                                session.inputs.push(Box::new(input.clone()));
                            }
                        }
                    }
                    continue 'vm_loop;
                }
                VmExit::PreemptionTimer => {
                    let modoff =
                        self.resolve_module(self.vm.reg(Register::Rip));
                    if session.report_coverage(&CoverageRecord {
                        module: modoff.0.map(|x| Cow::Borrowed(x)),
                        offset: modoff.1,
                    }) {
                        single_step = 1000;
                        if let Some(input) = &self.fuzz_input {
                            if session.input_dedup.entry_or_insert(
                                    input, 0, || Box::new(())).inserted() {
                                session.inputs.push(
                                    Box::new(input.clone()));
                            }
                        }
                    }
                    continue 'vm_loop;
                }
                _ => {},
            }
            
            // Attempt to handle the vmexit with the user's callback
            if let Some(vmexit_filter) = session.vmexit_filter {
                if vmexit_filter(self, &vmexit) {
                    continue 'vm_loop;
                }
            }

            // Unhandled VM exit, break
            break 'vm_loop vmexit;
        };

        // Update number of fuzz cases
        self.stats.fuzz_cases += 1;

        // Sync the local statistics into the master on an interval
        self.stats.total_cycles += cpu::rdtsc() - fuzz_start;
        if cpu::rdtsc() >= self.sync {
            self.stats.sync_into(&mut session.stats.lock());
            if self.worker_id == 0 {
                // Report to the server
                session.report_statistics();
            }

            // Set the next sync time
            self.sync = time::future(STATISTIC_SYNC_INTERVAL);
        }

        vmexit
    }

    /// Attempt to resolve the `addr` into a module + offset based on the
    /// current `module_list`
    pub fn resolve_module(&mut self, addr: u64) -> (Option<&Arc<String>>, u64){
        if let Some((base, (end, name))) =
                self.module_list.range(..=addr).next_back() {
            if addr <= *end {
                (Some(name), addr - base)
            } else {
                (None, addr)
            }
        } else {
            (None, addr)
        }
    }

    /// Assuming the current process is a Windows 64-bit userland process,
    /// extract the module list from it
    pub fn get_module_list_win64(&mut self) -> Option<()> {
        // Create a new module list
        let mut module_list = BTreeMap::new();

        // Get the base of the TEB
        let gs_base = self.vm.reg(Register::GsBase);

        // Get the address of the `_PEB`
        let peb = self.read_virt::<u64>(VirtAddr(gs_base + 0x60))?;

        // Get the address of the `_PEB_LDR_DATA`
        let peb_ldr_data = self.read_virt::<u64>(VirtAddr(peb + 0x18))?;

        // Get the in load order module list links
        let mut mod_flink =
            self.read_virt::<u64>(VirtAddr(peb_ldr_data + 0x10))?;
        let mod_blink = self.read_virt::<u64>(VirtAddr(peb_ldr_data + 0x18))?;

        // Traverse the linked list
        while mod_flink != 0 {
            let base = self.read_virt::<u64>(VirtAddr(mod_flink + 0x30))?;
            let size = self.read_virt::<u32>(VirtAddr(mod_flink + 0x40))?;
            if size <= 0 {
                return None;
            }

            // Get the length of the module name unicode string
            let name_len = self.read_virt::<u16>(VirtAddr(mod_flink + 0x58))?;
            let name_ptr = self.read_virt::<u64>(VirtAddr(mod_flink + 0x60))?;
            if name_ptr == 0 || name_len <= 0 || (name_len % 2) != 0 {
                return None;
            }

            let mut name = vec![0u16; name_len as usize / 2];
            for (ii, wc) in name.iter_mut().enumerate() {
                *wc = self.read_virt::<u16>(VirtAddr(
                    name_ptr.checked_add((ii as u64).checked_mul(2)?)?))?;
            }

            // Convert the module name into a UTF-8 Rust string
            let name_utf8 = Arc::new(String::from_utf16(&name).ok()?);

            // Save the module information into the module list
            module_list.insert(base,
                (base.checked_add(size as u64 - 1)?, name_utf8));

            // Go to the next link in the table
            if mod_flink == mod_blink { break; }
            mod_flink = self.read_virt::<u64>(VirtAddr(mod_flink))?;
        }

        // Establish the new module list
        self.module_list = module_list;

        Some(())
    }

    /// Get the base address for a given segment
    pub fn seg_base(&self, segment: Segment) -> u64 {
        match segment {
            Segment::Es => self.vm.reg(Register::EsBase),
            Segment::Ds => self.vm.reg(Register::DsBase),
            Segment::Fs => self.vm.reg(Register::FsBase),
            Segment::Gs => self.vm.reg(Register::GsBase),
            Segment::Ss => self.vm.reg(Register::SsBase),
            Segment::Cs => self.vm.reg(Register::CsBase),
        }
    }

    /// Reads memory using the `addr` provided
    pub fn read_addr(&mut self, addr: Address, mut buf: &mut [u8])
            -> Option<()> {
        // Nothing to do in the 0 byte case
        if buf.len() == 0 { return Some(()); }

        // Offset into the read we've completed
        let mut loff = 0u64;

        while buf.len() > 0 {
            // Get the guest physical address for this page
            let gpaddr = match addr {
                Address::PhysicalLinear { addr } => addr.wrapping_add(loff),
                Address::PhysicalSegOff { seg, off } => {
                    self.seg_base(seg).wrapping_add(off).wrapping_add(loff)
                }
                Address::Virtual { seg, off, mode, cr3 } => {
                    let linear = self.seg_base(seg).wrapping_add(off)
                        .wrapping_add(loff);
                    let (page, off, _) = match mode {
                        PagingMode::Bits32 => {
                            translate_32_no_pae(cr3, VirtAddr(linear),
                                |paddr| self.read_phys(paddr))?
                        }
                        PagingMode::Bits32Pae => {
                            translate_32_pae(cr3, VirtAddr(linear),
                                |paddr| self.read_phys(paddr))?
                        }
                        PagingMode::Bits64 => {
                            translate_64_4_level(cr3, VirtAddr(linear),
                                |paddr| self.read_phys(paddr))?
                        }
                    };
                    page.0.wrapping_add(off)
                }
                Address::Linear { addr, mode, cr3 } => {
                    let addr = addr.wrapping_add(loff);
                    let (page, off, _) = match mode {
                        PagingMode::Bits32 => {
                            translate_32_no_pae(cr3, VirtAddr(addr),
                                |paddr| self.read_phys(paddr))?
                        }
                        PagingMode::Bits32Pae => {
                            translate_32_pae(cr3, VirtAddr(addr),
                                |paddr| self.read_phys(paddr))?
                        }
                        PagingMode::Bits64 => {
                            translate_64_4_level(cr3, VirtAddr(addr),
                                |paddr| self.read_phys(paddr))?
                        }
                    };
                    page.0.wrapping_add(off)
                }
            };

            // Get the host physical address for this page
            let paddr = self.translate(PhysAddr(gpaddr), false)?;

            // Compute the remaining number of bytes on the page
            let page_remain = 0x1000 - (paddr.0 & 0xfff);

            // Compute the number of bytes to copy
            let to_copy = core::cmp::min(page_remain as usize, buf.len());

            // Read the memory from the backing page into the user's buffer
            let psl = unsafe { mm::slice_phys(paddr, to_copy as u64) };
            buf[..to_copy].copy_from_slice(psl);

            // Advance the buffers
            loff += to_copy as u64;
            buf   = &mut buf[to_copy..];
        }

        Some(())
    }
    
    /// Writes memory using to the `addr` provided
    pub fn write_addr(&mut self, addr: Address, mut buf: &[u8])
            -> Option<()> {
        // Nothing to do in the 0 byte case
        if buf.len() == 0 { return Some(()); }

        // Offset into the read we've completed
        let mut loff = 0u64;

        while buf.len() > 0 {
            // Get the guest physical address for this page
            let gpaddr = match addr {
                Address::PhysicalLinear { addr } => addr.wrapping_add(loff),
                Address::PhysicalSegOff { seg, off } => {
                    self.seg_base(seg).wrapping_add(off).wrapping_add(loff)
                }
                Address::Virtual { seg, off, mode, cr3 } => {
                    let linear = self.seg_base(seg).wrapping_add(off)
                        .wrapping_add(loff);
                    let (page, off, _) = match mode {
                        PagingMode::Bits32 => {
                            translate_32_no_pae(cr3, VirtAddr(linear),
                                |paddr| self.read_phys(paddr))?
                        }
                        PagingMode::Bits32Pae => {
                            translate_32_pae(cr3, VirtAddr(linear),
                                |paddr| self.read_phys(paddr))?
                        }
                        PagingMode::Bits64 => {
                            translate_64_4_level(cr3, VirtAddr(linear),
                                |paddr| self.read_phys(paddr))?
                        }
                    };
                    page.0.wrapping_add(off)
                }
                Address::Linear { addr, mode, cr3 } => {
                    let addr = addr.wrapping_add(loff);
                    let (page, off, _) = match mode {
                        PagingMode::Bits32 => {
                            translate_32_no_pae(cr3, VirtAddr(addr),
                                |paddr| self.read_phys(paddr))?
                        }
                        PagingMode::Bits32Pae => {
                            translate_32_pae(cr3, VirtAddr(addr),
                                |paddr| self.read_phys(paddr))?
                        }
                        PagingMode::Bits64 => {
                            translate_64_4_level(cr3, VirtAddr(addr),
                                |paddr| self.read_phys(paddr))?
                        }
                    };
                    page.0.wrapping_add(off)
                }
            };

            // Get the host physical address for this page
            let paddr = self.translate(PhysAddr(gpaddr), true)?;

            // Compute the remaining number of bytes on the page
            let page_remain = 0x1000 - (paddr.0 & 0xfff);

            // Compute the number of bytes to copy
            let to_copy = core::cmp::min(page_remain as usize, buf.len());

            // Read the memory from the backing page into the user's buffer
            let psl = unsafe { mm::slice_phys_mut(paddr, to_copy as u64) };
            psl.copy_from_slice(&buf[..to_copy]);

            // Advance the buffers
            loff += to_copy as u64;
            buf   = &buf[to_copy..];
        }

        Some(())
    }

    /// Gets the current paging mode of the system
    pub fn paging_mode(&self) -> Option<PagingMode> {
        let cr0  = self.vm.reg(Register::Cr0);
        let cr4  = self.vm.reg(Register::Cr4);
        let efer = self.vm.reg(Register::Efer);

        if cr0 & (1 << 31) == 0 {
            // Paging disabled
            None
        } else {
            // Paging enabled
            if efer & (1 << 8) == 0 {
                // EFER.LME not set (32-bit mode)
                if cr4 & (1 << 5) == 0 {
                    // CR4.PAE clear
                    Some(PagingMode::Bits32)
                } else {
                    // CR4.PAE set
                    Some(PagingMode::Bits32Pae)
                }
            } else {
                // EFER.LME set (64-bit mode)
                if cr4 & (1 << 5) == 0 {
                    // CR4.PAE clear, invalid state
                    None
                } else {
                    // CR4.PAE set
                    Some(PagingMode::Bits64)
                }
            }
        }
    }
    
    /// Reads the contents at `vaddr` into a `T` which implements `Primitive`
    /// using the current active page table
    pub fn read_virt<T: Primitive>(&mut self, vaddr: VirtAddr) -> Option<T> {
        self.read_virt_cr3(vaddr, self.vm.reg(Register::Cr3))
    }
    
    /// Reads the contents at `vaddr` into a `T` which implements `Primitive`
    /// using the page table in `cr3`
    pub fn read_virt_cr3<T: Primitive>(&mut self, vaddr: VirtAddr, cr3: u64)
            -> Option<T> {
        let mut ret = T::default();
        self.read_virt_cr3_into(vaddr, ret.cast_mut(), cr3)?;
        Some(ret)
    }
    
    /// Read the contents of the guest virtual memory at `vaddr` into the
    /// `buf` provided using the current page table
    ///
    /// Returns `None` if the request cannot be fully satisfied. It is possible
    /// that some reading did occur, but is partial.
    pub fn read_virt_into(&mut self, vaddr: VirtAddr,
                          buf: &mut [u8]) -> Option<()> {
        self.read_virt_cr3_into(vaddr, buf, self.vm.reg(Register::Cr3))
    }
    
    /// Read the contents of the guest virtual memory at `vaddr` into the
    /// `buf` provided using page table `cr3`
    ///
    /// Returns `None` if the request cannot be fully satisfied. It is possible
    /// that some reading did occur, but is partial.
    pub fn read_virt_cr3_into(&mut self, vaddr: VirtAddr,
                              buf: &mut [u8], cr3: u64) -> Option<()> {
        self.read_addr(Address::Linear {
            addr: vaddr.0,
            mode: self.paging_mode()?,
            cr3:  cr3,
        }, buf)
    }
    
    /// Write the contents of `buf` to the guest virtual memory at `vaddr`
    /// using page table `cr3`
    ///
    /// Returns `None` if the request cannot be fully satisfied. It is possible
    /// that some reading did occur, but is partial.
    pub fn write_virt_cr3_from(&mut self, vaddr: VirtAddr,
                               buf: &[u8], cr3: u64) -> Option<()> {
        self.write_addr(Address::Linear {
            addr: vaddr.0,
            mode: self.paging_mode()?,
            cr3:  cr3,
        }, buf)
    }

    /// Reads the contents at `gpaddr` into a `T` which implements `Primitive`
    pub fn read_phys<T: Primitive>(&mut self, gpaddr: PhysAddr) -> Option<T> {
        let mut ret = T::default();
        self.read_phys_into(gpaddr, ret.cast_mut())?;
        Some(ret)
    }

    /// Read the contents of the guest physical memory at `gpaddr` into the
    /// `buf` provided
    ///
    /// Returns `None` if the request cannot be fully satisfied. It is possible
    /// that some reading did occur, but is partial.
    pub fn read_phys_into(&mut self, mut gpaddr: PhysAddr, mut buf: &mut [u8])
            -> Option<()> {
        // Nothing to do in the 0 byte case
        if buf.len() == 0 { return Some(()); }
        
        // Starting physical address (invalid paddr, but page aligned)
        let mut paddr = PhysAddr(!0xfff);

        while buf.len() > 0 {
            if (paddr.0 & 0xfff) == 0 {
                // Crossed into a new page, translate
                paddr = self.translate(gpaddr, false)?;
            }

            // Compute the remaining number of bytes on the page
            let page_remain = 0x1000 - (paddr.0 & 0xfff);

            // Compute the number of bytes to copy
            let to_copy = core::cmp::min(page_remain as usize, buf.len());

            // Read the memory from the backing page into the user's buffer
            let psl = unsafe { mm::slice_phys(paddr, to_copy as u64) };
            buf[..to_copy].copy_from_slice(psl);

            // Advance the buffer pointers
            paddr  = PhysAddr(paddr.0 + to_copy as u64);
            gpaddr = PhysAddr(gpaddr.0 + to_copy as u64);
            buf    = &mut buf[to_copy..];
        }

        Some(())
    }
    
    /// Writes the contents of `T` to the `gpaddr`
    pub fn write_phys<T: Primitive>(&mut self, gpaddr: PhysAddr, val: T)
            -> Option<()> {
        self.write_phys_from(gpaddr, val.cast())
    }

    /// Write the contents of `buf` into the guest physical memory at `gpaddr`
    /// at the guest
    ///
    /// Returns `None` if the request cannot be fully satisfied. It is possible
    /// that some writing did occur, but is partial.
    pub fn write_phys_from(&mut self, mut gpaddr: PhysAddr, mut buf: &[u8])
            -> Option<()>{
        // Nothing to do in the 0 byte case
        if buf.len() == 0 { return Some(()); }
        
        // Starting physical address (invalid paddr, but page aligned)
        let mut paddr = PhysAddr(!0xfff);

        while buf.len() > 0 {
            if (paddr.0 & 0xfff) == 0 {
                // Crossed into a new page, translate
                paddr = self.translate(gpaddr, true)?;
            }

            // Compute the remaining number of bytes on the page
            let page_remain = 0x1000 - (paddr.0 & 0xfff);

            // Compute the number of bytes to copy
            let to_copy = core::cmp::min(page_remain as usize, buf.len());

            // Get mutable access to the underlying page and copy the memory
            // from the buffer into it
            let psl = unsafe { mm::slice_phys_mut(paddr, to_copy as u64) };
            psl.copy_from_slice(&buf[..to_copy]);

            // Advance the buffer pointers
            paddr  = PhysAddr(paddr.0 + to_copy as u64);
            gpaddr = PhysAddr(gpaddr.0 + to_copy as u64);
            buf    = &buf[to_copy..];
        }

        Some(())
    }

    /// Attempts to get a slice to the page backing `gpaddr` in host
    /// addressable memory
    fn get_page(&self, gpaddr: PhysAddr) -> Option<VirtAddr> {
        // Validate alignment
        assert!(gpaddr.0 & 0xfff == 0,
                "get_page() requires an aligned guest physical address");

        // Attempt to translate the page, it is possible it has not yet been
        // mapped and we need to page it in from the network mapped storage in
        // the `FuzzTarget`
        let translation = self.vm.ept.translate(gpaddr);
        if let Some(Mapping { page: Some(orig_page), .. }) = translation {
            Some(VirtAddr(unsafe {
                mm::slice_phys_mut(orig_page.0, 4096).as_ptr() as u64
            }))
        } else {
            if let Some(master) = &self.master {
                master.get_page(gpaddr)
            } else if let Some(netmem) = &self.network_mem {
                // Find the region which may contain our address
                let (phys_base, (offset, end)) = netmem.phys_ranges
                    .range(..=gpaddr.0).next_back()?;

                // Make sure our address falls in the region
                if gpaddr.0 < *phys_base || gpaddr.0 > *end {
                    return None;
                }

                // Compute the offset into the memory based on our offset into
                // the region
                let offset = offset
                    .checked_add((gpaddr.0 - phys_base) as usize)?;
                assert!(offset & 0xfff == 0, "Whoa, page offset not aligned");

                // Get a slice to the memory backing this requested region
                let data = netmem.memory.get(offset..offset + 4096)?;
                Some(VirtAddr(data.as_ptr() as u64))
            } else {
                // Nobody can provide the memory for us, it's not present
                None
            }
        }
    }

    /// Translate a physical address for the guest into a physical address on
    /// the host. If `write` is set, the translation will occur for a write
    /// access, and thus the copy-on-write will be performed on the page if
    /// needed to satisfy the write.
    ///
    /// If the physical address is not valid for the guest, this will return
    /// `None`.
    ///
    /// The translation will only be valid for the page the `gpaddr` resides in
    /// The returned physical address will have the offset from the physical
    /// address applied. Such that a request for physical address `0x13371337`
    /// would return a physical address ending in `0x337`
    fn translate(&mut self, gpaddr: PhysAddr, write: bool) -> Option<PhysAddr> {
        // Get access to physical memory
        let mut pmem = mm::PhysicalMemory;
        
        // Align the guest physical address
        let align_gpaddr = PhysAddr(gpaddr.0 & !0xfff);

        // Attempt to translate the page, it is possible it has not yet been
        // mapped and we need to page it in from the network mapped storage in
        // the `FuzzTarget`
        let translation = self.vm.ept.translate_dirty(align_gpaddr, write);
        
        // First, determine if we need to perform a CoW or make a mapping for
        // an unmapped page
        if let Some(Mapping {
                pte: Some(pte), page: Some(orig_page), .. }) = translation {
            // Page is mapped, it is possible it needs to be promoted to
            // writable
            let page_writable =
                (unsafe { mm::read_phys::<u64>(pte) } & EPT_WRITE) != 0;

            // If the page is writable, and this is is a write, OR if the
            // operation is not a write, then the existing allocation can
            // satisfy the translation request.
            if (write && page_writable) || !write {
                return Some(PhysAddr((orig_page.0).0 + (gpaddr.0 & 0xfff)));
            }
        }

        // At this stage, we either must perform a CoW or map an unmapped page

        // Get the original contents of the page
        let orig_page_gpaddr = if let Some(master) = &self.master {
            // Get the page from the master
            master.get_page(align_gpaddr)?
        } else if let Some(_) = &self.network_mem {
            self.get_page(align_gpaddr)?
        } else {
            // Page is not present, and cannot be filled from the master or
            // network memory
            return None;
        };

        // Look up the physical page backing for the mapping

        // Touch the page to make sure it's present
        unsafe { core::ptr::read_volatile(orig_page_gpaddr.0 as *const u8); }
        
        let orig_page = {
            // Get access to the host page table
            let mut page_table = core!().boot_args.page_table.lock();
            let page_table = page_table.as_mut().unwrap();

            // Translate the mapping virtual address into a physical
            // address
            //
            // This will always succeed as we touched the memory above
            let (page, offset) =
                page_table.translate(&mut pmem, orig_page_gpaddr)
                    .map(|x| x.page).flatten()
                    .expect("Whoa, memory page not mapped?!");
            PhysAddr(page.0 + offset)
        };

        // Get a slice to the original read-only page
        let ro_page = unsafe { mm::slice_phys_mut(orig_page, 4096) };

        let page = if let Some(Mapping { pte: Some(pte), page: Some(_), .. }) =
                translation {
            // Promote the original page via CoW
                
            // Allocate a new page
            let page = pmem.alloc_phys(
                Layout::from_size_align(4096, 4096).unwrap()).unwrap();

            // Get mutable access to the underlying page
            let psl = unsafe { mm::slice_phys_mut(page, 4096) };

            // Copy in the bytes to initialize the page from the network
            // mapped memory
            psl.copy_from_slice(&ro_page);

            // Promote the page via CoW
            unsafe {
                mm::write_phys(pte, 
                    page.0 | EPT_WRITE | EPT_READ | EPT_EXEC | EPT_USER_EXEC);
            }

            page
        } else {
            // Page was not mapped
            if write {
                // Page needs to be CoW-ed from the network mapped file

                // Allocate a new page
                let page = pmem.alloc_phys(
                    Layout::from_size_align(4096, 4096).unwrap()).unwrap();

                // Get mutable access to the underlying page
                let psl = unsafe { mm::slice_phys_mut(page, 4096) };

                // Copy in the bytes to initialize the page from the network
                // mapped memory
                psl.copy_from_slice(&ro_page);

                unsafe {
                    // Map in the page as RW
                    self.vm.ept.map_raw(align_gpaddr,
                        PageType::Page4K,
                        page.0 | EPT_READ | EPT_WRITE | EPT_EXEC |
                        EPT_USER_EXEC | EPT_MEMTYPE_WB)
                        .unwrap();
                }

                // Return the physical address of the new page
                page
            } else {
                // Page is only being accessed for read. Alias the guest's
                // physical memory directly into the network mapped page as
                // read-only
                
                unsafe {
                    // Map in the page as read-only into the guest page table
                    self.vm.ept.map_raw(align_gpaddr, PageType::Page4K,
                        orig_page.0 | EPT_READ | EPT_EXEC | EPT_USER_EXEC |
                        EPT_MEMTYPE_WB)
                        .unwrap();
                }

                // Return the physical address of the backing page
                orig_page
            }
        };
        
        // Return the host physical address of the requested guest physical
        // address
        Some(PhysAddr(page.0 + (gpaddr.0 & 0xfff)))
    }
}

type InjectCallback<'a, C> = fn(&mut Worker<'a, C>);

type VmExitFilter<'a, C> = fn(&mut Worker<'a, C>, &VmExit) -> bool;

/// A session for multiple workers to fuzz a shared job
pub struct FuzzSession<'a, C> {
    /// Master VM state
    master_vm: Arc<Worker<'a, C>>,

    /// Optional user-controlled global context
    global_context: Option<Arc<C>>,

    /// Timeout for each fuzz case
    timeout: Option<u64>,

    /// Callback to invoke before every fuzz case, for the fuzzer to inject
    /// information into the VM
    inject: Option<InjectCallback<'a, C>>,

    /// Callback to invoke when VM exits are hit to allow a user to handle VM
    /// exits to re-enter the VM
    vmexit_filter: Option<VmExitFilter<'a, C>>,
    
    /// All observed coverage information
    coverage: Aht<CoverageRecord<'a>, (), 65536>,

    /// Coverage which has yet to be reported to the server
    pending_coverage: LockCell<Vec<CoverageRecord<'a>>, LockInterrupts>,

    /// Hash table of inputs
    input_dedup: Aht<Vec<u8>, (), 65536>,

    /// Inputs which caused coverage
    inputs: AtomicVec<Vec<u8>, 4096>,

    /// Global statistics for the fuzz cases
    stats: LockCell<Statistics, LockInterrupts>,

    /// Open connection to the server
    server: UdpBind,

    /// Address to use when communicating with the server
    server_addr: UdpAddress,

    /// Number of workers
    workers: AtomicU64,

    /// "Unique" session identifier
    id: u64,
}

impl<'a, C> FuzzSession<'a, C> {
    /// Create a new empty fuzz session
    pub fn from_falkdump<S>(server: &str, name: S) -> Self
            where S: AsRef<str> {
        macro_rules! consume {
            ($ptr:expr, $ty:ty) => {{
                let ret = <$ty>::from_le_bytes(
                    $ptr[..size_of::<$ty>()].try_into().unwrap());
                $ptr = &$ptr[size_of::<$ty>()..];
                ret
            }}
        }

        // Convert the generic name into a reference to a string
        let name: &str = name.as_ref();

        // Network map the memory file contents as read-only
        let memory = NetMapping::new(server, name.as_ref(), true)
            .expect("Failed to netmap falkdump");

        // Check the signature
        assert!(&memory[..8] == b"FALKDUMP", "Invalid signature for falkdump");

        // Get a pointer to the file contents
        let mut ptr = &memory[8..];

        // Get the size of the region region in bytes
        let regs_size = consume!(ptr, u64);
        ptr = &ptr[regs_size as usize..];

        // Get the number of regions
        let regions = consume!(ptr, u64);

        // Parse out the physical region information
        let mut phys_ranges = BTreeMap::new();
        for _ in 0..regions {
            let start  = consume!(ptr, u64);
            let end    = consume!(ptr, u64);
            let offset = consume!(ptr, u64);

            assert!(end > start && end & 0xfff == 0xfff && start & 0xfff == 0);

            // Log the region
            phys_ranges.insert(start, (offset as usize, end));
        }
        
        // Create a new master VM from the information provided
        let netbacking = Arc::new(NetBacking { memory, phys_ranges });
        let mut master = Worker::from_net(netbacking.clone());
        
        // Parse the registers from the register state
        let mut ptr = &netbacking.memory[16..16 + regs_size as usize];
        let _version = consume!(ptr, u32);
        let _size    = consume!(ptr, u32);
        master.vm.set_reg(Register::Rax, consume!(ptr, u64));
        master.vm.set_reg(Register::Rbx, consume!(ptr, u64));
        master.vm.set_reg(Register::Rcx, consume!(ptr, u64));
        master.vm.set_reg(Register::Rdx, consume!(ptr, u64));
        master.vm.set_reg(Register::Rsi, consume!(ptr, u64));
        master.vm.set_reg(Register::Rdi, consume!(ptr, u64));
        master.vm.set_reg(Register::Rsp, consume!(ptr, u64));
        master.vm.set_reg(Register::Rbp, consume!(ptr, u64));
        master.vm.set_reg(Register::R8 , consume!(ptr, u64));
        master.vm.set_reg(Register::R9 , consume!(ptr, u64));
        master.vm.set_reg(Register::R10, consume!(ptr, u64));
        master.vm.set_reg(Register::R11, consume!(ptr, u64));
        master.vm.set_reg(Register::R12, consume!(ptr, u64));
        master.vm.set_reg(Register::R13, consume!(ptr, u64));
        master.vm.set_reg(Register::R14, consume!(ptr, u64));
        master.vm.set_reg(Register::R15, consume!(ptr, u64));
        master.vm.set_reg(Register::Rip, consume!(ptr, u64));
        master.vm.set_reg(Register::Rflags, consume!(ptr, u64));

        master.vm.set_reg(Register::Cs,      consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::CsLimit, consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::CsAccessRights,
                          (consume!(ptr, u32) as u64) >> 8);
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::CsBase, consume!(ptr, u64));
        
        master.vm.set_reg(Register::Ds,      consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::DsLimit, consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::DsAccessRights,
                          (consume!(ptr, u32) as u64) >> 8);
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::DsBase, consume!(ptr, u64));
        
        master.vm.set_reg(Register::Es,      consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::EsLimit, consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::EsAccessRights,
                          (consume!(ptr, u32) as u64) >> 8);
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::EsBase, consume!(ptr, u64));
        
        master.vm.set_reg(Register::Fs,      consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::FsLimit, consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::FsAccessRights,
                          (consume!(ptr, u32) as u64) >> 8);
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::FsBase, consume!(ptr, u64));
        
        master.vm.set_reg(Register::Gs,      consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::GsLimit, consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::GsAccessRights,
                          (consume!(ptr, u32) as u64) >> 8);
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::GsBase, consume!(ptr, u64));
        
        master.vm.set_reg(Register::Ss,      consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::SsLimit, consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::SsAccessRights,
                          (consume!(ptr, u32) as u64) >> 8);
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::SsBase, consume!(ptr, u64));
        
        master.vm.set_reg(Register::Ldtr,      consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::LdtrLimit, consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::LdtrAccessRights,
                          (consume!(ptr, u32) as u64) >> 8);
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::LdtrBase, consume!(ptr, u64));
        
        master.vm.set_reg(Register::Tr,      consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::TrLimit, consume!(ptr, u32) as u64);
        master.vm.set_reg(Register::TrAccessRights,
                          (consume!(ptr, u32) as u64) >> 8);
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::TrBase, consume!(ptr, u64));
        
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::GdtrLimit, consume!(ptr, u32) as u64);
        let _ = consume!(ptr, u32);
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::GdtrBase, consume!(ptr, u64));
        
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::IdtrLimit, consume!(ptr, u32) as u64);
        let _ = consume!(ptr, u32);
        let _ = consume!(ptr, u32);
        master.vm.set_reg(Register::IdtrBase, consume!(ptr, u64));
        
        master.vm.set_reg(Register::Cr0, consume!(ptr, u64));
        let _ = consume!(ptr, u64);
        master.vm.set_reg(Register::Cr2, consume!(ptr, u64));
        master.vm.set_reg(Register::Cr3, consume!(ptr, u64));
        master.vm.set_reg(Register::Cr4, consume!(ptr, u64) | (1 << 13));
        
        master.vm.set_reg(Register::KernelGsBase, consume!(ptr, u64));
        
        master.vm.set_reg(Register::Cr8, consume!(ptr, u64));
        
        master.vm.set_reg(Register::CStar, consume!(ptr, u64));
        master.vm.set_reg(Register::LStar, consume!(ptr, u64));
        master.vm.set_reg(Register::FMask, consume!(ptr, u64));
        master.vm.set_reg(Register::Star,  consume!(ptr, u64));

        master.vm.set_reg(Register::SysenterCs,  consume!(ptr, u64));
        master.vm.set_reg(Register::SysenterEsp, consume!(ptr, u64));
        master.vm.set_reg(Register::SysenterEip, consume!(ptr, u64));
        
        master.vm.set_reg(Register::Efer, consume!(ptr, u64));

        let _ = consume!(ptr, u64);
        let _ = consume!(ptr, u64);
        let _ = consume!(ptr, u64);
        let _ = consume!(ptr, u64);
        let _ = consume!(ptr, u64);
        let _ = consume!(ptr, u64);
        let _ = consume!(ptr, u64);
        master.vm.set_reg(Register::Dr7, consume!(ptr, u64));

        unsafe {
            // Remainder should be fxsave area
            assert!(ptr.len() == 512);

            master.vm.set_fxsave(
                core::ptr::read_unaligned(
                    ptr[..512].as_ptr() as *const FxSave));
        }
        
        let efer = master.vm.reg(Register::Efer);
        if efer & (1 << 8) != 0 {
            // Long mode, QEMU gives some non-zero limits, zero them out
            master.vm.set_reg(Register::EsLimit, 0);
            master.vm.set_reg(Register::CsLimit, 0);
            master.vm.set_reg(Register::SsLimit, 0);
            master.vm.set_reg(Register::DsLimit, 0);
            master.vm.set_reg(Register::FsLimit, 0);
            master.vm.set_reg(Register::GsLimit, 0);
        }

        /// Perform some filtering of the access rights as QEMU and VT-x have
        /// slightly different expectations for these bits
        macro_rules! filter_ars {
            ($ar:expr, $lim:expr) => {
                // Mark any non-present segment as inactive
                if master.vm.reg($ar) & (1 << 7) == 0 {
                    master.vm.set_reg($ar, 0x10000);
                }

                // If any bit in the bottom 12 bits of the limit is zero, then
                // G must be zero
                if master.vm.reg($lim) & 0xfff != 0xfff {
                    master.vm.set_reg($ar, master.vm.reg($ar) & !(1 << 15));
                }
            }
        }

        filter_ars!(Register::EsAccessRights, Register::EsLimit);
        filter_ars!(Register::CsAccessRights, Register::CsLimit);
        filter_ars!(Register::SsAccessRights, Register::SsLimit);
        filter_ars!(Register::DsAccessRights, Register::DsLimit);
        filter_ars!(Register::FsAccessRights, Register::FsLimit);
        filter_ars!(Register::GsAccessRights, Register::GsLimit);
        filter_ars!(Register::LdtrAccessRights, Register::LdtrLimit);
        filter_ars!(Register::TrAccessRights, Register::TrLimit);

        // Get access to a network device
        let netdev = NetDevice::get().expect("Failed to get network device");

        // Bind to a random UDP port on this network device
        let udp = NetDevice::bind_udp(netdev.clone())
            .expect("Failed to bind to UDP for network");

        // Resolve the target
        let server_address = UdpAddress::resolve(
            &netdev, udp.port(), server)
            .expect("Couldn't resolve target address");

        FuzzSession {
            master_vm:        Arc::new(master),
            coverage:         Aht::new(),
            pending_coverage: LockCell::new(Vec::new()),
            stats:            LockCell::new(Statistics::default()),
            timeout:          None,
            inject:           None,
            vmexit_filter:    None,
            input_dedup:      Aht::new(),
            inputs:           AtomicVec::new(),
            server:           udp,
            server_addr:      server_address,
            workers:          AtomicU64::new(0),
            id:               cpu::rdtsc(),
            global_context:   None,
        }
    }

    /// Invoke a closure with access to the initial memory and register states
    /// of the snapshot such that they can be mutated to create the basis for
    /// all fuzz cases.
    pub fn init_master_vm<F>(mut self, callback: F) -> Self
            where F: FnOnce(&mut Worker<C>) {
        callback(Arc::get_mut(&mut self.master_vm).unwrap());
        self
    }

    /// Set the timeout for the VMs in microseconds
    pub fn timeout(mut self, timeout: u64) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Set the injection callback routine. This will be invoked every time
    /// the VM is reset and a new fuzz case is about to begin.
    pub fn inject(mut self, inject: InjectCallback<'a, C>) -> Self {
        self.inject = Some(inject);
        self
    }
    
    /// Set the global context for the session
    pub fn global_context(mut self, context: C) -> Self {
        self.global_context = Some(Arc::new(context));
        self
    }
    
    /// Set the VM exit filter for the workers. This will be invoked on an
    /// unhandled VM exit and gives an opportunity for the fuzzer to handle
    /// a VM exit to allow re-entry into the VM
    pub fn vmexit_filter(mut self, vmexit_filter: VmExitFilter<'a, C>)
            -> Self {
        self.vmexit_filter = Some(vmexit_filter);
        self
    }

    /// Get a new worker for this fuzz session
    pub fn worker(session: Arc<Self>) -> Worker<'a, C> {
        // Log into the server with a new worker
        session.login();

        // Get a new worker ID
        let worker_id = session.workers.fetch_add(1, Ordering::SeqCst);

        // Fork the worker from the master
        Worker::fork(session.clone(), session.master_vm.clone(), worker_id)
    }

    /// Update statistics to the server
    pub fn report_statistics(&self) {
        // Attempt to log into the server
        let mut packet = self.server.device().allocate_packet();
        {
            let stats = self.stats.lock();
            let mut pkt = packet.create_udp(&self.server_addr);
            ServerMessage::ReportStatistics {
                fuzz_cases:   stats.fuzz_cases,
                total_cycles: stats.total_cycles,
                vm_cycles:    stats.vm_cycles,
                reset_cycles: stats.reset_cycles,
            }.serialize(&mut pkt).unwrap();
        }
        self.server.device().send(packet, true);
    }

    /// Log in with the server
    pub fn login(&self) {
        loop {
            // Attempt to log into the server
            let mut packet = self.server.device().allocate_packet();
            {
                let mut pkt = packet.create_udp(&self.server_addr);
                ServerMessage::Login(self.id, core!().id)
                    .serialize(&mut pkt).unwrap();
            }
            self.server.device().send(packet, true);

            // Wait for the acknowledge from the server
            if self.server.recv_timeout(50_000, |_, udp| {
                // Deserialize the message
                let mut ptr = &udp.payload[..];
                let msg = ServerMessage::deserialize(&mut ptr)
                    .expect("Failed to deserialize File ID response");
                
                // Check if we got an ack
                match msg {
                    ServerMessage::LoginAck(sid, core) => {
                        if sid == self.id && core == core!().id {
                            Some(())
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }).is_some() { break; }
        }
    }

    /// Report coverage
    pub fn report_coverage(&self, cr: &CoverageRecord) -> bool {
        if self.coverage.entry_or_insert(cr, cr.offset as usize,
                                         || Box::new(())).inserted() {
            // Coverage was new, queue it to be reported to the server
            self.pending_coverage.lock().push(CoverageRecord {
                module: cr.module.as_ref().map(|x| Cow::Owned((**x).clone())),
                offset: cr.offset,
            });

            /*
            // Coverage was new, report it to the server
            loop {
                // Report the coverage
                let mut packet = self.server.device().allocate_packet();
                {
                    let mut pkt = packet.create_udp(&self.server_addr);
                    ServerMessage::ReportCoverage(Cow::Borrowed(cr))
                        .serialize(&mut pkt).unwrap();
                }
                self.server.device().send(packet, true);

                // Wait for the acknowledge from the server
                if self.server.recv_timeout(100, |_, udp| {
                    // Deserialize the message
                    let mut ptr = &udp.payload[..];
                    let msg = ServerMessage::deserialize(&mut ptr)
                        .expect("Failed to deserialize File ID response");
                    
                    // Check if we got an ack
                    match msg {
                        ServerMessage::ReportCoverageAck(x) => {
                            // Check if the ack is acknowledging the coverage
                            // we reported
                            if &*x == cr {
                                // Ack matches, break out of the recv
                                Some(())
                            } else {
                                // Nope
                                None
                            }
                        }
                        _ => None,
                    }
                }).is_some() { break; }
            }*/

            true
        } else {
            false
        }
    }
}

