//! Lock-free SPSC ring buffer, usable in-process (`RingBuffer`) or across
//! processes via POSIX shared memory (`ShmRingBuffer`).

use memmap2::{MmapMut, MmapOptions};
use std::cell::UnsafeCell;
use std::ffi::CString;
use std::io;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

const PERMISSIONS: libc::mode_t = 0o600;

#[repr(align(64))]
struct Padded(AtomicU64);

#[repr(C)]
pub struct RingBuffer<T, const N: usize> {
    head: Padded,
    tail: Padded,
    dropped: Padded,
    buffer: UnsafeCell<[T; N]>,
}

unsafe impl<T: Send, const N: usize> Sync for RingBuffer<T, N> {}
unsafe impl<T: Send, const N: usize> Send for RingBuffer<T, N> {}

impl<T: Copy + Default, const N: usize> RingBuffer<T, N> {
    const ASSERT_NONZERO_CAPACITY: () = assert!(N > 0, "RingBuffer capacity N must be > 0");

    pub fn new() -> Box<Self> {
        () = Self::ASSERT_NONZERO_CAPACITY;
        unsafe {
            let layout = std::alloc::Layout::new::<Self>();
            let raw = std::alloc::alloc(layout) as *mut Self;
            if raw.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            Self::init_in_place(raw);
            Box::from_raw(raw)
        }
    }

    pub fn push(&self, item: T) -> Result<(), T> {
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= N as u64 {
            self.dropped.0.fetch_add(1, Ordering::Relaxed);
            return Err(item);
        }
        unsafe {
            (*self.buffer.get())[(tail as usize) % N] = item;
        }
        self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    pub fn pop(&self) -> Option<T> {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let item = unsafe { (*self.buffer.get())[(head as usize) % N] };
        self.head.0.store(head.wrapping_add(1), Ordering::Release);
        Some(item)
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped.0.load(Ordering::Relaxed)
    }

    /// Initialize a `RingBuffer` into already-allocated memory (e.g. a mapped
    /// shm page), never building one on the stack.
    ///
    /// # Safety
    /// `mem` must be non-null, aligned for `Self`, and point to
    /// `size_of::<Self>()` writable, unaliased bytes (overwritten here).
    pub unsafe fn init_in_place(mem: *mut Self) {
        () = Self::ASSERT_NONZERO_CAPACITY;
        unsafe {
            std::ptr::write(&raw mut (*mem).head, Padded(AtomicU64::new(0)));
            std::ptr::write(&raw mut (*mem).tail, Padded(AtomicU64::new(0)));
            std::ptr::write(&raw mut (*mem).dropped, Padded(AtomicU64::new(0)));
            let buf_ptr = (&raw mut (*mem).buffer) as *mut T;
            for i in 0..N {
                std::ptr::write(buf_ptr.add(i), T::default());
            }
        }
    }
}

pub struct ShmRingBuffer<T, const N: usize> {
    mmap: MmapMut,
    _marker: PhantomData<T>,
}

impl<T: Copy + Default, const N: usize> ShmRingBuffer<T, N> {
    pub fn create(shm_name: &str) -> io::Result<Self> {
        let fd = Self::shm_open(shm_name, libc::O_CREAT | libc::O_RDWR)?;
        let size = std::mem::size_of::<RingBuffer<T, N>>();
        unsafe {
            if libc::ftruncate(fd, size as libc::off_t) != 0 {
                let err = io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
        }
        let mut mmap = unsafe {
            let result = MmapOptions::new().len(size).map_mut(fd);
            libc::close(fd);
            result?
        };
        let ptr = mmap.as_mut_ptr() as *mut RingBuffer<T, N>;
        unsafe {
            RingBuffer::init_in_place(ptr);
        }
        Ok(Self {
            mmap,
            _marker: PhantomData,
        })
    }

    pub fn open(shm_name: &str) -> io::Result<Self> {
        let fd = Self::shm_open(shm_name, libc::O_RDWR)?;
        let size = std::mem::size_of::<RingBuffer<T, N>>();
        let actual = unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut stat) != 0 {
                let err = io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
            stat.st_size as usize
        };
        if actual != size {
            unsafe {
                libc::close(fd);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "shared memory segment '{shm_name}' is {actual} bytes, expected {size} bytes \
                     (RingBuffer<T, N> type/capacity mismatch between processes)"
                ),
            ));
        }
        let mmap = unsafe {
            let result = MmapOptions::new().len(size).map_mut(fd);
            libc::close(fd);
            result?
        };
        Ok(Self {
            mmap,
            _marker: PhantomData,
        })
    }

    pub fn unlink(shm_name: &str) -> io::Result<()> {
        let c_name = Self::shm_c_name(shm_name)?;
        let res = unsafe { libc::shm_unlink(c_name.as_ptr()) };
        if res != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn shm_c_name(shm_name: &str) -> io::Result<CString> {
        CString::new(shm_name).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "shm name contains a null byte")
        })
    }

    fn shm_open(shm_name: &str, flags: libc::c_int) -> io::Result<libc::c_int> {
        let c_name = Self::shm_c_name(shm_name)?;
        let fd = unsafe { libc::shm_open(c_name.as_ptr(), flags, PERMISSIONS) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

impl<T, const N: usize> std::ops::Deref for ShmRingBuffer<T, N> {
    type Target = RingBuffer<T, N>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(self.mmap.as_ptr() as *const RingBuffer<T, N>) }
    }
}

unsafe impl<T: Send, const N: usize> Send for ShmRingBuffer<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for ShmRingBuffer<T, N> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_respects_fifo_order_and_capacity() {
        let rb = RingBuffer::<u32, 4>::new();
        assert_eq!(rb.pop(), None);

        for i in 0..4 {
            rb.push(i).unwrap();
        }
        assert_eq!(rb.push(99), Err(99));
        assert_eq!(rb.dropped_count(), 1);

        for i in 0..4 {
            assert_eq!(rb.pop(), Some(i));
        }
        assert_eq!(rb.pop(), None);
    }

    #[test]
    fn new_does_not_overflow_the_stack_for_large_n() {
        let rb = RingBuffer::<u8, { 16 * 1024 * 1024 }>::new();
        rb.push(42).unwrap();
        assert_eq!(rb.pop(), Some(42));
    }

    #[test]
    fn shm_ring_buffer_round_trips_across_processes() {
        let shm_name = format!("/sherlock_test_ring_{}", std::process::id());

        let parent = ShmRingBuffer::<u64, 8>::create(&shm_name).expect("create shm ring buffer");

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");

        if pid == 0 {
            let child =
                ShmRingBuffer::<u64, 8>::open(&shm_name).expect("child open shm ring buffer");
            for i in 0..8u64 {
                while child.push(i).is_err() {}
            }
            std::process::exit(0);
        }

        let mut status: libc::c_int = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(waited, pid);
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "child exited abnormally: {status}"
        );

        let mut received = Vec::new();
        while received.len() < 8 {
            if let Some(v) = parent.pop() {
                received.push(v);
            }
        }
        assert_eq!(received, (0..8u64).collect::<Vec<_>>());

        ShmRingBuffer::<u64, 8>::unlink(&shm_name).expect("unlink shm ring buffer");
    }

    #[test]
    fn open_rejects_mismatched_capacity() {
        let shm_name = format!("/sherlock_test_ring_mismatch_{}", std::process::id());
        let _producer = ShmRingBuffer::<u64, 8>::create(&shm_name).expect("create shm ring buffer");

        match ShmRingBuffer::<u64, 16>::open(&shm_name) {
            Err(err) => assert_eq!(err.kind(), io::ErrorKind::InvalidData),
            Ok(_) => panic!("size mismatch should error"),
        }

        ShmRingBuffer::<u64, 8>::unlink(&shm_name).expect("unlink shm ring buffer");
    }
}
