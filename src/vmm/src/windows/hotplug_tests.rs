//! Tests of hot-plugged RAM against a real Windows Hypervisor Platform partition.
//!
//! The virtio-mem device commits and maps a range when the guest plugs it, and unmaps and
//! decommits it when the guest unplugs it. These tests make the same calls on a real partition,
//! with no guest. They are skipped on a host that has no hypervisor.

use std::sync::Arc;

use vm_memory::{GuestRegionMmap, MmapRegion};
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_DECOMMIT, PAGE_READWRITE,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::*;

const BLOCK: u64 = 2 << 20;
const BLOCKS: u64 = 32;
const BASE: u64 = 4 << 30;
const LOW: usize = 1 << 20;

struct Machine {
    vm: Vm,
    mapper: Box<dyn devices::virtio::RamMapper>,
    host: u64,
    _memory: GuestMemoryMmap,
}

/// A partition with ordinary low RAM and a hotplug region that is only reserved.
fn machine() -> Option<Machine> {
    let mut vm = match Vm::new(1, false) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("skipped: no Windows Hypervisor Platform partition: {error}");
            return None;
        }
    };
    let hotplug = MmapRegion::new_reserved((BLOCKS * BLOCK) as usize).unwrap();
    let host = hotplug.as_ptr() as u64;
    let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), LOW)])
        .unwrap()
        .insert_region(Arc::new(
            GuestRegionMmap::new(hotplug, GuestAddress(BASE)).unwrap(),
        ))
        .unwrap();
    vm.memory_init(&memory).unwrap();
    let mapper = vm.ram_mapper().expect("a reserved region gives a mapper");
    Some(Machine {
        vm,
        mapper,
        host,
        _memory: memory,
    })
}

impl Machine {
    fn commit(&self, block: u64, blocks: u64) -> bool {
        let at = (self.host + block * BLOCK) as *const core::ffi::c_void;
        // SAFETY: the range is inside the reservation of the hotplug region.
        !unsafe { VirtualAlloc(at, (blocks * BLOCK) as usize, MEM_COMMIT, PAGE_READWRITE) }
            .is_null()
    }

    fn decommit(&self, block: u64, blocks: u64) -> bool {
        let at = (self.host + block * BLOCK) as *mut core::ffi::c_void;
        // SAFETY: the range is inside the reservation of the hotplug region.
        unsafe { VirtualFree(at, (blocks * BLOCK) as usize, MEM_DECOMMIT) != 0 }
    }

    fn map(&self, block: u64, blocks: u64) -> bool {
        self.mapper
            .map(
                self.host + block * BLOCK,
                BASE + block * BLOCK,
                blocks * BLOCK,
            )
            .is_ok()
    }

    fn unmap(&self, block: u64, blocks: u64) -> bool {
        self.mapper
            .unmap(BASE + block * BLOCK, blocks * BLOCK)
            .is_ok()
    }

    fn state(&self, block: u64) -> u32 {
        host_page_state((self.host + block * BLOCK) as *const u8)
    }
}

fn process_commit() -> usize {
    // SAFETY: an all-zero PROCESS_MEMORY_COUNTERS_EX is a valid value.
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    // SAFETY: `counters` is sized and its size field is set, as the call requires.
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            counters.cb,
        )
    };
    assert_ne!(ok, 0);
    counters.PrivateUsage
}

#[test]
fn a_reserved_region_is_not_mapped_at_boot_and_refuses_dirty_tracking() {
    let Some(mut machine) = machine() else { return };
    assert!(machine.vm.has_reserved_ram());
    // Only the low RAM is a region that dirty tracking knows.
    assert_eq!(machine.vm.memory_regions.len(), 1);
    assert!(machine.vm.begin_dirty_tracking().is_err());
    assert_eq!(machine.state(0), MEM_RESERVE);
}

#[test]
fn ranges_map_after_commit_and_unmap_before_decommit() {
    let Some(machine) = machine() else { return };
    // One request of the guest can cover many blocks, and a later request a part of them.
    assert!(machine.commit(0, 8));
    assert!(machine.map(0, 8));
    // Unplug one block in the middle of a range that one call mapped.
    assert!(machine.unmap(3, 1));
    assert!(machine.decommit(3, 1));
    assert_eq!(machine.state(3), MEM_RESERVE);
    assert_eq!(machine.state(2), MEM_COMMIT);
    assert_eq!(machine.state(4), MEM_COMMIT);
    // Plug it again, then unplug the whole range in two parts that do not match the map.
    assert!(machine.commit(3, 1));
    assert!(machine.map(3, 1));
    assert!(machine.unmap(0, 5));
    assert!(machine.decommit(0, 5));
    assert!(machine.unmap(5, 3));
    assert!(machine.decommit(5, 3));
    for block in 0..8 {
        assert_eq!(machine.state(block), MEM_RESERVE);
    }
}

#[test]
fn many_plug_and_unplug_cycles_leave_no_commit_behind() {
    let Some(machine) = machine() else { return };
    let cycles = std::env::var("MEM_STRESS_STEPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20_000u64);
    let before = process_commit();
    let mut plugged = [false; BLOCKS as usize];
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    for _ in 0..cycles {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        let value = seed.wrapping_mul(0x2545_f491_4f6c_dd1d);
        let block = value % BLOCKS;
        let blocks = (1 + (value >> 8) % 4).min(BLOCKS - block);
        let range = block as usize..(block + blocks) as usize;
        if plugged[range.clone()].iter().all(|plugged| !*plugged) {
            assert!(machine.commit(block, blocks));
            assert!(machine.map(block, blocks));
            // The host can write the new memory, and it reads as zero first.
            let at = (machine.host + block * BLOCK) as *mut u64;
            // SAFETY: the range is committed.
            unsafe {
                assert_eq!(at.read_volatile(), 0);
                at.write_volatile(value);
            }
            plugged[range].fill(true);
        } else if plugged[range.clone()].iter().all(|plugged| *plugged) {
            assert!(machine.unmap(block, blocks));
            assert!(machine.decommit(block, blocks));
            plugged[range].fill(false);
        }
    }
    for block in 0..BLOCKS {
        if plugged[block as usize] {
            assert!(machine.unmap(block, 1));
            assert!(machine.decommit(block, 1));
        }
    }
    let after = process_commit();
    assert!(
        after < before + BLOCK as usize,
        "commit grew from {before} to {after} bytes"
    );
}

/// Records what the partition does in the cases that the device never makes. The design does
/// not rely on these results; the test prints them for the record and cleans up.
#[test]
fn report_partition_behavior_outside_the_contract() {
    let Some(machine) = machine() else { return };
    println!("unmap of a range that was never mapped: {}", machine.unmap(0, 1));
    println!("map of a reserved range that is not committed: {}", machine.map(1, 1));
    println!("  unmap after that: {}", machine.unmap(1, 1));
    assert!(machine.commit(2, 1));
    assert!(machine.map(2, 1));
    println!("map of a range that is mapped: {}", machine.map(2, 1));
    println!("decommit of a range that is mapped: {}", machine.decommit(2, 1));
    println!("  state after that: {:#x} (commit {MEM_COMMIT:#x}, reserve {MEM_RESERVE:#x})", machine.state(2));
    println!("  unmap after that: {}", machine.unmap(2, 1));
    machine.decommit(2, 1);
}
