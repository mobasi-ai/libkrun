# Host memory release for virtio-mem on macOS and Windows

This fork changes one thing in the VM engine: when a guest unplugs a virtio-mem block, the host
gets the memory of the block back on macOS and Windows. Upstream does this only on Linux.

The guest is not trusted. This document gives the design, the rules that keep a hostile guest
inside its VM, what the change does not do, and how to run the tests.

## What each host does

| Host | At boot | Plug of a range | Unplug of a range |
| --- | --- | --- | --- |
| Linux (KVM) | The hotplug region is mapped. | Nothing. | `MADV_DONTNEED` (upstream). |
| macOS (HVF) | The hotplug region is mapped. | Nothing. | Unmap from the hypervisor, put new zero pages in its place (`mmap` with `MAP_FIXED`), map it again. |
| Windows (WHP) | The hotplug region is only reserved. It costs no commit and is not mapped. | Commit in steps of 64 MiB with a check of the commit reserve after each step, then map. | Unmap, decommit. |

On macOS and Linux the hypervisor always maps the whole region. On Windows it maps exactly the
plugged blocks.

Windows needs the different design because it charges committed memory against the commit limit
(RAM plus page file) at once, used or not. A region that is committed at boot costs its full
size.

## Where the code is

| File | Content |
| --- | --- |
| `src/devices/src/virtio/mem/backing.rs` | Every host call that changes memory under the guest, and the `HostRange` type. |
| `src/devices/src/virtio/mem/device.rs` | The request handlers. They call `back_range` and `release_backing`. |
| `src/devices/src/virtio/mem/mod.rs` | The `RamMapper` trait: map and unmap of hot-plugged RAM in the hypervisor. |
| `src/vmm/src/macos/vstate.rs`, `src/vmm/src/windows/vstate.rs` | The mapper of each host. |
| `src/vmm/src/builder.rs` | The reserved region on Windows, and the choice to give the device a mapper. |
| `third_party/msb-vm-memory` | A copy of the upstream crate with one added function, `MmapRegion::new_reserved`. |

## Rules

1. **The guest never chooses a host address.** A request names blocks. `HostRange::resolve`
   turns blocks into a host range and refuses the request unless the range is a whole number
   of blocks inside the region, the region is exactly one region of guest memory, and the
   region is private anonymous memory. Each host call takes a `HostRange`.
2. **Unmap before release.** The hypervisor does not map a range while the host replaces or
   decommits its memory.
3. **A Windows range stays reserved.** A decommit keeps the addresses. No other allocation can
   take them, so a stale host pointer gets an access violation and never the memory of a
   different owner.
4. **The bitmap always equals what the host backs.** A request changes the bitmap only after
   the host calls succeeded. `UNPLUG_ALL` records each run of blocks when it is released.
5. **The device waits for no other thread.** It calls the mapper on its own thread. The mapper
   takes no lock of the VMM, so a host control call that holds the VMM lock and waits for the
   device cannot block the device. The Windows mapper shares the partition with the VM, so the
   partition lives as long as the device can use it.
6. **A failure has one safe result.**

| Event | Result |
| --- | --- |
| The host cannot commit a plugged range (Windows error 1455), or the commit reserve of 512 MiB would be used | The answer is `BUSY`. Nothing changed. The Linux driver asks again after a delay. |
| The hypervisor refuses to map a plugged range (Windows) | The mapper unmaps the range, so that no part of it stays mapped. The range is decommitted. The answer is `BUSY`. If that unmap fails, the VM process stops. |
| The driver did not acknowledge `VIRTIO_MEM_F_UNPLUGGED_INACCESSIBLE` (Windows) | A plug gets `ERROR`. Such a driver can read unplugged memory, which stops the VM on Windows, so it gets no memory. Linux 5.16 and later acknowledge the feature. A write of a features page replaces the page, so a driver that starts after a device reset does not inherit the acknowledgement of the driver before it. |
| A commit step or a map failed, and the decommit of what was committed fails too (Windows) | The VM process stops. An unplugged range must not stay committed: the VMM answers a guest access to committed memory that is not mapped from the host memory. |
| The hypervisor refuses to unmap (Windows) | The answer is `BUSY`. The range stays plugged, committed, and mapped. |
| The VMM refuses to unmap because first-write tracking is active (macOS) | The range is zeroed in place, as upstream does. It stays mapped. |
| `mmap` cannot replace the pages (macOS) | The range is mapped again and zeroed in place. |
| The hypervisor cannot map a range again after the unmap (macOS), or a decommit fails after the unmap (Windows) | The VM process stops (`fail_stop`). The mapping and the bitmap do not agree, and the change cannot be undone. |

`ERROR` is not used for a condition that can pass: the Linux driver stops for good after
`ERROR`.

## What a hostile guest can do

These limits are accepted. Each one stays inside the VM of the guest.

- **Stop its own VM.** On Windows a guest that reads or writes a range that is not plugged gets
  a memory access exit, and the VM stops with the error "guest memory that is not plugged". A
  guest that points a different device (disk, network) at such a range makes a host thread
  fault, and the VMM process ends with an access violation.
- **Use memory up to the maximum.** On macOS and Linux the region stays mapped, so a guest can
  touch blocks that are not plugged. The host gives them memory. The limit is the maximum size
  of the VM, and the host must count that size, not the plugged size. A guest also sees the
  data that it wrote itself to such a block, until an unplug of the block zeroes it.
- **Keep memory.** The host asks for a size. A guest that does not unplug keeps what it has.
- **Make one host thread busy.** There is no limit on the rate of plug and unplug requests.
- **Take commit for a moment (Windows).** A plug commits before it checks the reserve, so that
  Windows can grow a page file that it manages. One step can take the system at most 64 MiB
  into the reserve of 512 MiB, for the time of the check.

## What is not supported

- **Windows: memory capture, restore, and dirty tracking** of a VM with a reserved hotplug
  region, and the restore of the virtio-mem device state. The calls return an error.
  Hot-plugged ranges are mapped outside the regions that dirty tracking knows, and a restored
  bitmap would not equal what the host backs.
- **Windows: NUMA placement or private memory together with the release.** The region is then
  committed and mapped at boot, and the device keeps the upstream behavior.
- **macOS: memory of the boot RAM** is not given back. Only unplugged blocks are. Start a VM
  small and let it grow.

## Tests

| Test | What it proves | Command |
| --- | --- | --- |
| Model check (unit) | Random and hostile requests through a real virtqueue agree with a model. The device makes the real host calls; a thread stands in for the hypervisor and records each map and unmap. Checks data in plugged blocks, zero on plug, commit state (Windows), and guard memory next to the region (Unix). | `cargo test -p msb_krun_devices --lib virtio::mem` |
| Long model check | The same, for many steps. | `MEM_STRESS_STEPS=5000000 cargo test --release -p msb_krun_devices --lib virtio::mem::device::release_tests` |
| Fuzz | Coverage-guided input to the same harness. | `cd fuzz && cargo +nightly fuzz run virtio_mem_requests -- -max_len=1200 -len_control=0` |
| Real partition (Windows) | The commit, map, unmap, and decommit calls on a real Windows Hypervisor Platform partition. | `cargo test -p msb_krun_vmm --lib hotplug_tests -- --test-threads=1` |
| End to end | A real guest: grow, fill, free, shrink; a resize storm with a data check; a hostile driver in the guest. | The scripts in `scripts/mobasi-dynamic-memory` of the Microsandbox fork. |

A seed corpus for the fuzz target:
`MEM_FUZZ_SEEDS=$PWD/fuzz/corpus/virtio_mem_requests cargo test -p msb_krun_devices --lib write_fuzz_seeds -- --ignored`

## The vendored crate

`third_party/msb-vm-memory` must equal the published `msb-vm-memory` of the version in
`src/devices/Cargo.toml`, plus `MmapRegion::new_reserved` in `src/mmap/windows.rs`. When
upstream changes the version, copy the new version and add the function again. To check:

```sh
diff -ru ~/.cargo/registry/src/*/msb-vm-memory-<version>/src third_party/msb-vm-memory/src
```

The only difference must be `new_reserved`.
