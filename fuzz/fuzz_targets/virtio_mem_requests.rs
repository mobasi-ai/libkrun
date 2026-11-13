//! Guest requests to the virtio-mem device. Each 12 bytes of the input are one step of the
//! harness in `devices::virtio::mem`: a host action, or a guest request with a valid or a
//! hostile address, block count, type, or descriptor chain. The harness panics when the device
//! and its model differ, or when a check of the host memory fails.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    devices::virtio::fuzz_mem_requests(data);
});
