use core::{
    cmp, ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{consts::BUF_SIZE, MODE_BLOCK_IF_FULL, MODE_MASK};

/// RTT Up channel
#[repr(C)]
pub(crate) struct Channel {
    pub name: *const u8,
    /// Pointer to the RTT buffer.
    pub buffer: *mut u8,
    pub size: usize,
    /// Written by the target.
    pub write: AtomicUsize,
    /// Written by the host.
    pub read: AtomicUsize,
    /// Channel properties.
    ///
    /// Currently, only the lowest 2 bits are used to set the channel mode (see constants below).
    pub flags: AtomicUsize,
}

impl Channel {
    pub fn write_all(&self, mut bytes: &[u8]) {
        // the host-connection-status is only modified after RAM initialization while the device is
        // halted, so we only need to check it once before the write-loop
        let write = match self.host_is_connected() {
            _ if cfg!(feature = "disable-blocking-mode") => Self::nonblocking_write,
            true => Self::blocking_write,
            false => Self::nonblocking_write,
        };

        while !bytes.is_empty() {
            let consumed = write(self, bytes);
            if consumed != 0 {
                bytes = &bytes[consumed..];
            }
        }
    }

    fn blocking_write(&self, bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        // calculate how much space is left in the buffer
        let read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        let available = available_buffer_size(read, write);

        // abort if buffer is full
        if available == 0 {
            return 0;
        }

        self.write_impl(bytes, write, available)
    }

    fn nonblocking_write(&self, bytes: &[u8]) -> usize {
        let write = self.write.load(Ordering::Acquire);

        // NOTE truncate at BUF_SIZE to avoid more than one "wrap-around" in a single `write` call
        self.write_impl(bytes, write, BUF_SIZE)
    }

    fn write_impl(&self, bytes: &[u8], cursor: usize, available: usize) -> usize {
        let len = bytes.len().min(available);

        // copy `bytes[..len]` to the RTT buffer
        unsafe {
            if cursor + len > BUF_SIZE {
                // split memcpy
                let pivot = BUF_SIZE - cursor;
                ptr::copy_nonoverlapping(bytes.as_ptr(), self.buffer.add(cursor), pivot);
                ptr::copy_nonoverlapping(bytes.as_ptr().add(pivot), self.buffer, len - pivot);
            } else {
                // single memcpy
                ptr::copy_nonoverlapping(bytes.as_ptr(), self.buffer.add(cursor), len);
            }
        }

        // adjust the write pointer, so the host knows that there is new data
        self.write
            .store(cursor.wrapping_add(len) % BUF_SIZE, Ordering::Release);

        // return the number of bytes written
        len
    }

    pub fn flush(&self) {
        // return early, if host is disconnected
        if !self.host_is_connected() {
            return;
        }

        // busy wait, until the read- catches up with the write-pointer
        let read = || self.read.load(Ordering::Relaxed);
        let write = || self.write.load(Ordering::Relaxed);
        while read() != write() {}
    }

    // Reads data from the channel buffer into `buf`
    //
    // Returns the number of bytes read. If a host is connected, this method always returns 0.
    // If two or more threads call `read` simultaneously, at least one will perform a valid read while the others may
    // return 0.
    pub fn read(&self, buf: &mut [u8]) -> usize {
        if self.host_is_connected() {
            return 0;
        }

        let read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Relaxed);

        let len = match read.cmp(&write) {
            cmp::Ordering::Equal => 0,
            cmp::Ordering::Less => write - read,
            cmp::Ordering::Greater => (BUF_SIZE - read) + write,
        };

        let len = len.min(buf.len());
        let new_read = if read + len > BUF_SIZE {
            // split memcpy
            let pivot = BUF_SIZE - read;
            unsafe { ptr::copy_nonoverlapping(self.buffer.add(read), buf.as_mut_ptr(), pivot) };
            unsafe {
                ptr::copy_nonoverlapping(self.buffer, buf.as_mut_ptr().add(pivot), len - pivot)
            };
            len - pivot
        } else {
            // single memcpy
            unsafe { ptr::copy_nonoverlapping(self.buffer.add(read), buf.as_mut_ptr(), len) };
            (read + len) % BUF_SIZE
        };

        match self
            .read
            .compare_exchange(read, new_read, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => len,
            Err(_) => 0,
        }
    }

    fn host_is_connected(&self) -> bool {
        // we assume that a host is connected if we are in blocking-mode. this is what probe-run does.
        self.flags.load(Ordering::Relaxed) & MODE_MASK == MODE_BLOCK_IF_FULL
    }
}

/// How much space is left in the buffer?
fn available_buffer_size(read_cursor: usize, write_cursor: usize) -> usize {
    if read_cursor > write_cursor {
        read_cursor - write_cursor - 1
    } else if read_cursor == 0 {
        BUF_SIZE - write_cursor - 1
    } else {
        BUF_SIZE - write_cursor
    }
}
