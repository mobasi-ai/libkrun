#[cfg(any(target_os = "macos", target_os = "windows"))]
mod backing;
mod device;
mod event_handler;

pub use self::defs::uapi::VIRTIO_ID_MEM as TYPE_MEM;
pub use self::device::{Mem, MemStateSnapshot, VIRTIO_MEM_BLOCK_SIZE};

/// Changes the hypervisor mapping of hot-plugged RAM for the virtio-mem device on macOS and
/// Windows.
///
/// The device calls it on its own thread with the device lock held. An implementation must not
/// take a lock that a thread can hold while that thread waits for the device.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub trait RamMapper: Send {
    /// Map a host range at a guest address with read, write, and execute access. On error no
    /// part of the range is mapped.
    fn map(&self, host_addr: u64, guest_addr: u64, len: u64) -> std::io::Result<()>;

    /// Unmap a guest range, so that the device can release its host memory. On error the
    /// range is still mapped.
    fn unmap(&self, guest_addr: u64, len: u64) -> std::io::Result<()>;
}
#[cfg(any(test, feature = "test_utils"))]
pub use self::device::harness::{fuzz_mem_requests, Harness as MemHarness};

mod defs {
    use super::super::QueueConfig;

    pub const MEM_DEV_ID: &str = "virtio_mem";
    pub const NUM_QUEUES: usize = 1;
    pub const QUEUE_SIZE: u16 = 128;
    pub static QUEUE_CONFIG: [QueueConfig; NUM_QUEUES] = [QueueConfig::new(QUEUE_SIZE); NUM_QUEUES];

    pub mod uapi {
        pub const VIRTIO_F_VERSION_1: u32 = 32;
        pub const VIRTIO_ID_MEM: u32 = 24;
    }
}

#[derive(Debug)]
pub enum MemError {
    /// Failed to create event fd.
    EventFd(std::io::Error),
    /// The hotplug region size is not a multiple of the block size.
    UnalignedRegion,
}

type Result<T> = std::result::Result<T, MemError>;
