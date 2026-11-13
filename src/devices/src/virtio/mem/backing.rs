//! Host memory behind the virtio-mem hotplug region on macOS and Windows.
//!
//! Linux gives the memory of an unplugged block back with `MADV_DONTNEED`. macOS and Windows
//! need more:
//!
//! - macOS keeps the pages of a range in the process while the hypervisor maps the range. The
//!   device unmaps the range, puts new zero-fill pages in its place, and maps it again.
//! - Windows charges committed memory against the commit limit (RAM plus page file), used or
//!   not. The hotplug region is only reserved at boot. The device commits and maps a range
//!   when the guest plugs it, and unmaps and decommits it when the guest unplugs it.
//!
//! Every call that changes host memory under the guest is in this file, and each call takes a
//! [`HostRange`]. Only [`HostRange::resolve`] makes one, and it proves that the range is inside
//! the one host mapping that backs the hotplug region. The guest chooses the block numbers of a
//! request. It never chooses a host address.

use std::io;

use vm_memory::{
    Address, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion,
    MemoryRegionAddress,
};

use super::device::VIRTIO_MEM_BLOCK_SIZE;

/// A range of whole blocks of the hotplug region, with the host address that backs it.
#[derive(Debug)]
pub(super) struct HostRange {
    host: *mut u8,
    guest_addr: u64,
    len: usize,
}

impl HostRange {
    /// Find the host memory of `len` bytes at `guest_addr`.
    ///
    /// `region_addr` and `region_size` are the hotplug region of the device. The call fails,
    /// and changes nothing, unless all of these are true:
    ///
    /// - the range is a whole number of blocks inside the region;
    /// - the region is exactly one region of guest memory, so that the range cannot cross into
    ///   a different host mapping;
    /// - the region is anonymous memory, because a file mapping cannot be replaced or
    ///   decommitted without a change to what the file shows.
    pub(super) fn resolve(
        memory: &GuestMemoryMmap,
        region_addr: u64,
        region_size: u64,
        guest_addr: u64,
        len: u64,
    ) -> io::Result<Self> {
        let refuse = |why: &'static str| io::Error::new(io::ErrorKind::InvalidInput, why);

        let offset = guest_addr
            .checked_sub(region_addr)
            .ok_or_else(|| refuse("the range starts below the hotplug region"))?;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| refuse("the range wraps"))?;
        if len == 0 || end > region_size {
            return Err(refuse("the range is not inside the hotplug region"));
        }
        if !offset.is_multiple_of(VIRTIO_MEM_BLOCK_SIZE) || !len.is_multiple_of(VIRTIO_MEM_BLOCK_SIZE)
        {
            return Err(refuse("the range is not a whole number of blocks"));
        }

        let region = memory
            .find_region(GuestAddress(region_addr))
            .ok_or_else(|| refuse("the hotplug region is not guest memory"))?;
        if region.start_addr().raw_value() != region_addr || region.len() != region_size {
            return Err(refuse("the hotplug region is not one region of guest memory"));
        }
        if region.file_offset().is_some() {
            return Err(refuse("the hotplug region is a file mapping"));
        }
        #[cfg(unix)]
        if region.flags() & libc::MAP_PRIVATE == 0 || region.flags() & libc::MAP_ANON == 0 {
            return Err(refuse("the hotplug region is not private anonymous memory"));
        }

        let host = region
            .get_host_address(MemoryRegionAddress(offset))
            .map_err(io::Error::other)?;
        let len = usize::try_from(len).map_err(|_| refuse("the range exceeds the address space"))?;
        Ok(Self {
            host,
            guest_addr,
            len,
        })
    }

    pub(super) fn host_addr(&self) -> u64 {
        self.host as u64
    }

    pub(super) fn guest_addr(&self) -> u64 {
        self.guest_addr
    }

    pub(super) fn len(&self) -> u64 {
        self.len as u64
    }
}

/// Stop the VM process.
///
/// Use this only when a host call failed after the device changed a mapping and the change
/// cannot be undone. The hypervisor mapping and the device state then do not agree, and a guest
/// that continued would run on memory that the device does not account for.
pub(super) fn fail_stop(what: &str, error: &io::Error) -> ! {
    error!("virtio-mem: cannot {what}: {error}; the VM stops");
    std::process::abort()
}

/// Put new zero-fill pages in the place of a range. The old pages go back to the host at once.
///
/// The hypervisor must not map the range during the call: it would keep the old pages.
/// A thread of this process that reads or writes the range during the call sees the old pages
/// or the new pages. `MAP_FIXED` replaces a mapping in one step, so the range is never without
/// memory.
#[cfg(target_os = "macos")]
pub(super) fn replace_with_zero_pages(range: &HostRange) -> io::Result<()> {
    // SAFETY: `HostRange::resolve` proved that the range is inside the private anonymous mapping
    // of the hotplug region, so `MAP_FIXED` cannot replace memory with a different owner. The
    // new mapping has the type and the protection of the old mapping.
    let replaced = unsafe {
        libc::mmap(
            range.host.cast(),
            range.len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if replaced == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Commit that the VMM process keeps free for its own allocations after a plug.
#[cfg(target_os = "windows")]
const COMMIT_RESERVE: u64 = 512 << 20;

/// The most that one step commits before the commit reserve is checked again. The tests use a
/// small step, so that a range of the test region takes more than one.
#[cfg(target_os = "windows")]
const COMMIT_STEP: usize = if cfg!(test) { 8 << 20 } else { 64 << 20 };

/// Commit a reserved range and keep the commit reserve. The new pages read as zero. On failure
/// nothing is committed.
///
/// A VMM whose own allocations fail aborts and takes the guest with it, and other programs need
/// commit too. So the commit is made in steps, with a check of the reserve after each step: a
/// guest that asks for a large range can take the system at most one step into the reserve, and
/// only for the time of the check. The commit comes before the check so that Windows can grow a
/// page file that it manages.
#[cfg(target_os = "windows")]
pub(super) fn commit(range: &HostRange) -> io::Result<()> {
    use windows_sys::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT, PAGE_READWRITE};
    let mut done = 0;
    while done < range.len {
        let step = (range.len - done).min(COMMIT_STEP);
        // SAFETY: `done + step` is at most the length of the range. The call changes only the
        // commit state of that part, and Windows refuses a part that is not inside one
        // reservation of this process.
        let committed = unsafe {
            VirtualAlloc(range.host.add(done).cast(), step, MEM_COMMIT, PAGE_READWRITE)
        };
        let result = if committed.is_null() {
            Err(io::Error::last_os_error())
        } else {
            done += step;
            commit_headroom()
        };
        if let Err(error) = result {
            if done > 0 {
                let partial = HostRange {
                    host: range.host,
                    guest_addr: range.guest_addr,
                    len: done,
                };
                // An unplugged range must not stay committed.
                if let Err(error) = decommit(&partial) {
                    fail_stop("decommit a range that was not plugged", &error);
                }
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Decommit a range and keep its address space reserved.
///
/// The partition must not map the range. The reservation stays, so no other allocation of this
/// process can take the addresses: a thread that still holds a pointer into the range gets an
/// access violation, never the memory of a different owner.
#[cfg(target_os = "windows")]
pub(super) fn decommit(range: &HostRange) -> io::Result<()> {
    use windows_sys::Win32::System::Memory::{VirtualFree, MEM_DECOMMIT};
    // SAFETY: the call changes only the commit state of the range. Windows refuses a range
    // that is not inside one reservation of this process.
    if unsafe { VirtualFree(range.host.cast(), range.len, MEM_DECOMMIT) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Fail when less than [`COMMIT_RESERVE`] of commit is left, for the whole system (RAM plus
/// page file) or under the per-process memory limit of the job that holds this process.
#[cfg(target_os = "windows")]
fn commit_headroom() -> io::Result<()> {
    use windows_sys::Win32::System::JobObjects::{
        JobObjectExtendedLimitInformation, QueryInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: an all-zero MEMORYSTATUSEX is a valid value.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` is sized and its length field is set, as the call requires.
    if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 && status.ullAvailPageFile < COMMIT_RESERVE
    {
        return Err(io::Error::other(format!(
            "only {} MiB of system commit would be left",
            status.ullAvailPageFile >> 20
        )));
    }

    // SAFETY: an all-zero JOBOBJECT_EXTENDED_LIMIT_INFORMATION is a valid value.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a null job handle asks for the job of this process; the buffer has the size given.
    let in_job = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    } != 0;
    if in_job && limits.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_PROCESS_MEMORY != 0 {
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
        } != 0;
        let limit = limits.ProcessMemoryLimit as u64;
        let used = counters.PrivateUsage as u64;
        if ok && limit.saturating_sub(used) < COMMIT_RESERVE {
            return Err(io::Error::other(format!(
                "only {} MiB would be left under the job memory limit",
                limit.saturating_sub(used) >> 20
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 2 << 30;
    const BLOCK: u64 = VIRTIO_MEM_BLOCK_SIZE;
    const SIZE: u64 = 4 * BLOCK;

    fn memory(ranges: &[(u64, u64)]) -> GuestMemoryMmap {
        let ranges: Vec<_> = ranges
            .iter()
            .map(|(start, size)| (GuestAddress(*start), *size as usize))
            .collect();
        GuestMemoryMmap::from_ranges(&ranges).unwrap()
    }

    #[test]
    fn a_range_of_whole_blocks_inside_the_region_resolves() {
        let memory = memory(&[(0, 0x4000), (BASE, SIZE)]);
        let base = memory.get_host_address(GuestAddress(BASE)).unwrap() as u64;
        for (first, count) in [(0, 1), (0, 4), (3, 1), (1, 2)] {
            let range =
                HostRange::resolve(&memory, BASE, SIZE, BASE + first * BLOCK, count * BLOCK)
                    .unwrap();
            assert_eq!(range.host_addr(), base + first * BLOCK);
            assert_eq!(range.guest_addr(), BASE + first * BLOCK);
            assert_eq!(range.len(), count * BLOCK);
        }
    }

    #[test]
    fn a_range_outside_the_region_or_off_a_block_boundary_is_refused() {
        let memory = memory(&[(0, 0x4000), (BASE, SIZE)]);
        for (addr, len) in [
            (BASE, 0),
            (BASE - BLOCK, BLOCK),
            (BASE, SIZE + BLOCK),
            (BASE + 3 * BLOCK, 2 * BLOCK),
            (BASE + SIZE, BLOCK),
            (BASE + 4096, BLOCK),
            (BASE, BLOCK - 4096),
            (BASE, 4096),
            (0, BLOCK),
            (u64::MAX - BLOCK + 1, BLOCK),
            (BASE, u64::MAX),
            (BASE + BLOCK, u64::MAX - BLOCK + 1),
        ] {
            assert!(
                HostRange::resolve(&memory, BASE, SIZE, addr, len).is_err(),
                "0x{addr:x}+0x{len:x}"
            );
        }
    }

    /// A range must not reach a host mapping that is not the hotplug region. Two regions of
    /// guest memory that are next to each other have host mappings that are not.
    #[test]
    fn a_region_that_is_not_one_region_of_guest_memory_is_refused() {
        let split = memory(&[(BASE, 2 * BLOCK), (BASE + 2 * BLOCK, 2 * BLOCK)]);
        assert!(HostRange::resolve(&split, BASE, SIZE, BASE, BLOCK).is_err());
        assert!(HostRange::resolve(&split, BASE, SIZE, BASE + BLOCK, 2 * BLOCK).is_err());

        let larger = memory(&[(BASE, SIZE + BLOCK)]);
        assert!(HostRange::resolve(&larger, BASE, SIZE, BASE, BLOCK).is_err());

        let shifted = memory(&[(BASE - BLOCK, SIZE + BLOCK)]);
        assert!(HostRange::resolve(&shifted, BASE, SIZE, BASE, BLOCK).is_err());

        let absent = memory(&[(0, 0x4000)]);
        assert!(HostRange::resolve(&absent, BASE, SIZE, BASE, BLOCK).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_file_mapping_is_refused() {
        use std::io::Write;
        use std::sync::Arc;
        use vm_memory::mmap::MmapRegionBuilder;
        use vm_memory::{FileOffset, GuestRegionMmap};

        let (fd, path) =
            nix::unistd::mkstemp(&std::env::temp_dir().join("mem-backing-XXXXXX")).unwrap();
        let mut file = std::fs::File::from(fd);
        std::fs::remove_file(path).unwrap();
        file.write_all(&vec![0; BLOCK as usize]).unwrap();
        let region = MmapRegionBuilder::new(BLOCK as usize)
            .with_file_offset(FileOffset::new(file, 0))
            .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE)
            .with_mmap_flags(libc::MAP_PRIVATE)
            .build()
            .unwrap();
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)])
            .unwrap()
            .insert_region(Arc::new(
                GuestRegionMmap::new(region, GuestAddress(BASE)).unwrap(),
            ))
            .unwrap();
        assert!(HostRange::resolve(&memory, BASE, BLOCK, BASE, BLOCK).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn replaced_pages_read_as_zero_and_the_pages_next_to_them_keep_their_data() {
        let memory = memory(&[(BASE, SIZE)]);
        let base = memory.get_host_address(GuestAddress(BASE)).unwrap();
        // SAFETY: the region is mapped and writable.
        unsafe { std::ptr::write_bytes(base, 0x5a, SIZE as usize) };
        let range = HostRange::resolve(&memory, BASE, SIZE, BASE + BLOCK, 2 * BLOCK).unwrap();
        replace_with_zero_pages(&range).unwrap();
        // SAFETY: the region is still mapped.
        let bytes = unsafe { std::slice::from_raw_parts(base, SIZE as usize) };
        let block = BLOCK as usize;
        assert!(bytes[..block].iter().all(|byte| *byte == 0x5a));
        assert!(bytes[block..3 * block].iter().all(|byte| *byte == 0));
        assert!(bytes[3 * block..].iter().all(|byte| *byte == 0x5a));
    }
}
