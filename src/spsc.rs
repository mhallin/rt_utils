use std::mem;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const CACHELINE_SIZE: usize = 64;

pub struct Sender<T> {
    buffer: Arc<RingBuffer<T>>,
}

pub struct Receiver<T> {
    buffer: Arc<RingBuffer<T>>,
}

impl<T> Sender<T> {
    pub fn try_send(&self, value: T) -> bool {
        self.buffer.try_write(value)
    }

    pub fn clear(&self) {
        self.buffer.clear();
    }

    pub fn size(&self) -> usize {
        self.buffer.available_write()
    }

    pub fn is_receiver_active(&self) -> bool {
        Arc::strong_count(&self.buffer) == 2
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Option<T> {
        self.buffer.try_read()
    }

    pub fn size(&self) -> usize {
        self.buffer.available_read()
    }

    pub fn is_sender_active(&self) -> bool {
        Arc::strong_count(&self.buffer) == 2
    }
}

pub fn channel<T>(size: usize) -> (Sender<T>, Receiver<T>) {
    let buffer = Arc::new(RingBuffer::new(size));
    let sender = Sender {
        buffer: buffer.clone(),
    };
    let receiver = Receiver { buffer };

    (sender, receiver)
}

const PADDING1_SIZE: usize = CACHELINE_SIZE - mem::size_of::<usize>() - mem::size_of::<usize>();
const PADDING2_SIZE: usize = CACHELINE_SIZE - mem::size_of::<usize>();

#[repr(C)]
struct RingBuffer<T> {
    entries: NonNull<T>,                // size_of::<usize>()
    size: usize,                        // size_of::<usize>()
    _padding1: [u8; PADDING1_SIZE],     // pad up to next cache line
    pub(self) write_index: AtomicUsize, // size_of::<usize>()
    _padding2: [u8; PADDING2_SIZE],     // pad up to next cache line
    pub(self) read_index: AtomicUsize,
}

unsafe impl<T> Sync for RingBuffer<T> {}
unsafe impl<T> Send for RingBuffer<T> {}

impl<T> RingBuffer<T> {
    fn new(size: usize) -> Self {
        assert!(size > 0, "Can not create channel with zero size");

        let mut entries_vec = Vec::with_capacity(size + 1);
        let entries = entries_vec.as_mut_ptr();

        mem::forget(entries_vec);

        RingBuffer {
            entries: NonNull::new(entries).unwrap(),
            size: size + 1,
            _padding1: [0; PADDING1_SIZE],
            read_index: AtomicUsize::new(0),
            _padding2: [0; PADDING2_SIZE],
            write_index: AtomicUsize::new(0),
        }
    }

    fn clear(&self) {
        self.write_index.store(0, Ordering::SeqCst);
        self.read_index.store(0, Ordering::SeqCst);
    }

    fn try_write(&self, value: T) -> bool {
        let write_index = self.write_index.load(Ordering::Relaxed);
        let read_index = self.read_index.load(Ordering::Acquire);

        if available_write(write_index, read_index, self.size) == 0 {
            return false;
        }

        unsafe { ptr::write(self.entries.as_ptr().add(write_index), value) };

        self.write_index
            .store((write_index + 1) % self.size, Ordering::Release);

        true
    }

    fn try_read(&self) -> Option<T> {
        let write_index = self.write_index.load(Ordering::Acquire);
        let read_index = self.read_index.load(Ordering::Relaxed);

        if available_read(write_index, read_index, self.size) == 0 {
            return None;
        }

        let value = unsafe { ptr::read(self.entries.as_ptr().add(read_index)) };

        self.read_index
            .store((read_index + 1) % self.size, Ordering::Release);

        Some(value)
    }

    fn available_write(&self) -> usize {
        let write_index = self.write_index.load(Ordering::Relaxed);
        let read_index = self.read_index.load(Ordering::Acquire);

        available_write(write_index, read_index, self.size)
    }

    fn available_read(&self) -> usize {
        let write_index = self.write_index.load(Ordering::Acquire);
        let read_index = self.read_index.load(Ordering::Relaxed);

        available_read(write_index, read_index, self.size)
    }
}

impl<T> Drop for RingBuffer<T> {
    fn drop(&mut self) {
        while self.try_read().is_some() {}

        let _entries_vec = unsafe { Vec::from_raw_parts(self.entries.as_ptr(), 0, self.size + 1) };
    }
}

fn available_read(write_index: usize, read_index: usize, size: usize) -> usize {
    if write_index >= read_index {
        write_index - read_index
    } else {
        write_index + size - read_index
    }
}

fn available_write(write_index: usize, read_index: usize, size: usize) -> usize {
    if write_index >= read_index {
        read_index + size - write_index - 1
    } else {
        read_index - write_index - 1
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use memoffset::offset_of;

    #[test]
    fn verify_no_false_sharing() {
        let write_index_offset = offset_of!(RingBuffer<u8>, write_index);
        let read_index_offset = offset_of!(RingBuffer<u8>, read_index);

        assert!(
            write_index_offset == CACHELINE_SIZE,
            "{} != 64",
            write_index_offset
        );
        assert!(
            read_index_offset == 2 * CACHELINE_SIZE,
            "{} != 128",
            read_index_offset
        );
    }

    #[test]
    fn new() {
        let (_send, recv) = channel::<i32>(4);
        assert_eq!(recv.try_recv(), None);
    }

    #[test]
    fn single() {
        let (send, recv) = channel(4);
        assert!(send.try_send(4));
        assert_eq!(recv.try_recv(), Some(4));
    }

    #[test]
    fn multiple() {
        let (send, recv) = channel(4);
        assert!(send.try_send(4));
        assert!(send.try_send(5));
        assert_eq!(recv.try_recv(), Some(4));
        assert_eq!(recv.try_recv(), Some(5));
    }

    #[test]
    fn interleaved() {
        let (send, recv) = channel(4);
        assert!(send.try_send(4));
        assert_eq!(recv.try_recv(), Some(4));
        assert!(send.try_send(5));
        assert_eq!(recv.try_recv(), Some(5));
    }

    #[test]
    fn drain() {
        let (send, recv) = channel(4);
        assert!(send.try_send(4));
        assert!(send.try_send(5));
        assert_eq!(recv.try_recv(), Some(4));
        assert_eq!(recv.try_recv(), Some(5));
        assert_eq!(recv.try_recv(), None);
    }

    #[test]
    fn full() {
        let (send, recv) = channel(4);
        assert!(send.try_send(4));
        assert!(send.try_send(5));
        assert!(send.try_send(6));
        assert!(send.try_send(7));
        assert!(!send.try_send(8));
        assert_eq!(recv.try_recv(), Some(4));
        assert_eq!(recv.try_recv(), Some(5));
        assert_eq!(recv.try_recv(), Some(6));
        assert_eq!(recv.try_recv(), Some(7));
        assert_eq!(recv.try_recv(), None);
    }

    #[test]
    fn drop_unpopped() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct WithDrop(Rc<Cell<i32>>);

        impl Drop for WithDrop {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let drop_count = Rc::new(Cell::new(0));

        {
            let (send, recv) = channel(4);
            assert!(send.try_send(WithDrop(drop_count.clone())));
            assert!(send.try_send(WithDrop(drop_count.clone())));
            assert!(send.try_send(WithDrop(drop_count.clone())));

            {
                let v = recv.try_recv();
                assert!(v.is_some());
            }

            assert_eq!(drop_count.get(), 1);
        }

        assert_eq!(drop_count.get(), 3);
    }

    #[test]
    fn is_receiver_active() {
        let (send, recv) = channel::<i8>(4);
        assert!(send.is_receiver_active());
        drop(recv);
        assert!(!send.is_receiver_active());
    }

    #[test]
    fn is_sender_active() {
        let (send, recv) = channel::<i8>(4);
        assert!(recv.is_sender_active());
        drop(send);
        assert!(!recv.is_sender_active());
    }
}
