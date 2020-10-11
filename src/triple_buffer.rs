use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const INDEX_MASK: usize = 0b0011;
const COMMIT_BIT: usize = 0b0100;

struct Internal<T> {
    buffers: [UnsafeCell<ManuallyDrop<T>>; 3],
    committed: AtomicUsize,
}

unsafe impl<T> Sync for Internal<T> {}
unsafe impl<T> Send for Internal<T> {}

pub struct Writer<T> {
    internal: Arc<Internal<T>>,
    write_index: usize,
}

pub struct WriteGuard<'a, T> {
    value: &'a mut ManuallyDrop<T>,
    writer: &'a mut Writer<T>,
}

pub struct Reader<T> {
    internal: Arc<Internal<T>>,
    read_index: usize,
}

impl<T> Writer<T> {
    pub fn write(&mut self, value: T) {
        let value_ptr = unsafe {
            self.internal.buffers[self.write_index]
                .get()
                .as_mut()
                .unwrap()
        };

        unsafe {
            ManuallyDrop::drop(value_ptr);
            ptr::write(value_ptr, ManuallyDrop::new(value))
        }

        let last_committed = self
            .internal
            .committed
            .swap(self.write_index | COMMIT_BIT, Ordering::Release);
        self.write_index = last_committed & INDEX_MASK;
    }

    pub fn get_mut(&mut self) -> WriteGuard<T> {
        let value_ptr = unsafe {
            self.internal.buffers[self.write_index]
                .get()
                .as_mut()
                .unwrap()
        };

        WriteGuard {
            value: value_ptr,
            writer: self,
        }
    }

    fn commit_write_guard<'a>(guard: &mut WriteGuard<'a, T>) {
        let last_committed = guard
            .writer
            .internal
            .committed
            .swap(guard.writer.write_index | COMMIT_BIT, Ordering::Release);
        guard.writer.write_index = last_committed & INDEX_MASK;
    }
}

impl<'a, T> Deref for WriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value
    }
}

impl<'a, T> DerefMut for WriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value
    }
}

impl<'a, T> Drop for WriteGuard<'a, T> {
    fn drop(&mut self) {
        Writer::commit_write_guard(self)
    }
}

impl<T> Reader<T> {
    pub fn read(&mut self) -> &T {
        if self.internal.committed.load(Ordering::Relaxed) & COMMIT_BIT != 0 {
            let last_committed = self
                .internal
                .committed
                .swap(self.read_index, Ordering::Acquire);

            self.read_index = last_committed & INDEX_MASK;
        }

        unsafe {
            self.internal.buffers[self.read_index]
                .get()
                .as_ref()
                .unwrap()
        }
    }
}

impl<T> Drop for Internal<T> {
    fn drop(&mut self) {
        for v in self.buffers.iter_mut() {
            unsafe { ManuallyDrop::drop(v.get().as_mut().unwrap()) };
        }
    }
}

pub fn triple_buffer_explicit<T>(initial_values: (T, T, T)) -> (Writer<T>, Reader<T>) {
    let internal = Arc::new(Internal {
        buffers: [
            UnsafeCell::new(ManuallyDrop::new(initial_values.0)),
            UnsafeCell::new(ManuallyDrop::new(initial_values.1)),
            UnsafeCell::new(ManuallyDrop::new(initial_values.2)),
        ],
        committed: AtomicUsize::new(1),
    });

    let writer = Writer {
        internal: internal.clone(),
        write_index: 2,
    };
    let reader = Reader {
        internal,
        read_index: 0,
    };

    (writer, reader)
}

pub fn triple_buffer<T: Clone>(initial_value: T) -> (Writer<T>, Reader<T>) {
    triple_buffer_explicit((initial_value.clone(), initial_value.clone(), initial_value))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn new() {
        let (_writer, mut reader) = triple_buffer(123);
        assert_eq!(reader.read(), &123);
    }

    #[test]
    fn write() {
        let (mut writer, mut reader) = triple_buffer(123);
        writer.write(345);
        assert_eq!(reader.read(), &345);
    }

    #[test]
    fn overwrite() {
        let (mut writer, mut reader) = triple_buffer(123);
        writer.write(345);
        writer.write(567);
        assert_eq!(reader.read(), &567);
    }

    #[test]
    fn interleaved() {
        let (mut writer, mut reader) = triple_buffer(123);
        writer.write(345);
        assert_eq!(reader.read(), &345);
        writer.write(567);
        assert_eq!(reader.read(), &567);
        assert_eq!(reader.read(), &567);
    }

    #[test]
    fn get_mut() {
        let (mut writer, mut reader) = triple_buffer(1213);
        {
            let mut v = writer.get_mut();
            *v = 456;
        }

        assert_eq!(reader.read(), &456);

        {
            let mut v = writer.get_mut();
            *v = 567;
        }

        assert_eq!(reader.read(), &567);
    }

    mod drop {
        use super::*;

        use std::cell::Cell;
        use std::rc::Rc;

        #[derive(Clone)]
        struct WithDrop(Rc<Cell<i32>>);

        impl Drop for WithDrop {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        #[test]
        fn unwritten() {
            let drop_count = Rc::new(Cell::new(0));

            {
                let (_writer, _reader) = triple_buffer(WithDrop(drop_count.clone()));
            }

            // 3 values inside the buffer dropped
            assert_eq!(drop_count.get(), 3);
        }

        #[test]
        fn fully_written() {
            let drop_count = Rc::new(Cell::new(0));

            {
                let (mut writer, mut reader) = triple_buffer(WithDrop(drop_count.clone()));
                writer.write(WithDrop(drop_count.clone()));
                reader.read();
                writer.write(WithDrop(drop_count.clone()));
            }

            // 3 values inside the buffer,
            // 1 value written and read,
            // 1 value written but not read
            assert_eq!(drop_count.get(), 5);
        }

        #[test]
        fn overwrite() {
            let drop_count = Rc::new(Cell::new(0));

            {
                let (mut writer, _reader) = triple_buffer(WithDrop(drop_count.clone()));
                writer.write(WithDrop(drop_count.clone()));
                writer.write(WithDrop(drop_count.clone()));
            }

            // 3 values inside the buffer,
            // 2 values written but not read
            assert_eq!(drop_count.get(), 5);
        }

        #[test]
        fn get_mut() {
            let drop_count = Rc::new(Cell::new(0));

            {
                let (mut writer, _) = triple_buffer(WithDrop(drop_count.clone()));
                {
                    let _ = writer.get_mut();
                }
            }

            // 3 values inside the buffer,
            // no value constructed by get_mut - it's modified in place
            assert_eq!(drop_count.get(), 3);
        }
    }
}
