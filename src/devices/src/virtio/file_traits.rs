// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fs::File;
use std::io::{Error, ErrorKind, Result};
#[cfg(feature = "blk")]
use std::io::{IoSlice, IoSliceMut};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[cfg(feature = "blk")]
use imago::io_buffers::{IoVector, IoVectorMut};
#[cfg(all(feature = "blk", windows))]
use imago::{FormatReadPlanStep, Mapping, StorageExt};
use vm_memory::VolatileSlice;

#[cfg(unix)]
use libc::{c_int, c_void, read, readv, size_t, write, writev};

#[cfg(unix)]
use super::bindings::{off64_t, pread64, preadv64, pwrite64, pwritev64};
#[cfg(feature = "blk")]
use super::block::device::DiskProperties;

/// A trait for setting the size of a file.
/// This is equivalent to File's `set_len` method, but
/// wrapped in a trait so that it can be implemented for
/// other types.
pub trait FileSetLen {
    // Set the size of this file.
    // This is the moral equivalent of `ftruncate()`.
    fn set_len(&self, _len: u64) -> Result<()>;
}

impl FileSetLen for File {
    fn set_len(&self, len: u64) -> Result<()> {
        File::set_len(self, len)
    }
}

/// A trait similar to `Read` and `Write`, but uses volatile memory as buffers.
pub trait FileReadWriteVolatile {
    /// Read bytes from this file into the given slice, returning the number of bytes read on
    /// success.
    fn read_volatile(&mut self, slice: VolatileSlice) -> Result<usize>;

    /// Like `read_volatile`, except it reads to a slice of buffers. Data is copied to fill each
    /// buffer in order, with the final buffer written to possibly being only partially filled. This
    /// method must behave as a single call to `read_volatile` with the buffers concatenated would.
    /// The default implementation calls `read_volatile` with either the first nonempty buffer
    /// provided, or returns `Ok(0)` if none exists.
    fn read_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
        bufs.iter()
            .find(|b| !b.is_empty())
            .map(|&b| self.read_volatile(b))
            .unwrap_or(Ok(0))
    }

    /// Reads bytes from this into the given slice until all bytes in the slice are written, or an
    /// error is returned.
    fn read_exact_volatile(&mut self, mut slice: VolatileSlice) -> Result<()> {
        while !slice.is_empty() {
            let bytes_read = self.read_volatile(slice)?;
            if bytes_read == 0 {
                return Err(Error::from(ErrorKind::UnexpectedEof));
            }
            // Will panic if read_volatile read more bytes than we gave it, which would be worthy of
            // a panic.
            slice = slice.offset(bytes_read).unwrap();
        }
        Ok(())
    }

    /// Write bytes from the slice to the given file, returning the number of bytes written on
    /// success.
    fn write_volatile(&mut self, slice: VolatileSlice) -> Result<usize>;

    /// Like `write_volatile`, except that it writes from a slice of buffers. Data is copied from
    /// each buffer in order, with the final buffer read from possibly being only partially
    /// consumed. This method must behave as a call to `write_volatile` with the buffers
    /// concatenated would. The default implementation calls `write_volatile` with either the first
    /// nonempty buffer provided, or returns `Ok(0)` if none exists.
    fn write_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
        bufs.iter()
            .find(|b| !b.is_empty())
            .map(|&b| self.write_volatile(b))
            .unwrap_or(Ok(0))
    }

    /// Write bytes from the slice to the given file until all the bytes from the slice have been
    /// written, or an error is returned.
    fn write_all_volatile(&mut self, mut slice: VolatileSlice) -> Result<()> {
        while !slice.is_empty() {
            let bytes_written = self.write_volatile(slice)?;
            if bytes_written == 0 {
                return Err(Error::from(ErrorKind::WriteZero));
            }
            // Will panic if read_volatile read more bytes than we gave it, which would be worthy of
            // a panic.
            slice = slice.offset(bytes_written).unwrap();
        }
        Ok(())
    }
}

impl<T: FileReadWriteVolatile + ?Sized> FileReadWriteVolatile for &mut T {
    fn read_volatile(&mut self, slice: VolatileSlice) -> Result<usize> {
        (**self).read_volatile(slice)
    }

    fn read_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
        (**self).read_vectored_volatile(bufs)
    }

    fn read_exact_volatile(&mut self, slice: VolatileSlice) -> Result<()> {
        (**self).read_exact_volatile(slice)
    }

    fn write_volatile(&mut self, slice: VolatileSlice) -> Result<usize> {
        (**self).write_volatile(slice)
    }

    fn write_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
        (**self).write_vectored_volatile(bufs)
    }

    fn write_all_volatile(&mut self, slice: VolatileSlice) -> Result<()> {
        (**self).write_all_volatile(slice)
    }
}

/// A trait similar to the unix `ReadExt` and `WriteExt` traits, but for volatile memory.
pub trait FileReadWriteAtVolatile {
    /// Reads bytes from this file at `offset` into the given slice, returning the number of bytes
    /// read on success.
    fn read_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize>;

    /// Like `read_at_volatile`, except it reads to a slice of buffers. Data is copied to fill each
    /// buffer in order, with the final buffer written to possibly being only partially filled. This
    /// method must behave as a single call to `read_at_volatile` with the buffers concatenated
    /// would. The default implementation calls `read_at_volatile` with either the first nonempty
    /// buffer provided, or returns `Ok(0)` if none exists.
    fn read_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        if let Some(&slice) = bufs.first() {
            self.read_at_volatile(slice, offset)
        } else {
            Ok(0)
        }
    }

    /// Reads bytes from this file at `offset` into the given slice until all bytes in the slice are
    /// read, or an error is returned.
    fn read_exact_at_volatile(&self, mut slice: VolatileSlice, mut offset: u64) -> Result<()> {
        while !slice.is_empty() {
            match self.read_at_volatile(slice, offset) {
                Ok(0) => return Err(Error::from(ErrorKind::UnexpectedEof)),
                Ok(n) => {
                    slice = slice.offset(n).unwrap();
                    offset = offset.checked_add(n as u64).unwrap();
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Writes bytes from this file at `offset` into the given slice, returning the number of bytes
    /// written on success.
    fn write_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize>;

    /// Like `write_at_at_volatile`, except that it writes from a slice of buffers. Data is copied
    /// from each buffer in order, with the final buffer read from possibly being only partially
    /// consumed. This method must behave as a call to `write_at_volatile` with the buffers
    /// concatenated would. The default implementation calls `write_at_volatile` with either the
    /// first nonempty buffer provided, or returns `Ok(0)` if none exists.
    fn write_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        if let Some(&slice) = bufs.first() {
            self.write_at_volatile(slice, offset)
        } else {
            Ok(0)
        }
    }

    /// Writes bytes from this file at `offset` into the given slice until all bytes in the slice
    /// are written, or an error is returned.
    fn write_all_at_volatile(&self, mut slice: VolatileSlice, mut offset: u64) -> Result<()> {
        while !slice.is_empty() {
            match self.write_at_volatile(slice, offset) {
                Ok(0) => return Err(Error::from(ErrorKind::WriteZero)),
                Ok(n) => {
                    slice = slice.offset(n).unwrap();
                    offset = offset.checked_add(n as u64).unwrap();
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl<T: FileReadWriteAtVolatile + ?Sized> FileReadWriteAtVolatile for &T {
    fn read_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
        (**self).read_at_volatile(slice, offset)
    }

    fn read_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        (**self).read_vectored_at_volatile(bufs, offset)
    }

    fn read_exact_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<()> {
        (**self).read_exact_at_volatile(slice, offset)
    }

    fn write_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
        (**self).write_at_volatile(slice, offset)
    }

    fn write_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        (**self).write_vectored_at_volatile(bufs, offset)
    }

    fn write_all_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<()> {
        (**self).write_all_at_volatile(slice, offset)
    }
}

#[cfg(unix)]
macro_rules! volatile_impl {
    ($ty:ty) => {
        impl FileReadWriteVolatile for $ty {
            fn read_volatile(&mut self, slice: VolatileSlice) -> Result<usize> {
                // Safe because only bytes inside the slice are accessed and the kernel is expected
                // to handle arbitrary memory for I/O.
                let ret = unsafe {
                    read(
                        self.as_raw_fd(),
                        slice.ptr_guard_mut().as_ptr() as *mut c_void,
                        slice.len(),
                    )
                };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn read_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
                let iovecs: Vec<libc::iovec> = bufs
                    .iter()
                    .map(|s| libc::iovec {
                        iov_base: s.ptr_guard_mut().as_ptr() as *mut c_void,
                        iov_len: s.len() as size_t,
                    })
                    .collect();

                if iovecs.is_empty() {
                    return Ok(0);
                }

                // Safe because only bytes inside the buffers are accessed and the kernel is
                // expected to handle arbitrary memory for I/O.
                let ret = unsafe { readv(self.as_raw_fd(), &iovecs[0], iovecs.len() as c_int) };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn write_volatile(&mut self, slice: VolatileSlice) -> Result<usize> {
                // Safe because only bytes inside the slice are accessed and the kernel is expected
                // to handle arbitrary memory for I/O.
                let ret = unsafe {
                    write(
                        self.as_raw_fd(),
                        slice.ptr_guard().as_ptr() as *const c_void,
                        slice.len(),
                    )
                };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn write_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
                let iovecs: Vec<libc::iovec> = bufs
                    .iter()
                    .map(|s| libc::iovec {
                        iov_base: s.ptr_guard_mut().as_ptr() as *mut c_void,
                        iov_len: s.len() as size_t,
                    })
                    .collect();

                if iovecs.is_empty() {
                    return Ok(0);
                }

                // Safe because only bytes inside the buffers are accessed and the kernel is
                // expected to handle arbitrary memory for I/O.
                let ret = unsafe { writev(self.as_raw_fd(), &iovecs[0], iovecs.len() as c_int) };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }
        }

        impl FileReadWriteAtVolatile for $ty {
            fn read_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
                // Safe because only bytes inside the slice are accessed and the kernel is expected
                // to handle arbitrary memory for I/O.
                let ret = unsafe {
                    pread64(
                        self.as_raw_fd(),
                        slice.ptr_guard_mut().as_ptr() as *mut c_void,
                        slice.len(),
                        offset as off64_t,
                    )
                };

                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn read_vectored_at_volatile(
                &self,
                bufs: &[VolatileSlice],
                offset: u64,
            ) -> Result<usize> {
                let iovecs: Vec<libc::iovec> = bufs
                    .iter()
                    .map(|s| libc::iovec {
                        iov_base: s.ptr_guard_mut().as_ptr() as *mut c_void,
                        iov_len: s.len() as size_t,
                    })
                    .collect();

                if iovecs.is_empty() {
                    return Ok(0);
                }

                // Safe because only bytes inside the buffers are accessed and the kernel is
                // expected to handle arbitrary memory for I/O.
                let ret = unsafe {
                    preadv64(
                        self.as_raw_fd(),
                        &iovecs[0],
                        iovecs.len() as c_int,
                        offset as off64_t,
                    )
                };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn write_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
                // Safe because only bytes inside the slice are accessed and the kernel is expected
                // to handle arbitrary memory for I/O.
                let ret = unsafe {
                    pwrite64(
                        self.as_raw_fd(),
                        slice.ptr_guard().as_ptr() as *const c_void,
                        slice.len(),
                        offset as off64_t,
                    )
                };

                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn write_vectored_at_volatile(
                &self,
                bufs: &[VolatileSlice],
                offset: u64,
            ) -> Result<usize> {
                let iovecs: Vec<libc::iovec> = bufs
                    .iter()
                    .map(|s| libc::iovec {
                        iov_base: s.ptr_guard_mut().as_ptr() as *mut c_void,
                        iov_len: s.len() as size_t,
                    })
                    .collect();

                if iovecs.is_empty() {
                    return Ok(0);
                }

                // Safe because only bytes inside the buffers are accessed and the kernel is
                // expected to handle arbitrary memory for I/O.
                let ret = unsafe {
                    pwritev64(
                        self.as_raw_fd(),
                        &iovecs[0],
                        iovecs.len() as c_int,
                        offset as off64_t,
                    )
                };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }
        }
    };
}

#[cfg(unix)]
volatile_impl!(File);

#[cfg(windows)]
mod windows_file_io {
    use std::cell::RefCell;
    use std::fs::File;
    use std::io::{Error, ErrorKind, Result};
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{copy_nonoverlapping, null};

    use vm_memory::VolatileSlice;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_HANDLE_EOF, ERROR_IO_PENDING, HANDLE,
    };
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent};

    use super::FileReadWriteAtVolatile;

    const MAX_IO_BYTES: usize = u32::MAX as usize;
    const MAX_RETAINED_STAGING_BYTES: usize = 1 << 20;

    /// Thread-local manual-reset Win32 event so even a `FILE_FLAG_OVERLAPPED` handle
    /// completes synchronously before stack `OVERLAPPED` and buffer guards are dropped.
    struct ThreadEvent(HANDLE);

    impl ThreadEvent {
        fn new() -> Result<Self> {
            // SAFETY: creates a private, unnamed, manual-reset, non-signaled Win32 event.
            let handle = unsafe { CreateEventW(null(), 1, 0, null()) };
            if handle.is_null() {
                Err(Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }

        fn reset_raw(&self) -> Result<HANDLE> {
            // SAFETY: `self.0` is a valid event handle owned by this thread-local.
            if unsafe { ResetEvent(self.0) } == 0 {
                return Err(Error::last_os_error());
            }
            Ok(self.0)
        }
    }

    impl Drop for ThreadEvent {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is owned exclusively by this `ThreadEvent`.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    thread_local! {
        static STAGING: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
        static IO_EVENT: RefCell<Option<ThreadEvent>> = const { RefCell::new(None) };
    }

    fn thread_io_event() -> Result<HANDLE> {
        IO_EVENT.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                *slot = Some(ThreadEvent::new()?);
            }
            slot.as_ref().expect("initialized thread event").reset_raw()
        })
    }

    fn validate_offset(offset: u64, len: usize) -> Result<()> {
        // Reject offsets above `i64::MAX` (matching Linux pread/pwrite `EINVAL`) and
        // specifically excluding Windows `OVERLAPPED` sentinels `0xFFFF_FFFF_FFFF_FFFF`
        // (append) and `0xFFFF_FFFF_FFFF_FFFE` (use current file pointer).
        // Use `ErrorKind::InvalidInput` (not raw POSIX `EINVAL = 22`, which Windows
        // interprets as `ERROR_BAD_COMMAND`) so host callers map it cleanly to `EINVAL`.
        let max_pos = i64::MAX as u64;
        if offset > max_pos
            || offset
                .checked_add(len as u64)
                .is_none_or(|end| end > max_pos)
        {
            return Err(Error::new(ErrorKind::InvalidInput, "file offset out of range"));
        }
        Ok(())
    }

    fn overlapped_at(offset: u64, event: HANDLE) -> OVERLAPPED {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.Anonymous.Anonymous.Offset = offset as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        overlapped.hEvent = event;
        overlapped
    }

    fn finish_overlapped(
        file: &File,
        overlapped: &OVERLAPPED,
        treat_eof_as_zero: bool,
    ) -> Result<usize> {
        let err = Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
            let mut transferred: u32 = 0;
            // SAFETY: `overlapped` and the I/O buffer remain live until `GetOverlappedResult`
            // with `bWait = 1` returns, guaranteeing Windows no longer references them.
            let done = unsafe {
                GetOverlappedResult(file.as_raw_handle(), overlapped, &mut transferred, 1)
            };
            if done != 0 {
                return Ok(transferred as usize);
            }
            let wait_err = Error::last_os_error();
            if treat_eof_as_zero && wait_err.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
                return Ok(0);
            }
            return Err(wait_err);
        }
        if treat_eof_as_zero && err.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
            Ok(0)
        } else {
            Err(err)
        }
    }

    fn read_raw_at(file: &File, ptr: *mut u8, len: usize, offset: u64) -> Result<usize> {
        let to_read = len.min(MAX_IO_BYTES);
        validate_offset(offset, to_read)?;
        if to_read == 0 {
            return Ok(0);
        }
        let event = thread_io_event()?;
        let mut read_bytes: u32 = 0;
        let mut overlapped = overlapped_at(offset, event);
        // SAFETY: `ptr` is valid for `to_read` writable bytes and remains alive until
        // `ReadFile` (and `GetOverlappedResult` if pending) completes.
        let ok = unsafe {
            ReadFile(
                file.as_raw_handle(),
                ptr,
                to_read as u32,
                &mut read_bytes,
                &mut overlapped,
            )
        };
        if ok != 0 {
            Ok(read_bytes as usize)
        } else {
            finish_overlapped(file, &overlapped, true)
        }
    }

    fn write_raw_at(file: &File, ptr: *const u8, len: usize, offset: u64) -> Result<usize> {
        let to_write = len.min(MAX_IO_BYTES);
        validate_offset(offset, to_write)?;
        if to_write == 0 {
            return Ok(0);
        }
        let event = thread_io_event()?;
        let mut written_bytes: u32 = 0;
        let mut overlapped = overlapped_at(offset, event);
        // SAFETY: `ptr` is valid for `to_write` readable bytes and remains alive until
        // `WriteFile` (and `GetOverlappedResult` if pending) completes.
        let ok = unsafe {
            WriteFile(
                file.as_raw_handle(),
                ptr,
                to_write as u32,
                &mut written_bytes,
                &mut overlapped,
            )
        };
        if ok != 0 {
            Ok(written_bytes as usize)
        } else {
            finish_overlapped(file, &overlapped, false)
        }
    }

    fn total_len(bufs: &[VolatileSlice]) -> Result<usize> {
        bufs.iter().try_fold(0usize, |acc, s| {
            acc.checked_add(s.len())
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "slice length overflow"))
        })
    }

    fn single_non_empty<'a>(bufs: &'a [VolatileSlice<'a>]) -> Option<VolatileSlice<'a>> {
        let mut found = None;
        for slice in bufs {
            if slice.is_empty() {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(*slice);
        }
        found
    }

    fn trim_staging(staging: &mut Vec<u8>) {
        if staging.capacity() > MAX_RETAINED_STAGING_BYTES {
            staging.truncate(MAX_RETAINED_STAGING_BYTES);
            staging.shrink_to_fit();
        }
    }

    impl FileReadWriteAtVolatile for File {
        fn read_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
            validate_offset(offset, slice.len())?;
            if slice.is_empty() {
                return Ok(0);
            }
            let guard = slice.ptr_guard_mut();
            read_raw_at(self, guard.as_ptr(), guard.len(), offset)
        }

        fn read_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
            let total = total_len(bufs)?;
            validate_offset(offset, total.min(MAX_IO_BYTES))?;
            if total == 0 {
                return Ok(0);
            }
            // Fast zero-copy path when a single non-empty slice holds the whole request;
            // never coalesce across independently backed slices by raw pointer adjacency.
            if let Some(only) = single_non_empty(bufs) {
                let guard = only.ptr_guard_mut();
                return read_raw_at(self, guard.as_ptr(), guard.len(), offset);
            }

            let target_len = total.min(MAX_IO_BYTES);
            STAGING.with(|cell| {
                let mut staging = cell.borrow_mut();
                if staging.len() < target_len {
                    staging.resize(target_len, 0);
                }
                let result = read_raw_at(self, staging.as_mut_ptr(), target_len, offset).map(|n| {
                    let mut copied = 0usize;
                    for slice in bufs {
                        let remaining = n - copied;
                        if remaining == 0 {
                            break;
                        }
                        let take = remaining.min(slice.len());
                        if take > 0 {
                            let guard = slice.ptr_guard_mut();
                            // SAFETY: `staging` holds at least `n` initialized bytes and `guard`
                            // is valid for `slice.len() >= take` writable bytes.
                            unsafe {
                                copy_nonoverlapping(
                                    staging.as_ptr().add(copied),
                                    guard.as_ptr(),
                                    take,
                                );
                            }
                            copied += take;
                        }
                    }
                    copied
                });
                trim_staging(&mut staging);
                result
            })
        }

        fn write_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
            validate_offset(offset, slice.len())?;
            if slice.is_empty() {
                return Ok(0);
            }
            let guard = slice.ptr_guard();
            write_raw_at(self, guard.as_ptr(), guard.len(), offset)
        }

        fn write_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
            let total = total_len(bufs)?;
            validate_offset(offset, total.min(MAX_IO_BYTES))?;
            if total == 0 {
                return Ok(0);
            }
            // Fast zero-copy path when a single non-empty slice holds the whole request.
            if let Some(only) = single_non_empty(bufs) {
                let guard = only.ptr_guard();
                return write_raw_at(self, guard.as_ptr(), guard.len(), offset);
            }

            let target_len = total.min(MAX_IO_BYTES);
            STAGING.with(|cell| {
                let mut staging = cell.borrow_mut();
                if staging.len() < target_len {
                    staging.resize(target_len, 0);
                }
                let mut copied = 0usize;
                for slice in bufs {
                    let remaining = target_len - copied;
                    if remaining == 0 {
                        break;
                    }
                    let take = remaining.min(slice.len());
                    if take > 0 {
                        let guard = slice.ptr_guard();
                        // SAFETY: `guard` is valid for `slice.len() >= take` readable bytes and
                        // `staging` has capacity for `target_len >= copied + take` bytes.
                        unsafe {
                            copy_nonoverlapping(
                                guard.as_ptr(),
                                staging.as_mut_ptr().add(copied),
                                take,
                            );
                        }
                        copied += take;
                    }
                }
                let result = write_raw_at(self, staging.as_ptr(), copied, offset);
                trim_staging(&mut staging);
                result
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use std::fs::OpenOptions;
        use std::io::{Read, Write};
        use std::os::windows::fs::OpenOptionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

        use super::*;

        fn temp_path(name: &str) -> std::path::PathBuf {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            std::env::temp_dir().join(format!(
                "libkrun-win-pos-io-{name}-{}-{ts}.bin",
                std::process::id()
            ))
        }

        #[test]
        fn positional_read_write_scattered_slices_and_eof() {
            let path = temp_path("scattered");
            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();

            // 128 KiB payload split across 32 non-contiguous 4 KiB guest pages (gaps between pages).
            let pages = 32usize;
            let page_size = 4096usize;
            let stride = 8192usize;
            let mem: GuestMemoryMmap<()> =
                GuestMemoryMmap::from_ranges(&[(GuestAddress(0), pages * stride * 2)]).unwrap();

            let mut expected = vec![0u8; pages * page_size];
            for (i, b) in expected.iter_mut().enumerate() {
                *b = ((i * 37 + 11) & 0xFF) as u8;
            }
            let mut src_slices = Vec::with_capacity(pages);
            for p in 0..pages {
                let addr = GuestAddress((p * stride) as u64);
                mem.write_slice(&expected[p * page_size..(p + 1) * page_size], addr)
                    .unwrap();
                src_slices.push(mem.get_slice(addr, page_size).unwrap());
            }

            // Positional write at non-zero offset (12345).
            let off = 12_345u64;
            let written = file.write_vectored_at_volatile(&src_slices, off).unwrap();
            assert_eq!(written, expected.len());

            // Read back into scattered destination pages.
            let dst_base = pages * stride;
            let mut dst_slices = Vec::with_capacity(pages);
            for p in 0..pages {
                let addr = GuestAddress((dst_base + p * stride) as u64);
                dst_slices.push(mem.get_slice(addr, page_size).unwrap());
            }
            let read = file.read_vectored_at_volatile(&dst_slices, off).unwrap();
            assert_eq!(read, expected.len());

            let mut actual = vec![0u8; expected.len()];
            for p in 0..pages {
                let addr = GuestAddress((dst_base + p * stride) as u64);
                mem.read_slice(&mut actual[p * page_size..(p + 1) * page_size], addr)
                    .unwrap();
            }
            assert_eq!(actual, expected);

            // EOF at and beyond end-of-file returns Ok(0), not an error.
            let eof_slice = mem.get_slice(GuestAddress(0), 64).unwrap();
            assert_eq!(
                file.read_at_volatile(eof_slice, off + expected.len() as u64)
                    .unwrap(),
                0
            );
            assert_eq!(
                file.read_at_volatile(eof_slice, off + expected.len() as u64 + 4096)
                    .unwrap(),
                0
            );

            // Partial read near EOF.
            let partial = file
                .read_at_volatile(eof_slice, off + expected.len() as u64 - 17)
                .unwrap();
            assert_eq!(partial, 17);

            drop(file);
            std::fs::remove_file(path).unwrap();
        }

        #[test]
        fn positional_write_on_append_handle_preserves_prefix() {
            let path = temp_path("append");
            {
                let mut f = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                f.write_all(b"hello ").unwrap();
            }
            let file = OpenOptions::new().append(true).open(&path).unwrap();
            let mem: GuestMemoryMmap<()> =
                GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
            mem.write_slice(b"world", GuestAddress(0)).unwrap();
            let slice = mem.get_slice(GuestAddress(0), 5).unwrap();
            // Supply offset 0 (inside existing prefix): append handle must append, not overwrite.
            let written = file.write_vectored_at_volatile(&[slice], 0).unwrap();
            assert_eq!(written, 5);
            drop(file);

            let mut contents = String::new();
            std::fs::File::open(&path)
                .unwrap()
                .read_to_string(&mut contents)
                .unwrap();
            assert_eq!(contents, "hello world");
            std::fs::remove_file(path).unwrap();
        }

        #[test]
        fn rejects_out_of_range_and_windows_sentinel_offsets() {
            let path = temp_path("offsets");
            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let mem: GuestMemoryMmap<()> =
                GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
            mem.write_slice(b"abcd", GuestAddress(0)).unwrap();
            let slice = mem.get_slice(GuestAddress(0), 4).unwrap();

            for bad_offset in [u64::MAX, u64::MAX - 1, (i64::MAX as u64) + 1, (i64::MAX as u64) - 2] {
                let err = file.write_at_volatile(slice, bad_offset).unwrap_err();
                assert_eq!(err.kind(), ErrorKind::InvalidInput);
                assert_eq!(err.raw_os_error(), None);
                let err = file.read_at_volatile(slice, bad_offset).unwrap_err();
                assert_eq!(err.kind(), ErrorKind::InvalidInput);
                assert_eq!(err.raw_os_error(), None);
            }
            assert_eq!(file.metadata().unwrap().len(), 0);

            drop(file);
            std::fs::remove_file(path).unwrap();
        }

        #[test]
        fn async_overlapped_handle_and_high_offset_word_work() {
            let path = temp_path("overlapped");
            let file = std::sync::Arc::new(
                OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .read(true)
                    .write(true)
                    .custom_flags(FILE_FLAG_OVERLAPPED)
                    .open(&path)
                    .unwrap(),
            );
            let mem: GuestMemoryMmap<()> =
                GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 8192)]).unwrap();
            mem.write_slice(b"overlapped-payload", GuestAddress(0))
                .unwrap();

            let src = mem.get_slice(GuestAddress(0), 18).unwrap();
            assert_eq!(file.write_at_volatile(src, 128).unwrap(), 18);

            let dst = mem.get_slice(GuestAddress(4096), 18).unwrap();
            assert_eq!(file.read_at_volatile(dst, 128).unwrap(), 18);
            let mut buf = [0u8; 18];
            mem.read_slice(&mut buf, GuestAddress(4096)).unwrap();
            assert_eq!(&buf, b"overlapped-payload");

            // Reading past EOF on an async handle must also return Ok(0).
            assert_eq!(file.read_at_volatile(dst, (1u64 << 32) + 64).unwrap(), 0);

            // Concurrent overlapped I/O from distinct threads uses independent per-thread events.
            let mut handles = Vec::new();
            for tid in 0..4u64 {
                let shared = std::sync::Arc::clone(&file);
                handles.push(std::thread::spawn(move || {
                    let tmem: GuestMemoryMmap<()> =
                        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 8192)]).unwrap();
                    let pattern = [(tid as u8) + 0x41; 64];
                    tmem.write_slice(&pattern, GuestAddress(0)).unwrap();
                    let wslice = tmem.get_slice(GuestAddress(0), 64).unwrap();
                    let rslice = tmem.get_slice(GuestAddress(4096), 64).unwrap();
                    let base = 4096 + tid * 256;
                    for _ in 0..32 {
                        assert_eq!(shared.write_at_volatile(wslice, base).unwrap(), 64);
                        assert_eq!(shared.read_at_volatile(rslice, base).unwrap(), 64);
                        let mut got = [0u8; 64];
                        tmem.read_slice(&mut got, GuestAddress(4096)).unwrap();
                        assert_eq!(got, pattern);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }

            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }
}

#[cfg(feature = "blk")]
impl FileReadWriteAtVolatile for DiskProperties {
    fn read_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
        self.read_vectored_at_volatile(&[slice], offset)
    }

    fn read_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        if bufs.is_empty() {
            return Ok(0);
        }

        #[cfg(windows)]
        if let Some(result) = self.windows_raw_read_vectored_at_volatile(bufs, offset) {
            return result;
        }

        #[cfg(windows)]
        if let Some(result) = self.windows_formatted_read_vectored_at_volatile(bufs, offset)? {
            return Ok(result);
        }

        let ptr_guards = bufs
            .iter()
            .map(|slice| slice.ptr_guard_mut())
            .collect::<Vec<_>>();
        let buffers = ptr_guards
            .iter()
            .map(|guard| {
                let slice = if guard.len() == 0 {
                    &mut []
                } else {
                    unsafe { std::slice::from_raw_parts_mut(guard.as_ptr(), guard.len()) }
                };
                IoSliceMut::new(slice)
            })
            .collect::<Vec<_>>();
        let iovec = IoVectorMut::from(buffers);
        let full_length = iovec
            .len()
            .try_into()
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
        self.file.lock().unwrap().readv(iovec, offset)?;
        Ok(full_length)
    }

    fn write_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
        self.write_vectored_at_volatile(&[slice], offset)
    }

    fn write_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        if bufs.is_empty() {
            return Ok(0);
        }

        #[cfg(windows)]
        if let Some(result) = self.windows_raw_write_vectored_at_volatile(bufs, offset) {
            return result;
        }

        #[cfg(windows)]
        if let Some(result) = self.windows_formatted_write_vectored_at_volatile(bufs, offset)? {
            return Ok(result);
        }

        let ptr_guards = bufs
            .iter()
            .map(|slice| slice.ptr_guard())
            .collect::<Vec<_>>();
        let buffers = ptr_guards
            .iter()
            .map(|guard| {
                let slice = if guard.len() == 0 {
                    &[]
                } else {
                    unsafe { std::slice::from_raw_parts(guard.as_ptr(), guard.len()) }
                };
                IoSlice::new(slice)
            })
            .collect::<Vec<_>>();
        let mut iovec = IoVector::from(buffers);
        let full_length_u64 = iovec.len();
        let full_length = full_length_u64
            .try_into()
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;

        // Keep the invariant local to the backing-write boundary as well as the virtio request
        // parser: a bounded mutation is accepted in full or rejected before its first writev.
        self.validate_mutation_range(offset, full_length_u64)?;

        let mut chunk_offset = offset;
        while !iovec.is_empty() {
            let plan = self.plan_buffered_mutation(chunk_offset, iovec.len())?;
            let chunk_length = plan.len();
            let (chunk, remainder) = iovec.split_at(chunk_length);

            let operation_result = self.file.lock().unwrap().writev(chunk, chunk_offset);
            self.finish_buffered_mutation(plan, operation_result)?;

            iovec = remainder;
            if !iovec.is_empty() {
                chunk_offset = chunk_offset.checked_add(chunk_length).ok_or_else(|| {
                    Error::new(ErrorKind::InvalidInput, "block write range overflow")
                })?;
            }
        }
        Ok(full_length)
    }
}

#[cfg(all(feature = "blk", windows))]
impl DiskProperties {
    fn windows_formatted_read_vectored_at_volatile(
        &self,
        bufs: &[VolatileSlice],
        offset: u64,
    ) -> Result<Option<usize>> {
        let full_length = volatile_slices_len(bufs)?;
        if full_length == 0 {
            return Ok(Some(0));
        }

        let file = self.file.lock().unwrap();
        let plan = file.plan_read(offset, full_length)?;
        if plan
            .steps()
            .iter()
            .any(|step| !planned_read_step_supported(step))
        {
            return Ok(None);
        }

        let ptr_guards = bufs
            .iter()
            .map(|slice| slice.ptr_guard_mut())
            .collect::<Vec<_>>();
        let buffers = ptr_guards
            .iter()
            .map(|guard| {
                let slice = if guard.len() == 0 {
                    &mut []
                } else {
                    unsafe { std::slice::from_raw_parts_mut(guard.as_ptr(), guard.len()) }
                };
                IoSliceMut::new(slice)
            })
            .collect::<Vec<_>>();
        let mut remaining = IoVectorMut::from(buffers);

        for step in plan.steps() {
            let Some(len) = planned_read_step_len(step) else {
                return Ok(None);
            };
            let (mut chunk, rest) = remaining.split_at(len);
            remaining = rest;

            match step {
                FormatReadPlanStep::Raw {
                    storage,
                    offset: storage_offset,
                    ..
                } => self
                    .windows_formatted_io_runtime
                    .block_on(storage.readv(chunk, *storage_offset))?,
                FormatReadPlanStep::Zero { .. } | FormatReadPlanStep::Eof { .. } => {
                    chunk.fill(0);
                }
                _ => return Ok(None),
            }
        }

        Ok(Some(
            full_length
                .try_into()
                .map_err(|e| Error::new(ErrorKind::InvalidData, e))?,
        ))
    }

    fn windows_formatted_write_vectored_at_volatile(
        &self,
        bufs: &[VolatileSlice],
        offset: u64,
    ) -> Result<Option<usize>> {
        let full_length = volatile_slices_len(bufs)?;
        if full_length == 0 {
            return Ok(Some(0));
        }

        let file = self.file.lock().unwrap();
        if offset >= file.size() || full_length > file.size() - offset {
            return Ok(None);
        }

        let mut steps = Vec::new();
        let mut image_offset = offset;
        let mut remaining_len = full_length;
        while remaining_len > 0 {
            let (mapping, mapped_len) = file.get_mapping_sync(image_offset, remaining_len)?;
            let step_len = std::cmp::min(mapped_len, remaining_len);
            if step_len == 0 {
                return Ok(None);
            }

            match mapping {
                Mapping::Raw {
                    storage,
                    offset: storage_offset,
                    writable: true,
                    ..
                } => steps.push((storage, storage_offset, step_len)),
                _ => return Ok(None),
            }

            image_offset = image_offset
                .checked_add(step_len)
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "write mapping overflow"))?;
            remaining_len -= step_len;
        }

        let ptr_guards = bufs
            .iter()
            .map(|slice| slice.ptr_guard())
            .collect::<Vec<_>>();
        let buffers = ptr_guards
            .iter()
            .map(|guard| {
                let slice = if guard.len() == 0 {
                    &[]
                } else {
                    unsafe { std::slice::from_raw_parts(guard.as_ptr(), guard.len()) }
                };
                IoSlice::new(slice)
            })
            .collect::<Vec<_>>();
        let mut remaining = IoVector::from(buffers);

        for (storage, storage_offset, len) in steps {
            let (chunk, rest) = remaining.split_at(len);
            remaining = rest;
            self.windows_formatted_io_runtime
                .block_on(storage.writev(chunk, storage_offset))?;
        }

        Ok(Some(
            full_length
                .try_into()
                .map_err(|e| Error::new(ErrorKind::InvalidData, e))?,
        ))
    }
}

#[cfg(all(feature = "blk", windows))]
fn volatile_slices_len(bufs: &[VolatileSlice]) -> Result<u64> {
    bufs.iter().try_fold(0u64, |total, slice| {
        total
            .checked_add(slice.len() as u64)
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "volatile slice length overflow"))
    })
}

#[cfg(all(feature = "blk", windows))]
fn planned_read_step_supported(step: &FormatReadPlanStep<'_, Box<dyn imago::DynStorage>>) -> bool {
    matches!(
        step,
        FormatReadPlanStep::Raw { .. }
            | FormatReadPlanStep::Zero { .. }
            | FormatReadPlanStep::Eof { .. }
    )
}

#[cfg(all(feature = "blk", windows))]
fn planned_read_step_len(step: &FormatReadPlanStep<'_, Box<dyn imago::DynStorage>>) -> Option<u64> {
    match step {
        FormatReadPlanStep::Raw { len, .. }
        | FormatReadPlanStep::Zero { len, .. }
        | FormatReadPlanStep::Eof { len, .. } => Some(*len),
        _ => None,
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(all(test, feature = "blk", windows))]
mod tests {
    use std::io::Write;
    use std::os::windows::fs::FileExt;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use imago::format::PreallocateMode;
    use imago::qcow2::Qcow2;
    use imago::vmdk::Vmdk;
    use imago::{
        raw::Raw, DenyImplicitOpenGate, DynStorage, FormatCreateBuilder, FormatDriverBuilder,
        PermissiveImplicitOpenGate, Storage, StorageCreateOptions, SyncFormatAccess,
    };
    use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

    use crate::virtio::block::device::CacheType;

    use super::*;

    #[test]
    fn windows_formatted_read_plan_handles_raw_and_eof_ranges() {
        let path = temp_image_path("planned-raw-eof");
        let payload = (0..512).map(|byte| (byte % 251) as u8).collect::<Vec<_>>();

        let mut file = File::create(&path).unwrap();
        file.write_all(&payload).unwrap();
        file.set_len(payload.len() as u64).unwrap();
        drop(file);

        let raw = Raw::<Box<dyn DynStorage>>::open_path_sync(&path, true).unwrap();
        let disk_image = Arc::new(Mutex::new(SyncFormatAccess::new(raw).unwrap()));
        let disk = DiskProperties::new(disk_image, Vec::new(), CacheType::Unsafe, false).unwrap();

        let mem: GuestMemoryMmap<()> =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        let target = mem.get_slice(GuestAddress(0x100), 1024).unwrap();

        assert_eq!(
            disk.windows_formatted_read_vectored_at_volatile(&[target], 0)
                .unwrap(),
            Some(1024)
        );

        let mut actual = vec![0xff; 1024];
        mem.read_slice(&mut actual, GuestAddress(0x100)).unwrap();
        assert_eq!(&actual[..payload.len()], &payload);
        assert_eq!(&actual[payload.len()..], vec![0; 512]);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn windows_formatted_write_uses_writable_raw_mapping() {
        let path = temp_image_path("planned-write");
        let file = File::create(&path).unwrap();
        file.set_len(1024).unwrap();
        drop(file);

        let raw = Raw::<Box<dyn DynStorage>>::open_path_sync(&path, true).unwrap();
        let disk_image = Arc::new(Mutex::new(SyncFormatAccess::new(raw).unwrap()));
        let disk = DiskProperties::new(disk_image, Vec::new(), CacheType::Unsafe, false).unwrap();

        let mem: GuestMemoryMmap<()> =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        let payload = (0..512).map(|byte| (byte % 193) as u8).collect::<Vec<_>>();
        mem.write_slice(&payload[..256], GuestAddress(0x100))
            .unwrap();
        mem.write_slice(&payload[256..], GuestAddress(0x400))
            .unwrap();

        let first = mem.get_slice(GuestAddress(0x100), 256).unwrap();
        let second = mem.get_slice(GuestAddress(0x400), 256).unwrap();
        assert_eq!(
            disk.windows_formatted_write_vectored_at_volatile(&[first, second], 128)
                .unwrap(),
            Some(payload.len())
        );

        let mut actual = vec![0u8; payload.len()];
        File::open(&path)
            .unwrap()
            .seek_read(&mut actual, 128)
            .unwrap();
        assert_eq!(actual, payload);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn windows_qcow2_write_falls_back_for_unallocated_range() {
        let path = temp_image_path("qcow2-write-fallback");
        let disk = create_qcow2_disk(&path, PreallocateMode::None);
        let mem: GuestMemoryMmap<()> =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        let payload = vec![0x6du8; 512];
        mem.write_slice(&payload, GuestAddress(0x100)).unwrap();

        let source = mem.get_slice(GuestAddress(0x100), payload.len()).unwrap();
        assert_eq!(
            disk.windows_formatted_write_vectored_at_volatile(&[source], 0)
                .unwrap(),
            None
        );
        assert_eq!(disk.write_vectored_at_volatile(&[source], 0).unwrap(), 512);

        let target = mem.get_slice(GuestAddress(0x400), payload.len()).unwrap();
        assert_eq!(disk.read_vectored_at_volatile(&[target], 0).unwrap(), 512);

        let mut actual = vec![0u8; payload.len()];
        mem.read_slice(&mut actual, GuestAddress(0x400)).unwrap();
        assert_eq!(actual, payload);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn windows_qcow2_write_uses_allocated_writable_raw_mapping() {
        let path = temp_image_path("qcow2-write-direct");
        let disk = create_qcow2_disk(&path, PreallocateMode::None);
        let mem: GuestMemoryMmap<()> =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();

        let first = vec![0x2au8; 512];
        mem.write_slice(&first, GuestAddress(0x100)).unwrap();
        let first_source = mem.get_slice(GuestAddress(0x100), first.len()).unwrap();
        assert_eq!(
            disk.write_vectored_at_volatile(&[first_source], 0).unwrap(),
            512
        );

        let replacement = vec![0x8eu8; 512];
        mem.write_slice(&replacement, GuestAddress(0x500)).unwrap();
        let replacement_source = mem
            .get_slice(GuestAddress(0x500), replacement.len())
            .unwrap();
        assert_eq!(
            disk.windows_formatted_write_vectored_at_volatile(&[replacement_source], 0)
                .unwrap(),
            Some(512)
        );

        let target = mem
            .get_slice(GuestAddress(0x900), replacement.len())
            .unwrap();
        assert_eq!(disk.read_vectored_at_volatile(&[target], 0).unwrap(), 512);

        let mut actual = vec![0u8; replacement.len()];
        mem.read_slice(&mut actual, GuestAddress(0x900)).unwrap();
        assert_eq!(actual, replacement);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn windows_vmdk_read_uses_flat_raw_mapping() {
        let descriptor_path = temp_image_path("vmdk-read");
        let flat_path = descriptor_path.with_extension("flat");
        let flat_name = flat_path.file_name().unwrap().to_string_lossy();
        let payload = (0..1024)
            .map(|byte| ((byte * 7) % 251) as u8)
            .collect::<Vec<_>>();

        let mut flat_file = File::create(&flat_path).unwrap();
        flat_file.write_all(&payload).unwrap();
        flat_file.set_len(payload.len() as u64).unwrap();
        drop(flat_file);

        let descriptor = format!(
            "# Disk DescriptorFile\n\
             version=1\n\
             CID=fffffffe\n\
             parentCID=ffffffff\n\
             createType=\"monolithicFlat\"\n\
             RW 2 FLAT \"{flat_name}\" 0\n\
             ddb.geometry.cylinders = \"1\"\n\
             ddb.geometry.heads = \"1\"\n\
             ddb.geometry.sectors = \"2\"\n"
        );
        let mut descriptor_file = File::create(&descriptor_path).unwrap();
        descriptor_file.write_all(descriptor.as_bytes()).unwrap();
        drop(descriptor_file);

        let disk = create_vmdk_disk(&descriptor_path);
        let mem: GuestMemoryMmap<()> =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        let target = mem.get_slice(GuestAddress(0x180), 512).unwrap();

        assert_eq!(
            disk.windows_formatted_read_vectored_at_volatile(&[target], 128)
                .unwrap(),
            Some(512)
        );

        let mut actual = vec![0u8; 512];
        mem.read_slice(&mut actual, GuestAddress(0x180)).unwrap();
        assert_eq!(actual, payload[128..640]);

        std::fs::remove_file(descriptor_path).unwrap();
        std::fs::remove_file(flat_path).unwrap();
    }

    fn create_qcow2_disk(path: &std::path::Path, preallocate: PreallocateMode) -> DiskProperties {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let qcow2 = runtime
            .block_on(async {
                let storage = Box::<dyn DynStorage>::create_open(
                    StorageCreateOptions::new().filename(path).overwrite(true),
                )
                .await?;
                Qcow2::<Box<dyn DynStorage>, Arc<imago::FormatAccess<_>>>::create_builder(storage)
                    .size(4096)
                    .cluster_size(512)
                    .preallocate(preallocate)
                    .create_open(DenyImplicitOpenGate::default(), |image| {
                        Ok(Qcow2::builder(image).backing(None).write(true))
                    })
                    .await
            })
            .unwrap();
        let disk_image = Arc::new(Mutex::new(SyncFormatAccess::new(qcow2).unwrap()));
        DiskProperties::new(disk_image, Vec::new(), CacheType::Unsafe, false).unwrap()
    }

    fn create_vmdk_disk(path: &std::path::Path) -> DiskProperties {
        let vmdk = Vmdk::<Box<dyn DynStorage>, Arc<imago::FormatAccess<_>>>::builder_path(path)
            .open_sync(PermissiveImplicitOpenGate::default())
            .unwrap();
        let disk_image = Arc::new(Mutex::new(SyncFormatAccess::new(vmdk).unwrap()));
        DiskProperties::new(disk_image, Vec::new(), CacheType::Unsafe, false).unwrap()
    }

    fn temp_image_path(test_name: &str) -> std::path::PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        std::env::temp_dir().join(format!(
            "libkrun-formatted-block-{test_name}-{}-{timestamp}.img",
            std::process::id()
        ))
    }
}
