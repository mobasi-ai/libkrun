//! Drives the virtio-mem device with arbitrary guest requests and checks it against a model.
//!
//! The unit tests and the fuzz target use this file. A step takes 12 bytes of input and makes
//! one host action or one guest request: a valid one, or one with a hostile address, block
//! count, request type, or descriptor chain. After each step the harness compares the device
//! with a model that it keeps apart from the device code, and checks the host memory:
//!
//! - the response and the plugged blocks agree with the model;
//! - a plugged block keeps the data that was written to it;
//! - a block reads as zero when the guest plugs it again;
//! - the hypervisor maps what it must: all blocks on macOS, the plugged blocks on Windows;
//! - on Windows a block is committed only while it is plugged;
//! - on Unix the host memory on each side of the hotplug region is not changed.
//!
//! A thread stands in for the VMM worker. It answers the map and unmap requests of the device,
//! records what a hypervisor would map, and can refuse a request to inject a failure. The
//! device makes the real host calls (`mmap`, `VirtualAlloc`, `VirtualFree`, `madvise`).

#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::sync::Mutex;

use vm_memory::{GuestRegionMmap, MmapRegion};

use super::*;
use crate::legacy::DummyIrqChip;
use crate::virtio::{MemoryAccessDomain, Queue};

/// Blocks in the hotplug region of the harness.
pub const BLOCKS: usize = 24;
/// Input bytes of one step.
pub const STEP_BYTES: usize = 12;

const BLOCK: u64 = VIRTIO_MEM_BLOCK_SIZE;
/// Guest address of the hotplug region of the harness.
pub const BASE: u64 = 2 << 30;
/// Size of the hotplug region of the harness.
pub const REGION: u64 = BLOCKS as u64 * BLOCK;
/// Guest memory below the hotplug region: the virtqueue and the request buffers.
const LOW: usize = 0x8000;
const DESC_TABLE: u64 = 0x0000;
const AVAIL_RING: u64 = 0x1000;
const USED_RING: u64 = 0x2000;
const REQUEST: u64 = 0x3000;
const RESPONSE: u64 = 0x3800;
/// An address that is in no region of guest memory.
const NOWHERE: u64 = 0x10_0000;
/// Offsets in a block that hold its tag. The harness reads them as zero in an unplugged block.
const TAG_OFFSETS: [u64; 3] = [0, BLOCK / 2, BLOCK - 8];
#[cfg(unix)]
const GUARD: usize = 1 << 20;
#[cfg(unix)]
const GUARD_BYTE: u8 = 0xc3;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

/// What a hypervisor would map, as the stand-in for the VMM worker records it.
#[cfg(any(target_os = "macos", target_os = "windows"))]
struct Hypervisor {
    mapped: Mutex<Vec<bool>>,
    violations: Mutex<Vec<String>>,
    /// Number of unmap requests to accept before one is refused. `usize::MAX` refuses none.
    unmaps_before_refusal: Mutex<usize>,
    refuse_next_map: AtomicBool,
    host_base: usize,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Hypervisor {
    fn violation(&self, text: String) {
        self.violations.lock().unwrap().push(text);
    }

    /// The blocks of a request, or `None` after a recorded violation.
    fn blocks(&self, what: &str, guest_addr: u64, len: u64) -> Option<std::ops::Range<usize>> {
        let inside = guest_addr >= BASE
            && len > 0
            && guest_addr.is_multiple_of(BLOCK)
            && len.is_multiple_of(BLOCK)
            && guest_addr - BASE + len <= REGION;
        if !inside {
            self.violation(format!("{what} outside the region: 0x{guest_addr:x}+0x{len:x}"));
            return None;
        }
        let first = ((guest_addr - BASE) / BLOCK) as usize;
        Some(first..first + (len / BLOCK) as usize)
    }

    fn serve(&self, message: WorkerMessage) {
        match message {
            WorkerMessage::RamMap(reply, host_addr, guest_addr, len) => {
                let ok = self.map(host_addr, guest_addr, len);
                let _ = reply.send(ok);
            }
            WorkerMessage::RamUnmap(reply, guest_addr, len) => {
                let ok = self.unmap(guest_addr, len);
                let _ = reply.send(ok);
            }
            _ => self.violation("a message that is not a RAM map or unmap".into()),
        }
    }

    fn map(&self, host_addr: u64, guest_addr: u64, len: u64) -> bool {
        if self.refuse_next_map.swap(false, Ordering::SeqCst) {
            return false;
        }
        let Some(blocks) = self.blocks("map", guest_addr, len) else {
            return false;
        };
        if host_addr != self.host_base as u64 + (guest_addr - BASE) {
            self.violation(format!("map of 0x{guest_addr:x} at a wrong host address"));
        }
        let mut mapped = self.mapped.lock().unwrap();
        for block in blocks {
            if mapped[block] {
                self.violation(format!("block {block} mapped two times"));
            }
            #[cfg(target_os = "windows")]
            if !is_committed(self.host_base + block * BLOCK as usize) {
                self.violation(format!("block {block} mapped before it was committed"));
            }
            mapped[block] = true;
        }
        true
    }

    fn unmap(&self, guest_addr: u64, len: u64) -> bool {
        {
            let mut left = self.unmaps_before_refusal.lock().unwrap();
            if *left == 0 {
                *left = usize::MAX;
                return false;
            }
            if *left != usize::MAX {
                *left -= 1;
            }
        }
        let Some(blocks) = self.blocks("unmap", guest_addr, len) else {
            return false;
        };
        let mut mapped = self.mapped.lock().unwrap();
        for block in blocks {
            if !mapped[block] {
                self.violation(format!("block {block} unmapped but it was not mapped"));
            }
            mapped[block] = false;
        }
        true
    }
}

#[cfg(target_os = "windows")]
fn is_committed(host_addr: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT,
    };
    // SAFETY: an all-zero MEMORY_BASIC_INFORMATION is a valid value.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid out buffer of the size that the call is given.
    let written = unsafe {
        VirtualQuery(
            host_addr as *const _,
            &mut info,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    written != 0 && info.State == MEM_COMMIT
}

/// The request that a step sends, after the model has read the input.
#[derive(Clone, Copy, Debug)]
struct Request {
    kind: u16,
    addr: u64,
    nb_blocks: u16,
    shape: Shape,
}

/// The descriptor chain of a request.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    /// A readable request and a writable response.
    Normal,
    /// No writable descriptor: the device cannot answer.
    NoResponse,
    /// No readable descriptor: the device sees an empty request.
    NoRequest,
    /// The request descriptor points outside guest memory.
    RequestNowhere,
    /// The response descriptor points outside guest memory.
    ResponseNowhere,
}

/// What a run did, so that a test can prove that it reached the states that matter.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub acks: u64,
    pub nacks: u64,
    pub busy: u64,
    pub errors: u64,
    pub blocks_plugged: u64,
    pub blocks_unplugged: u64,
}

/// The device, its guest memory, and the model that the device must agree with.
pub struct Harness {
    device: Mem,
    memory: GuestMemoryMmap,
    domain: MemoryAccessDomain,
    host_base: usize,
    /// The model: one entry for each block, true when plugged.
    plugged: Vec<bool>,
    /// The model: the size that the host asked for, in blocks.
    requested: usize,
    /// The value that a plugged block holds at each tag offset.
    tags: Vec<u64>,
    next_tag: u64,
    steps: u64,
    pub stats: Stats,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    hypervisor: Arc<Hypervisor>,
    /// The mapping that holds the guards and the hotplug region.
    #[cfg(unix)]
    mapping: usize,
}

// SAFETY: the raw addresses are of memory that the harness owns for its whole life.
unsafe impl Send for Harness {}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    pub fn new() -> Self {
        let low = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), LOW)]).unwrap();

        // Unix: one mapping holds a guard, the hotplug region, and a guard, so that a release
        // that reached past the region would change a guard.
        #[cfg(unix)]
        let (mapping, host_base, hotplug) = {
            let flags = libc::MAP_PRIVATE | libc::MAP_ANON;
            let prot = libc::PROT_READ | libc::PROT_WRITE;
            let total = GUARD * 2 + REGION as usize;
            // SAFETY: a new anonymous mapping.
            let mapping = unsafe { libc::mmap(std::ptr::null_mut(), total, prot, flags, -1, 0) };
            assert_ne!(mapping, libc::MAP_FAILED);
            let mapping = mapping as usize;
            let host_base = mapping + GUARD;
            // SAFETY: both guards are inside the new mapping.
            unsafe {
                std::ptr::write_bytes(mapping as *mut u8, GUARD_BYTE, GUARD);
                std::ptr::write_bytes((host_base + REGION as usize) as *mut u8, GUARD_BYTE, GUARD);
            }
            // SAFETY: the range is inside the new mapping and has the given protection and
            // flags. The harness unmaps it only when it is dropped.
            let region = unsafe {
                MmapRegion::build_raw(host_base as *mut u8, REGION as usize, prot, flags)
            }
            .unwrap();
            (mapping, host_base, region)
        };
        // Windows: the region is only reserved, as the VMM builds it.
        #[cfg(target_os = "windows")]
        let (host_base, hotplug) = {
            let region = MmapRegion::new_reserved(REGION as usize).unwrap();
            (region.as_ptr() as usize, region)
        };
        let memory = low
            .insert_region(Arc::new(
                GuestRegionMmap::new(hotplug, GuestAddress(BASE)).unwrap(),
            ))
            .unwrap();

        let mut device = Mem::new().unwrap();
        device.set_region(BASE, REGION).unwrap();

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let hypervisor = {
            let hypervisor = Arc::new(Hypervisor {
                // macOS maps the whole region at boot. Windows maps a block when it is plugged.
                mapped: Mutex::new(vec![cfg!(target_os = "macos"); BLOCKS]),
                violations: Mutex::new(Vec::new()),
                unmaps_before_refusal: Mutex::new(usize::MAX),
                refuse_next_map: AtomicBool::new(false),
                host_base,
            });
            let (sender, receiver) = crossbeam_channel::unbounded();
            device.set_map_sender(sender);
            let worker = hypervisor.clone();
            std::thread::spawn(move || {
                for message in receiver {
                    worker.serve(message);
                }
            });
            hypervisor
        };

        Self {
            device,
            memory,
            domain: MemoryAccessDomain::new(),
            host_base,
            plugged: vec![false; BLOCKS],
            requested: 0,
            tags: vec![0; BLOCKS],
            next_tag: 0x5eed_0000_0000_0001,
            steps: 0,
            stats: Stats::default(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            hypervisor,
            #[cfg(unix)]
            mapping,
        }
    }

    /// The host address and the length of the hotplug region.
    pub fn host_region(&self) -> (usize, usize) {
        (self.host_base, REGION as usize)
    }

    /// The blocks that the model has plugged.
    pub fn plugged(&self) -> &[bool] {
        &self.plugged
    }

    /// Make one step from the input, then check the device against the model.
    pub fn step(&mut self, input: &[u8; STEP_BYTES]) {
        self.steps += 1;
        match input[0] % 16 {
            0 | 1 => self.set_requested(input[1] as usize % (BLOCKS + 1)),
            2 => self.refuse_unmap_after(0),
            3 => self.refuse_next_map(),
            _ => {
                let request = decode_request(input);
                self.request(request);
            }
        }
        self.check();
    }

    /// Host action: ask the guest to converge on a number of plugged blocks.
    pub fn set_requested(&mut self, blocks: usize) {
        self.device.set_requested_size(blocks as u64 * BLOCK);
        self.requested = blocks;
    }

    /// Make the hypervisor refuse one unmap request, after it accepted `accepted` of them.
    pub fn refuse_unmap_after(&mut self, accepted: usize) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            *self.hypervisor.unmaps_before_refusal.lock().unwrap() = accepted;
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let _ = accepted;
    }

    /// Make the hypervisor refuse the next map request. Only Windows maps on a plug; a map
    /// that fails on macOS stops the VM, so the harness does not inject it there.
    pub fn refuse_next_map(&mut self) {
        #[cfg(target_os = "windows")]
        self.hypervisor.refuse_next_map.store(true, Ordering::SeqCst);
    }

    /// Guest request with valid descriptors and any address. Returns the response type.
    pub fn guest_raw(&mut self, kind: u16, addr: u64, nb_blocks: u16) -> u16 {
        let request = Request {
            kind,
            addr,
            nb_blocks,
            shape: Shape::Normal,
        };
        let response = self.request(request);
        self.check();
        response.expect("a normal request gets a response").0
    }

    /// Guest request with valid descriptors for blocks of the region.
    pub fn guest(&mut self, kind: u16, first_block: usize, nb_blocks: u16) -> u16 {
        self.guest_raw(kind, BASE + first_block as u64 * BLOCK, nb_blocks)
    }

    pub fn plug(&mut self, first_block: usize, nb_blocks: u16) -> u16 {
        self.guest(VIRTIO_MEM_REQ_PLUG, first_block, nb_blocks)
    }

    pub fn unplug(&mut self, first_block: usize, nb_blocks: u16) -> u16 {
        self.guest(VIRTIO_MEM_REQ_UNPLUG, first_block, nb_blocks)
    }

    pub fn unplug_all(&mut self) -> u16 {
        self.guest(VIRTIO_MEM_REQ_UNPLUG_ALL, 0, 0)
    }

    /// Bring the device back to its first state, for the next fuzz input.
    pub fn reset(&mut self) {
        self.refuse_unmap_after(usize::MAX);
        #[cfg(target_os = "windows")]
        self.hypervisor.refuse_next_map.store(false, Ordering::SeqCst);
        assert_eq!(self.unplug_all(), VIRTIO_MEM_RESP_ACK);
        self.set_requested(0);
        self.check();
    }

    /// Send a request, apply it to the model, and compare the response. Returns the response
    /// that the device wrote, when it could write one.
    fn request(&mut self, request: Request) -> Option<(u16, u16)> {
        let before = self.plugged.iter().filter(|plugged| **plugged).count() as u64;
        let expected = self.apply_to_model(request);
        let after = self.plugged.iter().filter(|plugged| **plugged).count() as u64;
        self.stats.blocks_plugged += after.saturating_sub(before);
        self.stats.blocks_unplugged += before.saturating_sub(after);
        match expected.0 {
            VIRTIO_MEM_RESP_ACK => self.stats.acks += 1,
            VIRTIO_MEM_RESP_NACK => self.stats.nacks += 1,
            VIRTIO_MEM_RESP_BUSY => self.stats.busy += 1,
            _ => self.stats.errors += 1,
        }
        let response = self.send(request);
        match request.shape {
            Shape::NoResponse | Shape::ResponseNowhere => assert_eq!(response, None),
            _ => assert_eq!(
                response,
                Some(expected),
                "step {}: {request:?} with plugged={:?} requested={}",
                self.steps,
                self.plugged,
                self.requested
            ),
        }
        self.tag_new_blocks();
        response
    }

    /// The model. It is written apart from the device code, with wide arithmetic, so that an
    /// error in the range checks of the device shows as a difference.
    fn apply_to_model(&mut self, request: Request) -> (u16, u16) {
        // A request that the device cannot read is an empty request: a plug of no blocks.
        let (kind, addr, nb_blocks) = match request.shape {
            Shape::NoRequest | Shape::RequestNowhere => (VIRTIO_MEM_REQ_PLUG, 0, 0),
            _ => (request.kind, request.addr, request.nb_blocks),
        };
        let blocks = || -> Option<std::ops::Range<usize>> {
            let (addr, count) = (addr as u128, nb_blocks as u128);
            let (base, block) = (BASE as u128, BLOCK as u128);
            if count == 0 || addr < base || (addr - base) % block != 0 {
                return None;
            }
            let first = (addr - base) / block;
            if first + count > BLOCKS as u128 {
                return None;
            }
            Some(first as usize..(first + count) as usize)
        };
        let plugged_count = self.plugged.iter().filter(|plugged| **plugged).count();

        match kind {
            VIRTIO_MEM_REQ_PLUG => {
                let Some(blocks) = blocks() else {
                    return (VIRTIO_MEM_RESP_ERROR, 0);
                };
                if plugged_count + blocks.len() > self.requested
                    || self.plugged[blocks.clone()].iter().any(|plugged| *plugged)
                {
                    return (VIRTIO_MEM_RESP_NACK, 0);
                }
                if self.take_map_refusal() {
                    return (VIRTIO_MEM_RESP_BUSY, 0);
                }
                self.plugged[blocks].fill(true);
                (VIRTIO_MEM_RESP_ACK, 0)
            }
            VIRTIO_MEM_REQ_UNPLUG => {
                let Some(blocks) = blocks() else {
                    return (VIRTIO_MEM_RESP_ERROR, 0);
                };
                if self.plugged[blocks.clone()].iter().any(|plugged| !*plugged) {
                    return (VIRTIO_MEM_RESP_NACK, 0);
                }
                // Windows keeps the range and answers BUSY when the hypervisor cannot unmap it.
                // macOS zeroes the range in place and still unplugs it.
                if self.unmap_refusal() == Some(0) && cfg!(target_os = "windows") {
                    return (VIRTIO_MEM_RESP_BUSY, 0);
                }
                self.unplug_in_model(blocks);
                (VIRTIO_MEM_RESP_ACK, 0)
            }
            VIRTIO_MEM_REQ_UNPLUG_ALL => {
                let mut runs = Vec::new();
                let mut block = 0;
                while block < BLOCKS {
                    if !self.plugged[block] {
                        block += 1;
                        continue;
                    }
                    let first = block;
                    while block < BLOCKS && self.plugged[block] {
                        block += 1;
                    }
                    runs.push(first..block);
                }
                // Each run is one unmap. On Windows the runs before a refused unmap are
                // unplugged, and the request stops at the refusal.
                let refusal = self.unmap_refusal();
                for (unmaps, run) in runs.into_iter().enumerate() {
                    if refusal == Some(unmaps) && cfg!(target_os = "windows") {
                        return (VIRTIO_MEM_RESP_BUSY, 0);
                    }
                    self.unplug_in_model(run);
                }
                (VIRTIO_MEM_RESP_ACK, 0)
            }
            VIRTIO_MEM_REQ_STATE => {
                let Some(blocks) = blocks() else {
                    return (VIRTIO_MEM_RESP_ERROR, 0);
                };
                let count = self.plugged[blocks.clone()]
                    .iter()
                    .filter(|plugged| **plugged)
                    .count();
                let state = if count == blocks.len() {
                    VIRTIO_MEM_STATE_PLUGGED
                } else if count == 0 {
                    VIRTIO_MEM_STATE_UNPLUGGED
                } else {
                    VIRTIO_MEM_STATE_MIXED
                };
                (VIRTIO_MEM_RESP_ACK, state)
            }
            _ => (VIRTIO_MEM_RESP_ERROR, 0),
        }
    }

    fn unplug_in_model(&mut self, blocks: std::ops::Range<usize>) {
        self.plugged[blocks.clone()].fill(false);
        self.tags[blocks].fill(0);
    }

    /// True when the hypervisor will refuse the map of the next plug.
    fn take_map_refusal(&mut self) -> bool {
        #[cfg(target_os = "windows")]
        return self.hypervisor.refuse_next_map.load(Ordering::SeqCst);
        #[cfg(not(target_os = "windows"))]
        false
    }

    /// The number of unmap requests that the hypervisor accepts before it refuses one, or
    /// `None` when it refuses none. The hypervisor counts down when the device sends a request.
    fn unmap_refusal(&self) -> Option<usize> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let left = *self.hypervisor.unmaps_before_refusal.lock().unwrap();
            return (left != usize::MAX).then_some(left);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        None
    }

    /// Write a request and its descriptor chain into guest memory, and let the device run.
    fn send(&mut self, request: Request) -> Option<(u16, u16)> {
        let memory = &self.memory;
        let write_desc = |index: u64, addr: u64, len: u32, flags: u16, next: u16| {
            let at = DESC_TABLE + index * 16;
            memory.write_obj(addr, GuestAddress(at)).unwrap();
            memory.write_obj(len, GuestAddress(at + 8)).unwrap();
            memory.write_obj(flags, GuestAddress(at + 12)).unwrap();
            memory.write_obj(next, GuestAddress(at + 14)).unwrap();
        };
        let request_addr = match request.shape {
            Shape::RequestNowhere => NOWHERE,
            _ => REQUEST,
        };
        let response_addr = match request.shape {
            Shape::ResponseNowhere => NOWHERE,
            _ => RESPONSE,
        };
        match request.shape {
            Shape::NoResponse => write_desc(0, request_addr, 24, 0, 0),
            Shape::NoRequest => write_desc(0, response_addr, 10, VIRTQ_DESC_F_WRITE, 0),
            _ => {
                write_desc(0, request_addr, 24, VIRTQ_DESC_F_NEXT, 1);
                write_desc(1, response_addr, 10, VIRTQ_DESC_F_WRITE, 0);
            }
        }
        memory
            .write_obj(
                VirtioMemReq {
                    req_type: request.kind,
                    addr: request.addr,
                    nb_blocks: request.nb_blocks,
                    ..Default::default()
                },
                GuestAddress(REQUEST),
            )
            .unwrap();
        // A response that the device does not write stays at this value.
        memory
            .write_obj(
                VirtioMemResp {
                    resp_type: u16::MAX,
                    padding: [0; 3],
                    state: u16::MAX,
                },
                GuestAddress(RESPONSE),
            )
            .unwrap();
        // The available ring: flags, index, and one entry that names descriptor 0.
        memory.write_obj(0u16, GuestAddress(AVAIL_RING)).unwrap();
        memory.write_obj(1u16, GuestAddress(AVAIL_RING + 2)).unwrap();
        memory.write_obj(0u16, GuestAddress(AVAIL_RING + 4)).unwrap();
        memory.write_obj(0u32, GuestAddress(USED_RING)).unwrap();

        let mut queue = Queue::new(8);
        queue.size = 8;
        queue.ready = true;
        queue.desc_table = GuestAddress(DESC_TABLE);
        queue.avail_ring = GuestAddress(AVAIL_RING);
        queue.used_ring = GuestAddress(USED_RING);
        queue.set_memory_access_domain(&self.domain);
        let interrupt =
            InterruptTransport::new(DummyIrqChip::new().into(), "mem-harness".into()).unwrap();
        self.device
            .activate(
                self.memory.clone(),
                interrupt,
                vec![DeviceQueue::new(queue, Arc::new(EventFd::new(0).unwrap()))],
            )
            .unwrap();
        assert!(self.device.process_req_queue());
        self.device.quiesce().unwrap();

        let response: VirtioMemResp = self.memory.read_obj(GuestAddress(RESPONSE)).unwrap();
        let (resp_type, state) = (response.resp_type, response.state);
        (resp_type != u16::MAX).then_some((resp_type, state))
    }

    fn block_addr(&self, block: usize, offset: u64) -> *mut u64 {
        (self.host_base + block * BLOCK as usize + offset as usize) as *mut u64
    }

    /// Give each block that the model plugged in the last request a tag. The block must read
    /// as zero first: memory that the guest plugs again holds no old data.
    fn tag_new_blocks(&mut self) {
        for block in 0..BLOCKS {
            if !self.plugged[block] || self.tags[block] != 0 {
                continue;
            }
            self.next_tag = self.next_tag.wrapping_mul(6364136223846793005).wrapping_add(1) | 1;
            for offset in TAG_OFFSETS {
                // SAFETY: the block is plugged, so its host memory is readable and writable.
                unsafe {
                    let found = self.block_addr(block, offset).read_volatile();
                    assert_eq!(found, 0, "step {}: plugged block {block} is not zero", self.steps);
                    self.block_addr(block, offset).write_volatile(self.next_tag);
                }
            }
            self.tags[block] = self.next_tag;
        }
    }

    /// Compare the device and the host memory with the model.
    pub fn check(&self) {
        let step = self.steps;
        assert_eq!(self.device.plugged_blocks, self.plugged, "step {step}: plugged blocks");
        let plugged_size = self.plugged.iter().filter(|plugged| **plugged).count() as u64 * BLOCK;
        assert_eq!({ self.device.config.plugged_size }, plugged_size, "step {step}: plugged size");
        assert_eq!(
            { self.device.config.requested_size },
            self.requested as u64 * BLOCK,
            "step {step}: requested size"
        );

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let violations = self.hypervisor.violations.lock().unwrap();
            assert!(violations.is_empty(), "step {step}: {violations:?}");
            let mapped = self.hypervisor.mapped.lock().unwrap();
            #[cfg(target_os = "macos")]
            assert!(mapped.iter().all(|mapped| *mapped), "step {step}: an unmapped block");
            #[cfg(target_os = "windows")]
            assert_eq!(*mapped, self.plugged, "step {step}: mapped blocks");
        }

        for block in 0..BLOCKS {
            #[cfg(target_os = "windows")]
            {
                let host = self.host_base + block * BLOCK as usize;
                for offset in [0, BLOCK as usize - 4096] {
                    assert_eq!(
                        is_committed(host + offset),
                        self.plugged[block],
                        "step {step}: commit state of block {block}"
                    );
                }
                if !self.plugged[block] {
                    continue;
                }
            }
            // A plugged block holds its tag. On Unix an unplugged block reads as zero.
            for offset in TAG_OFFSETS {
                // SAFETY: the block is readable: it is plugged, or the host is Unix, where the
                // whole region is mapped.
                let found = unsafe { self.block_addr(block, offset).read_volatile() };
                assert_eq!(
                    found, self.tags[block],
                    "step {step}: block {block} at offset 0x{offset:x} (plugged={})",
                    self.plugged[block]
                );
            }
        }

        #[cfg(unix)]
        if step.is_multiple_of(256) {
            self.check_guards();
        }
    }

    /// The host memory on each side of the hotplug region must not change.
    #[cfg(unix)]
    pub fn check_guards(&self) {
        for start in [self.mapping, self.host_base + REGION as usize] {
            // SAFETY: both guards are inside the mapping of the harness.
            let guard = unsafe { std::slice::from_raw_parts(start as *const u8, GUARD) };
            assert!(
                guard.iter().all(|byte| *byte == GUARD_BYTE),
                "step {}: a guard next to the hotplug region changed",
                self.steps
            );
        }
    }

    /// Checks to make when a run ends.
    pub fn finish(&self) {
        self.check();
        #[cfg(unix)]
        self.check_guards();
    }
}

#[cfg(unix)]
impl Drop for Harness {
    fn drop(&mut self) {
        // SAFETY: the mapping belongs to the harness, and the guest memory that points into it
        // does not unmap it.
        unsafe { libc::munmap(self.mapping as *mut _, GUARD * 2 + REGION as usize) };
    }
}

/// Read a guest request from the input of a step. Most requests are valid or nearly valid, so
/// that a run reaches deep states; the others have a hostile address, count, type, or chain.
fn decode_request(input: &[u8; STEP_BYTES]) -> Request {
    let kind = match input[1] % 16 {
        0..=6 => VIRTIO_MEM_REQ_PLUG,
        7..=12 => VIRTIO_MEM_REQ_UNPLUG,
        13 => VIRTIO_MEM_REQ_STATE,
        14 => VIRTIO_MEM_REQ_UNPLUG_ALL,
        _ => 4 + input[2] as u16,
    };
    let block = input[4] as u64 % BLOCKS as u64;
    let addr = match input[3] % 16 {
        // A block of the region.
        0..=10 => BASE + block * BLOCK,
        // One of the two blocks after the end of the region.
        11 => BASE + REGION + (input[4] as u64 % 2) * BLOCK,
        // Not on a block boundary.
        12 | 13 => BASE + block * BLOCK + 1 + input[5] as u64 * 16,
        // Below the region.
        14 => BASE.wrapping_sub((1 + input[4] as u64) * BLOCK),
        // Any address.
        _ => u64::from_le_bytes([
            input[4], input[5], input[6], input[7], input[8], input[9], input[10], input[11],
        ]),
    };
    let nb_blocks = match input[6] % 16 {
        0..=11 => 1 + (input[7] % 4) as u16,
        12 => 0,
        13 => u16::from_le_bytes([input[7], input[8]]),
        // Near the size of the whole region.
        _ => BLOCKS as u16 - 1 + (input[7] % 3) as u16,
    };
    let shape = match input[9] % 32 {
        0..=27 => Shape::Normal,
        28 => Shape::NoResponse,
        29 => Shape::NoRequest,
        30 => Shape::RequestNowhere,
        _ => Shape::ResponseNowhere,
    };
    Request {
        kind,
        addr,
        nb_blocks,
        shape,
    }
}

/// Run a fuzz input: each 12 bytes are one step. Panics when the device and the model differ.
pub fn fuzz_mem_requests(data: &[u8]) {
    thread_local! {
        static HARNESS: std::cell::RefCell<Harness> = std::cell::RefCell::new(Harness::new());
    }
    HARNESS.with(|harness| {
        let mut harness = harness.borrow_mut();
        for step in data.chunks_exact(STEP_BYTES) {
            harness.step(step.try_into().unwrap());
        }
        harness.finish();
        harness.reset();
    });
}
