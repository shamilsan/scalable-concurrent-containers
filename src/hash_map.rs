//! The module implements [`HashMap`].

use super::async_yield::{self, AwaitableBarrier};
use super::ebr::{Arc, AtomicArc, Barrier};
use super::hash_table::cell::Locker;
use super::hash_table::cell_array::CellArray;
use super::hash_table::HashTable;

use std::borrow::Borrow;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash};
use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::sync::atomic::{AtomicU8, AtomicUsize};

/// Scalable concurrent hash map data structure.
///
/// [`HashMap`] is a concurrent hash map data structure that is targeted at a highly concurrent
/// workload. The use of an epoch-based reclamation technique enables the data structure to
/// implement non-blocking resizing and fine-granular locking. It has a single array of entry
/// metadata, and each entry of the array, called `Cell`, manages a fixed size key-value pair
/// array. Each `Cell` has a customized 8-byte read-write mutex to protect the data structure,
/// and a linked list for resolving hash collisions.
///
/// ## The key features of [`HashMap`]
///
/// * Non-sharded: the data is managed by a single entry metadata array.
/// * Automatic resizing: it automatically grows or shrinks.
/// * Non-blocking resizing: resizing does not block other threads.
/// * Incremental resizing: each access to the data structure is mandated to rehash a fixed
///   number of key-value pairs.
/// * Optimized resizing: key-value pairs managed by a single `Cell` are guaranteed to be
///   relocated to consecutive `Cell` instances.
/// * No busy waiting: the customized mutex never spins.
///
/// ## The key statistics for [`HashMap`]
///
/// * The expected size of metadata for a single key-value pair: 2-byte.
/// * The expected number of atomic operations required for an operation on a single key: 2.
/// * The expected number of atomic variables accessed during a single key operation: 1.
/// * The number of entries managed by a single metadata `Cell` without a linked list: 32.
/// * The expected maximum linked list length when resize is triggered: log(capacity) / 8.
pub struct HashMap<K, V, H = RandomState>
where
    K: 'static + Eq + Hash + Sync,
    V: 'static + Sync,
    H: BuildHasher,
{
    array: AtomicArc<CellArray<K, V, false>>,
    minimum_capacity: usize,
    additional_capacity: AtomicUsize,
    resize_mutex: AtomicU8,
    build_hasher: H,
}

impl<K, V, H> HashMap<K, V, H>
where
    K: 'static + Eq + Hash + Sync,
    V: 'static + Sync,
    H: BuildHasher,
{
    /// Creates an empty [`HashMap`] with the given capacity and [`BuildHasher`].
    ///
    /// The actual capacity is equal to or greater than the given capacity.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(1000, RandomState::new());
    ///
    /// let result = hashmap.capacity();
    /// assert_eq!(result, 1024);
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    /// let result = hashmap.capacity();
    /// assert_eq!(result, 64);
    /// ```
    pub fn new(capacity: usize, build_hasher: H) -> HashMap<K, V, H> {
        let initial_capacity = capacity.max(Self::default_capacity());
        let array = Arc::new(CellArray::<K, V, false>::new(
            initial_capacity,
            AtomicArc::null(),
        ));
        let current_capacity = array.num_entries();
        HashMap {
            array: AtomicArc::from(array),
            minimum_capacity: current_capacity,
            additional_capacity: AtomicUsize::new(0),
            resize_mutex: AtomicU8::new(0),
            build_hasher,
        }
    }

    /// Temporarily increases the minimum capacity of the [`HashMap`].
    ///
    /// The reserved space is not exclusively owned by the [`Ticket`], thus can be overtaken.
    /// Unused space is immediately reclaimed when the [`Ticket`] is dropped.
    ///
    /// # Errors
    ///
    /// Returns `None` if a too large number is given.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<usize, usize, RandomState> = HashMap::new(1000, RandomState::new());
    /// assert_eq!(hashmap.capacity(), 1024);
    ///
    /// let ticket = hashmap.reserve(10000);
    /// assert!(ticket.is_some());
    /// assert_eq!(hashmap.capacity(), 16384);
    /// for i in 0..16 {
    ///     assert!(hashmap.insert(i, i).is_ok());
    /// }
    /// drop(ticket);
    ///
    /// assert_eq!(hashmap.capacity(), 1024);
    /// ```
    pub fn reserve(&self, capacity: usize) -> Option<Ticket<K, V, H>> {
        let mut current_additional_capacity = self.additional_capacity.load(Relaxed);
        loop {
            if usize::MAX - self.minimum_capacity - current_additional_capacity <= capacity {
                // The given value is too large.
                return None;
            }
            match self.additional_capacity.compare_exchange(
                current_additional_capacity,
                current_additional_capacity + capacity,
                Relaxed,
                Relaxed,
            ) {
                Ok(_) => {
                    self.resize(&Barrier::new());
                    return Some(Ticket {
                        hash_map: self,
                        increment: capacity,
                    });
                }
                Err(current) => current_additional_capacity = current,
            }
        }
    }

    /// Inserts a key-value pair into the [`HashMap`].
    ///
    /// # Errors
    ///
    /// Returns an error along with the supplied key-value pair if the key exists.
    ///
    /// # Panics
    ///
    /// Panics if memory allocation fails, or the number of entries in the target cell reaches
    /// `u32::MAX`.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(hashmap.insert(1, 0).is_ok());
    /// assert_eq!(hashmap.insert(1, 1).unwrap_err(), (1, 1));
    /// ```
    #[inline]
    pub fn insert(&self, key: K, val: V) -> Result<(), (K, V)> {
        let (hash, partial_hash) = self.hash(&key);
        if let Some((k, v)) = self
            .insert_entry::<false>(key, val, hash, partial_hash, &Barrier::new())
            .ok()
            .unwrap()
        {
            Err((k, v))
        } else {
            Ok(())
        }
    }

    /// Inserts a key-value pair into the [`HashMap`].
    ///
    /// It is an asynchronous method returning an `impl Future` for the caller to await or poll.
    ///
    /// # Errors
    ///
    /// Returns an error along with the supplied key-value pair if the key exists.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    /// let future_insert = hashmap.insert_async(11, 17);
    /// ```
    #[inline]
    pub async fn insert_async(&self, mut key: K, mut val: V) -> Result<(), (K, V)> {
        let (hash, partial_hash) = self.hash(&key);
        loop {
            match self.insert_entry::<true>(key, val, hash, partial_hash, &Barrier::new()) {
                Ok(Some(returned)) => return Err(returned),
                Ok(None) => return Ok(()),
                Err(returned) => {
                    key = returned.0;
                    val = returned.1;
                }
            }
            async_yield::async_yield().await;
        }
    }

    /// Updates an existing key-value pair.
    ///
    /// It returns `None` if the key does not exist.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(hashmap.update(&1, |_, _| true).is_none());
    /// assert!(hashmap.insert(1, 0).is_ok());
    /// assert_eq!(hashmap.update(&1, |_, v| { *v = 2; *v }).unwrap(), 2);
    /// assert_eq!(hashmap.read(&1, |_, v| *v).unwrap(), 2);
    /// ```
    #[inline]
    pub fn update<Q, F, R>(&self, key_ref: &Q, updater: F) -> Option<R>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
        F: FnOnce(&K, &mut V) -> R,
    {
        let (hash, partial_hash) = self.hash(key_ref);
        let barrier = Barrier::new();
        let (_, _, iterator) = self
            .acquire::<Q, false>(key_ref, hash, partial_hash, &barrier)
            .ok()?;
        if let Some(iterator) = iterator {
            if let Some((k, v)) = iterator.get() {
                // The presence of `locker` prevents the entry from being modified outside it.
                #[allow(clippy::cast_ref_to_mut)]
                return Some(updater(k, unsafe { &mut *(v as *const V as *mut V) }));
            }
        }
        None
    }

    /// Constructs the value in-place, or modifies an existing value corresponding to the key.
    ///
    /// # Panics
    ///
    /// Panics if memory allocation fails, or the number of entries in the target cell is
    /// reached `u32::MAX`.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// hashmap.upsert(1, || 2, |_, v| *v = 2);
    /// assert_eq!(hashmap.read(&1, |_, v| *v).unwrap(), 2);
    /// hashmap.upsert(1, || 2, |_, v| *v = 3);
    /// assert_eq!(hashmap.read(&1, |_, v| *v).unwrap(), 3);
    /// ```
    #[inline]
    pub fn upsert<FI: FnOnce() -> V, FU: FnOnce(&K, &mut V)>(
        &self,
        key: K,
        constructor: FI,
        updater: FU,
    ) {
        let (hash, partial_hash) = self.hash(&key);
        let barrier = Barrier::new();
        let (_, locker, iterator) = self
            .acquire::<_, false>(&key, hash, partial_hash, &barrier)
            .ok()
            .unwrap();
        if let Some(iterator) = iterator {
            if let Some((k, v)) = iterator.get() {
                // The presence of `locker` prevents the entry from being modified outside it.
                #[allow(clippy::cast_ref_to_mut)]
                updater(k, unsafe { &mut *(v as *const V as *mut V) });
                return;
            }
        }
        locker.insert(key, constructor(), partial_hash, &barrier);
    }

    /// Removes a key-value pair if the key exists.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(hashmap.remove(&1).is_none());
    /// assert!(hashmap.insert(1, 0).is_ok());
    /// assert_eq!(hashmap.remove(&1).unwrap(), (1, 0));
    /// ```
    #[inline]
    pub fn remove<Q>(&self, key_ref: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.remove_if(key_ref, |_| true)
    }

    /// Removes a key-value pair if the key exists.
    ///
    /// It is an asynchronous method returning an `impl Future` for the caller to await or poll.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    /// let future_insert = hashmap.insert_async(11, 17);
    /// let future_remove = hashmap.remove_async(&11);
    /// ```
    #[inline]
    pub async fn remove_async<Q>(&self, key_ref: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.remove_if_async(key_ref, |_| true).await
    }

    /// Removes a key-value pair if the key exists and the given condition is met.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(hashmap.insert(1, 0).is_ok());
    /// assert!(hashmap.remove_if(&1, |v| *v == 1).is_none());
    /// assert_eq!(hashmap.remove_if(&1, |v| *v == 0).unwrap(), (1, 0));
    /// ```
    #[inline]
    pub fn remove_if<Q, F: FnMut(&V) -> bool>(
        &self,
        key_ref: &Q,
        mut condition: F,
    ) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let (hash, partial_hash) = self.hash(key_ref);
        self.remove_entry::<Q, _, false>(
            key_ref,
            hash,
            partial_hash,
            &mut condition,
            &Barrier::new(),
        )
        .ok()
        .and_then(|(r, _)| r)
    }

    /// Removes a key-value pair if the key exists and the given condition is met.
    ///
    /// It is an asynchronous method returning an `impl Future` for the caller to await or poll.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    /// let future_insert = hashmap.insert_async(11, 17);
    /// let future_remove = hashmap.remove_if_async(&11, |_| true);
    /// ```
    #[inline]
    pub async fn remove_if_async<Q, F: FnMut(&V) -> bool>(
        &self,
        key_ref: &Q,
        mut condition: F,
    ) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let (hash, partial_hash) = self.hash(key_ref);
        loop {
            if let Ok(result) = self.remove_entry::<Q, F, true>(
                key_ref,
                hash,
                partial_hash,
                &mut condition,
                &Barrier::new(),
            ) {
                return result.0;
            }
            async_yield::async_yield().await;
        }
    }

    /// Reads a key-value pair.
    ///
    /// It returns `None` if the key does not exist.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(hashmap.read(&1, |_, v| *v).is_none());
    /// assert!(hashmap.insert(1, 10).is_ok());
    /// assert_eq!(hashmap.read(&1, |_, v| *v).unwrap(), 10);
    /// ```
    #[inline]
    pub fn read<Q, R, F: FnMut(&K, &V) -> R>(&self, key_ref: &Q, reader: F) -> Option<R>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let barrier = Barrier::new();
        self.read_with(key_ref, reader, &barrier)
    }

    /// Reads a key-value pair.
    ///
    /// It returns `None` if the key does not exist. It is an asynchronous method returning an
    /// `impl Future` for the caller to await or poll.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    /// let future_insert = hashmap.insert_async(11, 17);
    /// let future_read = hashmap.read_async(&11, |_, v| *v);
    /// ```
    #[inline]
    pub async fn read_async<Q, R, F: FnMut(&K, &V) -> R>(
        &self,
        key_ref: &Q,
        mut reader: F,
    ) -> Option<R>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let (hash, partial_hash) = self.hash(key_ref);
        loop {
            if let Ok(result) = self.read_entry::<Q, R, _, true>(
                key_ref,
                hash,
                partial_hash,
                &mut reader,
                &Barrier::new(),
            ) {
                return result;
            }
            async_yield::async_yield().await;
        }
    }

    /// Reads a key-value pair using the supplied [`Barrier`].
    ///
    /// It enables the caller to use the value reference outside the method. It returns `None`
    /// if the key does not exist.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::ebr::Barrier;
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(hashmap.insert(1, 10).is_ok());
    ///
    /// let barrier = Barrier::new();
    /// let value_ref = hashmap.read_with(&1, |k, v| v, &barrier).unwrap();
    /// assert_eq!(*value_ref, 10);
    /// ```
    #[inline]
    pub fn read_with<'b, Q, R, F: FnMut(&'b K, &'b V) -> R>(
        &self,
        key_ref: &Q,
        mut reader: F,
        barrier: &'b Barrier,
    ) -> Option<R>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let (hash, partial_hash) = self.hash(key_ref);
        self.read_entry::<Q, R, F, false>(key_ref, hash, partial_hash, &mut reader, barrier)
            .ok()
            .and_then(|r| r)
    }

    /// Checks if the key exists.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(!hashmap.contains(&1));
    /// assert!(hashmap.insert(1, 0).is_ok());
    /// assert!(hashmap.contains(&1));
    /// ```
    #[inline]
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.read(key, |_, _| ()).is_some()
    }

    /// Iterates over all the entries in the [`HashMap`].
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(hashmap.insert(1, 0).is_ok());
    /// assert!(hashmap.insert(2, 1).is_ok());
    ///
    /// let mut acc = 0;
    /// hashmap.for_each(|k, v| { acc += *k; *v = 2; });
    /// assert_eq!(acc, 3);
    /// assert_eq!(hashmap.read(&1, |_, v| *v).unwrap(), 2);
    /// assert_eq!(hashmap.read(&2, |_, v| *v).unwrap(), 2);
    /// ```
    #[inline]
    pub fn for_each<F: FnMut(&K, &mut V)>(&self, mut f: F) {
        self.retain(|k, v| {
            f(k, v);
            true
        });
    }

    /// Iterates over all the entries in the [`HashMap`].
    ///
    /// It is an asynchronous method returning an `impl Future` for the caller to await or poll.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// let future_insert = hashmap.insert_async(1, 0);
    /// let future_for_each = hashmap.for_each_async(|k, v| println!("{} {}", k, v));
    /// ```
    #[inline]
    pub async fn for_each_async<F: FnMut(&K, &mut V)>(&self, mut f: F) {
        self.retain_async(|k, v| {
            f(k, v);
            true
        })
        .await;
    }

    /// Retains key-value pairs that satisfy the given predicate.
    ///
    /// It returns the number of entries remaining and removed.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(hashmap.insert(1, 0).is_ok());
    /// assert!(hashmap.insert(2, 1).is_ok());
    /// assert!(hashmap.insert(3, 2).is_ok());
    ///
    /// assert_eq!(hashmap.retain(|k, v| *k == 1 && *v == 0), (1, 2));
    /// ```
    pub fn retain<F: FnMut(&K, &mut V) -> bool>(&self, mut filter: F) -> (usize, usize) {
        let mut retained_entries = 0;
        let mut removed_entries = 0;

        let barrier = Barrier::new();

        // An acquire fence is required to correctly load the contents of the array.
        let mut current_array_ptr = self.array.load(Acquire, &barrier);
        while let Some(current_array_ref) = current_array_ptr.as_ref() {
            if !current_array_ref.old_array(&barrier).is_null() {
                current_array_ref.partial_rehash::<_, _, _, false>(
                    |key| self.hash(key),
                    |_, _| None,
                    &barrier,
                );
                current_array_ptr = self.array.load(Acquire, &barrier);
                continue;
            }

            for cell_index in 0..current_array_ref.num_cells() {
                if let Some(locker) = Locker::lock(current_array_ref.cell(cell_index), &barrier) {
                    let mut iterator = locker.cell().iter(&barrier);
                    while iterator.next().is_some() {
                        let retain = if let Some((k, v)) = iterator.get() {
                            #[allow(clippy::cast_ref_to_mut)]
                            filter(k, unsafe { &mut *(v as *const V as *mut V) })
                        } else {
                            true
                        };
                        if retain {
                            retained_entries += 1;
                        } else {
                            locker.erase(&mut iterator);
                            removed_entries += 1;
                        }
                        if retained_entries == usize::MAX || removed_entries == usize::MAX {
                            // Gives up iteration on an integer overflow.
                            return (retained_entries, removed_entries);
                        }
                    }
                }
            }

            let new_current_array_ptr = self.array.load(Acquire, &barrier);
            if current_array_ptr == new_current_array_ptr {
                break;
            }
            retained_entries = 0;
            current_array_ptr = new_current_array_ptr;
        }

        if removed_entries >= retained_entries {
            self.resize(&barrier);
        }

        (retained_entries, removed_entries)
    }

    /// Retains key-value pairs that satisfy the given predicate.
    ///
    /// It returns the number of entries remaining and removed. It is an asynchronous method
    /// returning an `impl Future` for the caller to await or poll.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// let future_insert = hashmap.insert_async(1, 0);
    /// let future_retain = hashmap.retain_async(|k, v| *k == 1);
    /// ```
    pub async fn retain_async<F: FnMut(&K, &mut V) -> bool>(
        &self,
        mut filter: F,
    ) -> (usize, usize) {
        let mut retained_entries = 0;
        let mut removed_entries = 0;

        // An acquire fence is required to correctly load the contents of the array.
        let mut awaitable_barrier = AwaitableBarrier::default();
        let mut current_array_holder = self.array.get_arc(Acquire, awaitable_barrier.barrier());
        while let Some(current_array) = current_array_holder.take() {
            while !current_array
                .old_array(awaitable_barrier.barrier())
                .is_null()
            {
                if current_array.partial_rehash::<_, _, _, true>(
                    |key| self.hash(key),
                    |_, _| None,
                    awaitable_barrier.barrier(),
                ) {
                    continue;
                }
                awaitable_barrier.drop_barrier_and_yield().await;
            }

            for cell_index in 0..current_array.num_cells() {
                loop {
                    {
                        // Limits the scope of `barrier`.
                        let barrier = awaitable_barrier.barrier();
                        if let Ok(result) =
                            Locker::try_lock(current_array.cell(cell_index), barrier)
                        {
                            if let Some(locker) = result {
                                let mut iterator = locker.cell().iter(barrier);
                                while iterator.next().is_some() {
                                    let retain = if let Some((k, v)) = iterator.get() {
                                        #[allow(clippy::cast_ref_to_mut)]
                                        filter(k, unsafe { &mut *(v as *const V as *mut V) })
                                    } else {
                                        true
                                    };
                                    if retain {
                                        retained_entries += 1;
                                    } else {
                                        locker.erase(&mut iterator);
                                        removed_entries += 1;
                                    }
                                    if retained_entries == usize::MAX
                                        || removed_entries == usize::MAX
                                    {
                                        // Gives up iteration on an integer overflow.
                                        return (retained_entries, removed_entries);
                                    }
                                }
                            }
                            break;
                        }
                    }
                    awaitable_barrier.drop_barrier_and_yield().await;
                }
            }

            if let Some(new_current_array) =
                self.array.get_arc(Acquire, awaitable_barrier.barrier())
            {
                if new_current_array.as_ptr() == current_array.as_ptr() {
                    break;
                }
                retained_entries = 0;
                current_array_holder.replace(new_current_array);
                continue;
            }
            break;
        }

        if removed_entries >= retained_entries {
            self.resize(&Barrier::new());
        }

        (retained_entries, removed_entries)
    }

    /// Clears all the key-value pairs.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(hashmap.insert(1, 0).is_ok());
    /// assert_eq!(hashmap.clear(), 1);
    /// ```
    #[inline]
    pub fn clear(&self) -> usize {
        self.retain(|_, _| false).1
    }

    /// Clears all the key-value pairs.
    ///
    /// It is an asynchronous method returning an `impl Future` for the caller to await or poll.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// let future_insert = hashmap.insert_async(1, 0);
    /// let future_clear = hashmap.clear_async();
    /// ```
    #[inline]
    pub async fn clear_async(&self) -> usize {
        self.retain_async(|_, _| false).await.1
    }

    /// Returns the number of entries in the [`HashMap`].
    ///
    /// It scans the entire array to calculate the number of valid entries, making its time
    /// complexity `O(N)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(hashmap.insert(1, 0).is_ok());
    /// assert_eq!(hashmap.len(), 1);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.num_entries(&Barrier::new())
    }

    /// Returns `true` if the [`HashMap`] is empty.
    ///
    /// It scans the entire array to calculate the number of valid entries, making its time
    /// complexity `O(N)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// assert!(hashmap.is_empty());
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the capacity of the [`HashMap`].
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(1000000, RandomState::new());
    /// assert_eq!(hashmap.capacity(), 1048576);
    /// ```
    #[inline]
    pub fn capacity(&self) -> usize {
        self.num_slots(&Barrier::new())
    }
}

impl<K, V> Default for HashMap<K, V, RandomState>
where
    K: 'static + Eq + Hash + Sync,
    V: 'static + Sync,
{
    /// Creates a [`HashMap`] with the default parameters.
    ///
    /// The default hash builder is [`RandomState`], and the default capacity is `64`.
    ///
    /// # Examples
    ///
    /// ```
    /// use scc::HashMap;
    ///
    /// let hashmap: HashMap<u64, u32> = HashMap::default();
    ///
    /// let result = hashmap.capacity();
    /// assert_eq!(result, 64);
    /// ```
    fn default() -> Self {
        HashMap {
            array: AtomicArc::new(CellArray::<K, V, false>::new(
                Self::default_capacity(),
                AtomicArc::null(),
            )),
            minimum_capacity: Self::default_capacity(),
            additional_capacity: AtomicUsize::new(0),
            resize_mutex: AtomicU8::new(0),
            build_hasher: RandomState::new(),
        }
    }
}

impl<K, V, H> HashTable<K, V, H, false> for HashMap<K, V, H>
where
    K: 'static + Eq + Hash + Sync,
    V: 'static + Sync,
    H: BuildHasher,
{
    fn hasher(&self) -> &H {
        &self.build_hasher
    }
    fn copier(_: &K, _: &V) -> Option<(K, V)> {
        None
    }
    fn cell_array(&self) -> &AtomicArc<CellArray<K, V, false>> {
        &self.array
    }
    fn minimum_capacity(&self) -> usize {
        self.minimum_capacity + self.additional_capacity.load(Relaxed)
    }
    fn resize_mutex(&self) -> &AtomicU8 {
        &self.resize_mutex
    }
}

/// [`Ticket`] keeps the increased minimum capacity of the [`HashMap`] during its lifetime.
///
/// The minimum capacity is lowered when the [`Ticket`] is dropped, thereby allowing unused
/// memory to be reclaimed.
pub struct Ticket<'h, K, V, H>
where
    K: 'static + Eq + Hash + Sync,
    V: 'static + Sync,
    H: BuildHasher,
{
    hash_map: &'h HashMap<K, V, H>,
    increment: usize,
}

impl<'h, K, V, H> Drop for Ticket<'h, K, V, H>
where
    K: 'static + Eq + Hash + Sync,
    V: 'static + Sync,
    H: BuildHasher,
{
    fn drop(&mut self) {
        let result = self
            .hash_map
            .additional_capacity
            .fetch_sub(self.increment, Relaxed);
        self.hash_map.resize(&Barrier::new());
        debug_assert!(result >= self.increment);
    }
}
