//! Tests of how the device gives host memory back, under random and hostile guest requests.

use super::harness::{Harness, BASE, BLOCKS, REGION, STEP_BYTES};
use super::*;

const BLOCK: usize = VIRTIO_MEM_BLOCK_SIZE as usize;

/// A small repeatable generator, so that a failed run can be run again from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn step(&mut self) -> [u8; STEP_BYTES] {
        let mut step = [0; STEP_BYTES];
        step[..8].copy_from_slice(&self.next().to_le_bytes());
        step[8..].copy_from_slice(&self.next().to_le_bytes()[..4]);
        step
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Pages of a host range that hold memory now.
#[cfg(unix)]
fn resident_pages(host: usize, len: usize) -> usize {
    // SAFETY: the page size query has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let mut pages = vec![0u8; len.div_ceil(page)];
    // SAFETY: the range is mapped, and the vector has one byte for each page of it.
    let result = unsafe { libc::mincore(host as *mut _, len, pages.as_mut_ptr().cast()) };
    assert_eq!(result, 0);
    pages.iter().filter(|page| **page & 1 != 0).count()
}

/// Commit that Windows charges to this process.
#[cfg(target_os = "windows")]
fn process_commit() -> usize {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
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
fn random_and_hostile_requests_agree_with_the_model() {
    let seed = env_u64("MEM_STRESS_SEED", 0x9e37_79b9_7f4a_7c15);
    let steps = env_u64("MEM_STRESS_STEPS", 20_000);
    let mut rng = Rng(seed);
    let mut harness = Harness::new();
    for _ in 0..steps {
        harness.step(&rng.step());
    }
    harness.finish();
    let stats = harness.stats;
    println!("seed {seed:#x}, {steps} steps: {stats:?}");
    // The run must reach the states that matter, and not only refusals.
    assert!(stats.blocks_plugged > steps / 20, "{stats:?}");
    assert!(stats.blocks_unplugged > steps / 20, "{stats:?}");
    assert!(stats.nacks > 0 && stats.errors > 0, "{stats:?}");
}

#[test]
fn hostile_addresses_and_counts_change_nothing() {
    let mut harness = Harness::new();
    harness.set_requested(BLOCKS);
    assert_eq!(harness.plug(4, 2), VIRTIO_MEM_RESP_ACK);
    let block = VIRTIO_MEM_BLOCK_SIZE;
    let end = BASE + REGION;

    // (address, block count): each one is outside the region, or not on a block boundary.
    let outside = [
        (0, 1),
        (BASE - block, 1),
        (BASE - block, 2),
        (BASE - 1, 1),
        (BASE + 1, 1),
        (BASE + block - 1, 1),
        (BASE + 4096, 1),
        (end, 1),
        (end - block, 2),
        (BASE, BLOCKS as u16 + 1),
        (BASE, u16::MAX),
        (BASE + block, BLOCKS as u16),
        (BASE, 0),
        (u64::MAX, 1),
        (u64::MAX - block + 1, 1),
        (u64::MAX - block + 1, u16::MAX),
        (1 << 63, 1),
        (BASE.wrapping_add(1 << 63), 1),
    ];
    for kind in [VIRTIO_MEM_REQ_PLUG, VIRTIO_MEM_REQ_UNPLUG, VIRTIO_MEM_REQ_STATE] {
        for (addr, count) in outside {
            assert_eq!(
                harness.guest_raw(kind, addr, count),
                VIRTIO_MEM_RESP_ERROR,
                "kind {kind} at 0x{addr:x} for {count} blocks"
            );
        }
    }
    for kind in [4, 5, 0x100, u16::MAX] {
        assert_eq!(harness.guest_raw(kind, BASE, 1), VIRTIO_MEM_RESP_ERROR);
    }

    // Inside the region, but not what the state of the blocks permits.
    assert_eq!(harness.plug(4, 1), VIRTIO_MEM_RESP_NACK);
    assert_eq!(harness.plug(3, 2), VIRTIO_MEM_RESP_NACK);
    assert_eq!(harness.unplug(3, 2), VIRTIO_MEM_RESP_NACK);
    assert_eq!(harness.unplug(6, 1), VIRTIO_MEM_RESP_NACK);
    harness.set_requested(2);
    assert_eq!(harness.plug(0, 1), VIRTIO_MEM_RESP_NACK);

    assert_eq!(
        harness.plugged().iter().filter(|plugged| **plugged).count(),
        2
    );
    harness.finish();
}

/// Run `body` in a new process that runs only the named test. The other tests of this file
/// commit and decommit memory at the same time, and the commit of a process is one number.
#[cfg(target_os = "windows")]
fn alone(test: &str, body: impl FnOnce()) {
    const CHILD: &str = "MEM_TEST_ALONE";
    if std::env::var_os(CHILD).is_some() {
        return body();
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("virtio::mem::device::release_tests::{test}"),
            "--nocapture",
        ])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(status.success(), "{test} failed in its own process");
}

#[test]
fn unplug_gives_the_host_memory_back() {
    #[cfg(target_os = "windows")]
    alone("unplug_gives_the_host_memory_back", release_and_measure);
    #[cfg(unix)]
    release_and_measure();
}

fn release_and_measure() {
    let mut harness = Harness::new();
    harness.set_requested(BLOCKS);
    let (host, _) = harness.host_region();
    let blocks = 8;
    #[cfg(target_os = "windows")]
    let before = process_commit();

    assert_eq!(harness.plug(0, blocks), VIRTIO_MEM_RESP_ACK);
    // Touch every page, as a guest that fills its memory does. Keep the tags of the harness.
    for offset in (0..blocks as usize * BLOCK).step_by(1024) {
        // SAFETY: the blocks are plugged, so their host memory is writable.
        unsafe { ((host + offset + 64) as *mut u8).write_volatile(0xa5) };
    }
    #[cfg(unix)]
    assert!(resident_pages(host, blocks as usize * BLOCK) >= blocks as usize * BLOCK / 16384);
    #[cfg(target_os = "windows")]
    assert!(process_commit() >= before + blocks as usize * BLOCK);

    assert_eq!(harness.unplug(0, blocks), VIRTIO_MEM_RESP_ACK);
    // The check of the harness reads three places of each block, which gives each of them a
    // new zero page. No other page of the range holds memory.
    #[cfg(unix)]
    assert!(resident_pages(host, blocks as usize * BLOCK) <= blocks as usize * 3);
    #[cfg(target_os = "windows")]
    assert!(process_commit() < before + BLOCK);

    // The same blocks can be plugged again. The harness checks that they read as zero.
    assert_eq!(harness.plug(0, blocks), VIRTIO_MEM_RESP_ACK);
    harness.finish();
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
fn a_refused_unmap_never_releases_mapped_memory() {
    let mut harness = Harness::new();
    harness.set_requested(BLOCKS);
    assert_eq!(harness.plug(2, 3), VIRTIO_MEM_RESP_ACK);

    // The hypervisor refuses the unmap, as the VMM does on macOS while it tracks first writes.
    harness.refuse_unmap_after(0);
    // Windows keeps the range plugged, committed, and mapped, and tells the guest to ask again.
    #[cfg(target_os = "windows")]
    {
        assert_eq!(harness.unplug(2, 3), VIRTIO_MEM_RESP_BUSY);
        assert!(harness.plugged()[2..5].iter().all(|plugged| *plugged));
    }
    // macOS zeroes the range in place: it stays mapped, and the unplug completes.
    #[cfg(target_os = "macos")]
    {
        assert_eq!(harness.unplug(2, 3), VIRTIO_MEM_RESP_ACK);
        assert_eq!(harness.plug(2, 3), VIRTIO_MEM_RESP_ACK);
    }
    // The next unmap is accepted, and the range is released.
    assert_eq!(harness.unplug(2, 3), VIRTIO_MEM_RESP_ACK);
    harness.finish();
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
fn unplug_all_records_each_released_run() {
    let mut harness = Harness::new();
    harness.set_requested(BLOCKS);
    for block in [0, 4, 8] {
        assert_eq!(harness.plug(block, 2), VIRTIO_MEM_RESP_ACK);
    }
    // The hypervisor accepts the unmap of the first run and refuses the second.
    harness.refuse_unmap_after(1);
    #[cfg(target_os = "windows")]
    {
        assert_eq!(harness.unplug_all(), VIRTIO_MEM_RESP_BUSY);
        let plugged = harness.plugged();
        assert!(!plugged[0] && !plugged[1], "the first run is released");
        assert!(plugged[4] && plugged[5] && plugged[8] && plugged[9]);
        // The guest asks again, and the other runs follow.
        assert_eq!(harness.unplug_all(), VIRTIO_MEM_RESP_ACK);
    }
    #[cfg(target_os = "macos")]
    assert_eq!(harness.unplug_all(), VIRTIO_MEM_RESP_ACK);
    assert!(harness.plugged().iter().all(|plugged| !*plugged));
    harness.finish();
}

#[cfg(target_os = "windows")]
#[test]
fn a_refused_map_leaves_the_range_reserved() {
    let mut harness = Harness::new();
    harness.set_requested(BLOCKS);
    harness.refuse_next_map();
    // The harness checks that the range is not committed and not mapped after the refusal.
    assert_eq!(harness.plug(0, 8), VIRTIO_MEM_RESP_BUSY);
    assert_eq!(harness.plug(0, 8), VIRTIO_MEM_RESP_ACK);
    harness.finish();
}

/// Put this process in a job with a memory limit, or change the limit of that job.
#[cfg(target_os = "windows")]
fn limit_process_commit(job: &mut isize, bytes: usize) {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain calls with valid arguments; an all-zero limit structure is a valid value.
    unsafe {
        let first = *job == 0;
        if first {
            *job = CreateJobObjectW(std::ptr::null(), std::ptr::null()) as isize;
            assert_ne!(*job, 0);
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        limits.ProcessMemoryLimit = bytes;
        assert_ne!(
            SetInformationJobObject(
                *job as _,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ),
            0
        );
        if first {
            assert_ne!(AssignProcessToJobObject(*job as _, GetCurrentProcess()), 0);
        }
    }
}

/// Windows refuses a commit when the commit limit is reached (error 1455). A job memory limit
/// makes the same refusal for this process only. The guest must get BUSY, and no part of the
/// range can stay committed or mapped: the harness checks both after each request.
#[cfg(target_os = "windows")]
#[test]
fn a_commit_that_windows_refuses_answers_busy_and_changes_nothing() {
    alone("a_commit_that_windows_refuses_answers_busy_and_changes_nothing", || {
        let mut harness = Harness::new();
        harness.set_requested(BLOCKS);
        assert_eq!(harness.plug(0, 1), VIRTIO_MEM_RESP_ACK);
        let mut job = 0;

        // Less than the reserve of 512 MiB is left: the commit succeeds, the check of the
        // reserve fails, and the commit goes back.
        limit_process_commit(&mut job, process_commit() + (300 << 20));
        harness.expect_refused_commit();
        assert_eq!(harness.plug(1, 8), VIRTIO_MEM_RESP_BUSY);

        // Less than the range is left: Windows refuses the commit itself.
        limit_process_commit(&mut job, process_commit() + (8 << 20));
        harness.expect_refused_commit();
        assert_eq!(harness.plug(1, 16), VIRTIO_MEM_RESP_BUSY);

        // The block that was plugged before keeps its data, and an unplug still works.
        assert_eq!(harness.unplug(0, 1), VIRTIO_MEM_RESP_ACK);

        // With commit available again, the same plug succeeds.
        limit_process_commit(&mut job, process_commit() + (2048 << 20));
        assert_eq!(harness.plug(1, 16), VIRTIO_MEM_RESP_ACK);
        harness.finish();
    });
}

/// A driver that did not acknowledge VIRTIO_MEM_F_UNPLUGGED_INACCESSIBLE can read unplugged
/// memory, which stops the VM on Windows. The device gives such a driver no memory.
#[cfg(target_os = "windows")]
#[test]
fn a_driver_that_can_read_unplugged_memory_gets_no_memory() {
    let mut harness = Harness::new();
    harness.set_requested(BLOCKS);
    harness.forget_features();
    assert_eq!(harness.plug(0, 1), VIRTIO_MEM_RESP_ERROR);
    assert!(harness.plugged().iter().all(|plugged| !*plugged));
    harness.finish();
}

/// Host threads of other devices hold pointers into guest memory. On Unix a thread that reads
/// or writes a range while the device releases it must find memory there: the old pages or the
/// new zero pages.
#[cfg(unix)]
#[test]
fn host_threads_can_touch_the_region_while_ranges_are_released() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    let seed = env_u64("MEM_STRESS_SEED", 0x0dd0_5eed_cafe_f00d);
    let steps = env_u64("MEM_STRESS_STEPS", 20_000) / 4;
    let mut harness = Harness::new();
    let (host, len) = harness.host_region();
    let stop = Arc::new(AtomicBool::new(false));
    let touches = Arc::new(AtomicU64::new(0));
    let threads: Vec<_> = (0..3u64)
        .map(|thread| {
            let (stop, touches) = (stop.clone(), touches.clone());
            std::thread::spawn(move || {
                let mut rng = Rng(seed ^ (thread + 1));
                while !stop.load(Ordering::Relaxed) {
                    // Offset 64 of a page: the harness keeps its tags at other offsets.
                    let page = (rng.next() as usize % (len / 4096)) * 4096;
                    let at = (host + page + 64) as *mut u8;
                    // SAFETY: the whole region stays mapped on Unix.
                    unsafe {
                        if thread == 0 {
                            std::hint::black_box(at.read_volatile());
                        } else {
                            at.write_volatile(thread as u8);
                        }
                    }
                    touches.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    let mut rng = Rng(seed);
    for _ in 0..steps {
        harness.step(&rng.step());
    }
    stop.store(true, Ordering::Relaxed);
    for thread in threads {
        thread.join().unwrap();
    }
    harness.finish();
    assert!(touches.load(Ordering::Relaxed) > steps);
    assert!(harness.stats.blocks_unplugged > steps / 20);
}

/// On Windows an unplugged range has no host memory. A host thread that still holds a pointer
/// into it must get an access violation, and nothing else: the addresses stay reserved, so no
/// other allocation can be there.
#[cfg(target_os = "windows")]
#[test]
fn host_access_to_an_unplugged_range_is_an_access_violation() {
    const CHILD: &str = "MEM_TEST_TOUCH_UNPLUGGED";
    if std::env::var_os(CHILD).is_some() {
        let mut harness = Harness::new();
        harness.set_requested(BLOCKS);
        assert_eq!(harness.plug(0, 1), VIRTIO_MEM_RESP_ACK);
        assert_eq!(harness.unplug(0, 1), VIRTIO_MEM_RESP_ACK);
        let (host, _) = harness.host_region();
        // SAFETY: none. The read must end this process with an access violation.
        std::hint::black_box(unsafe { (host as *const u8).read_volatile() });
        std::process::exit(0);
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "virtio::mem::device::release_tests::host_access_to_an_unplugged_range_is_an_access_violation",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0xC000_0005_u32 as i32));
}

/// Writes a seed corpus for the fuzz target, so that the fuzzer starts from long runs that
/// plug and unplug many blocks:
/// `MEM_FUZZ_SEEDS=<folder> cargo test -p msb_krun_devices --lib write_fuzz_seeds -- --ignored`
#[test]
#[ignore = "writes files; run it by name with MEM_FUZZ_SEEDS set"]
fn write_fuzz_seeds() {
    let folder = std::path::PathBuf::from(std::env::var_os("MEM_FUZZ_SEEDS").unwrap());
    std::fs::create_dir_all(&folder).unwrap();
    for seed in 1..=32u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let bytes: Vec<u8> = (0..100).flat_map(|_| rng.step()).collect();
        std::fs::write(folder.join(format!("seed-{seed:02}")), bytes).unwrap();
    }
}
