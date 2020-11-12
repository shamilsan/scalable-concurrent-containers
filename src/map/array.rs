use super::cell::{Cell, ARRAY_SIZE};
use crossbeam::epoch::{Atomic, Guard, Shared};
use std::convert::TryInto;
use std::mem::MaybeUninit;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

pub struct Array<K: Clone + Eq, V> {
    metadata_array: Vec<Cell<K, V>>,
    entry_array: Vec<MaybeUninit<(K, V)>>,
    lb_capacity: u8,
    rehashing: AtomicUsize,
    old_array: Atomic<Array<K, V>>,
}

impl<K: Clone + Eq, V> Array<K, V> {
    pub fn new(capacity: usize, old_array: Atomic<Array<K, V>>) -> Array<K, V> {
        let lb_capacity = Self::calculate_lb_metadata_array_size(capacity);
        let mut array = Array {
            metadata_array: Vec::with_capacity(1 << lb_capacity),
            entry_array: Vec::with_capacity((1 << lb_capacity) * (ARRAY_SIZE as usize)),
            lb_capacity: lb_capacity,
            rehashing: AtomicUsize::new(0),
            old_array: old_array,
        };
        for _ in 0..(1 << lb_capacity) {
            array.metadata_array.push(Default::default());
        }
        for _ in 0..(1 << lb_capacity) * (ARRAY_SIZE as usize) {
            array
                .entry_array
                .push(unsafe { MaybeUninit::uninit().assume_init() });
        }
        array
    }

    pub fn get_cell(&self, index: usize) -> &Cell<K, V> {
        &self.metadata_array[index]
    }

    pub fn get_key_value_pair(&self, index: usize) -> *const (K, V) {
        self.entry_array[index].as_ptr()
    }

    pub fn num_cells(&self) -> usize {
        1 << self.lb_capacity
    }

    pub fn get_old_array<'a>(&self, guard: &'a Guard) -> Shared<'a, Array<K, V>> {
        self.old_array.load(Relaxed, guard)
    }

    pub fn calculate_metadata_array_index(&self, hash: u64) -> usize {
        (hash >> (64 - self.lb_capacity)).try_into().unwrap()
    }

    pub fn calculate_lb_metadata_array_size(capacity: usize) -> u8 {
        let adjusted_capacity = capacity.min((usize::MAX / 2) - (ARRAY_SIZE as usize - 1));
        let required_cells = ((adjusted_capacity + (ARRAY_SIZE as usize - 1))
            / (ARRAY_SIZE as usize))
            .next_power_of_two();
        let lb_capacity =
            ((std::mem::size_of::<usize>() * 8) - (required_cells.leading_zeros() as usize) - 1)
                .max(1);

        // 2^lb_capacity * ARRAY_SIZE >= capacity
        debug_assert!(lb_capacity > 0);
        debug_assert!(lb_capacity < (std::mem::size_of::<usize>() * 8));
        debug_assert!((1 << lb_capacity) * (ARRAY_SIZE as usize) >= adjusted_capacity);
        lb_capacity.try_into().unwrap()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn static_assertions() {
        assert_eq!(0usize.next_power_of_two(), 1);
        assert_eq!(1usize.next_power_of_two(), 1);
        assert_eq!(2usize.next_power_of_two(), 2);
        assert_eq!(3usize.next_power_of_two(), 4);
        assert_eq!(1 << 0, 1);
        assert_eq!(0usize.is_power_of_two(), false);
        assert_eq!(1usize.is_power_of_two(), true);
        assert_eq!(19usize / (ARRAY_SIZE as usize), 1);
        for capacity in 0..1024 as usize {
            assert!(
                (1 << Array::<bool, bool>::calculate_lb_metadata_array_size(capacity))
                    * (ARRAY_SIZE as usize)
                    >= capacity
            );
        }
        assert!(
            (1 << Array::<bool, bool>::calculate_lb_metadata_array_size(usize::MAX))
                * (ARRAY_SIZE as usize)
                >= (usize::MAX / 2)
        );
        for i in 2..(std::mem::size_of::<usize>() - 3) {
            let capacity = (1 << i) * (ARRAY_SIZE as usize);
            assert_eq!(
                Array::<bool, bool>::calculate_lb_metadata_array_size(capacity) as usize,
                i
            );
        }
    }
}
