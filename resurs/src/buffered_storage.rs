pub use crate::{Handle, Storage};

const N_STORAGE_BUFFERS: usize = 2;

/// Convenience type for double buffered storage of T
pub struct BufferedStorage<T> {
    storage: Storage<[T; N_STORAGE_BUFFERS]>,
}

impl<T> Handle<[T; N_STORAGE_BUFFERS]> {
    pub fn as_unbuffered(self) -> Handle<T> {
        Handle::<T>::new(self.id)
    }
}

impl<T> Handle<T> {
    pub fn as_buffered(&self) -> Handle<[T; N_STORAGE_BUFFERS]> {
        Handle::<[T; N_STORAGE_BUFFERS]>::new(self.id)
    }
}

impl<T> BufferedStorage<T> {
    pub const N_BUFFERS: usize = N_STORAGE_BUFFERS;
    pub fn add(&mut self, t: [T; N_STORAGE_BUFFERS]) -> Handle<T> {
        self.storage.add(t).as_unbuffered()
    }

    pub fn remove(&mut self, h: Handle<T>) -> Option<[T; N_STORAGE_BUFFERS]> {
        self.storage.remove(h.as_buffered())
    }

    pub fn has(&self, h: &Handle<T>) -> bool {
        self.storage.has(&h.as_buffered())
    }

    pub fn get(&self, h: &Handle<T>, idx: usize) -> Option<&T> {
        self.storage.get(&h.as_buffered()).map(|x| &x[idx])
    }

    pub fn get_mut(&mut self, h: &Handle<T>, idx: usize) -> Option<&mut T> {
        self.storage.get_mut(&h.as_buffered()).map(|x| &mut x[idx])
    }

    pub fn get_all(&self, h: &Handle<T>) -> Option<&[T; N_STORAGE_BUFFERS]> {
        self.storage.get(&h.as_buffered())
    }

    pub fn get_all_mut(&mut self, h: &Handle<T>) -> Option<&mut [T; N_STORAGE_BUFFERS]> {
        self.storage.get_mut(&h.as_buffered())
    }

    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }

    pub fn len(&self) -> usize {
        self.storage.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &[T; N_STORAGE_BUFFERS]> {
        self.storage.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut [T; N_STORAGE_BUFFERS]> {
        self.storage.iter_mut()
    }

    pub fn drain_filter<F>(&mut self, f: F) -> super::storage::DrainFilter<'_, F, [T; 2]>
    where
        F: FnMut(&mut [T; 2]) -> bool,
    {
        super::storage::DrainFilter::new(&mut self.storage, f)
    }
}

impl<T> Default for BufferedStorage<T> {
    fn default() -> Self {
        Self {
            storage: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add() {
        let mut s = BufferedStorage::default();
        let ints = [3, 10];
        let h = s.add(ints);

        assert_eq!(s.len(), 1);
        assert!(s.has(&h));
        assert_eq!(*s.get(&h, 0).unwrap(), 3);
        assert_eq!(*s.get(&h, 1).unwrap(), 10);
        assert_eq!(*s.get_all(&h).unwrap(), [3, 10]);
        assert_eq!(s.get(&h, 0).copied(), s.get_mut(&h, 0).copied());
        assert_eq!(s.get(&h, 1).copied(), s.get_mut(&h, 1).copied());
    }

    #[test]
    fn remove() {
        let mut s = BufferedStorage::default();
        let ints0 = [3, 10];
        let h0 = s.add(ints0);

        let ints1 = [30, 100];
        let h1 = s.add(ints1);
        assert_eq!(s.len(), 2);

        let ints_ret = s.remove(h0).expect("Missing!");
        assert_eq!(ints_ret, ints0);
        assert_eq!(s.len(), 1);
        assert_eq!(*s.get(&h1, 0).unwrap(), 30);
        assert_eq!(*s.get(&h1, 1).unwrap(), 100);
        assert_eq!(*s.get_all(&h1).unwrap(), [30, 100]);
    }
}
