#![feature(result_into_ok_or_err)]

use std::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    ptr::null_mut,
    sync::atomic::{
        AtomicI32, AtomicPtr,
        Ordering::{Acquire, Relaxed, Release},
    },
};

// The platform's syscalls/instructions:

fn futex_wait(_futex: &AtomicI32, _initial: i32) {
    todo!()
}

fn futex_wake(_futex: &AtomicI32, _n: i32) {
    todo!()
}

/// Wake one thread on the first futex, and requeue the other waiters to the second futex.
fn futex_wake_and_requeue(_futex: *const AtomicI32, _futex2: *const AtomicI32) {
    todo!()
}

// Mutex:

pub struct Mutex<T: ?Sized> {
    /// 0: unlocked
    /// 1: locked, no other threads waiting
    /// 2: locked, and other threads waiting (contended)
    futex: AtomicI32,
    value: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}

pub struct MutexGuard<'a, T: ?Sized + 'a> {
    mutex: &'a Mutex<T>,
}

unsafe impl<T: ?Sized + Sync> Sync for MutexGuard<'_, T> {}
unsafe impl<T: ?Sized + Send> Send for MutexGuard<'_, T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            futex: AtomicI32::new(0),
            value: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn try_lock(&self) -> Option<MutexGuard<T>> {
        self.futex
            .compare_exchange(0, 1, Acquire, Relaxed)
            .is_ok()
            .then(|| MutexGuard { mutex: self })
    }

    pub fn lock(&self) -> MutexGuard<T> {
        if self.futex.compare_exchange(0, 1, Acquire, Relaxed).is_err() {
            lock_contended(&self.futex);
        }
        MutexGuard { mutex: self }
    }
}

fn lock_contended(futex: &AtomicI32) {
    loop {
        // Put the lock in contended state, if it wasn't already.
        if futex.swap(2, Acquire) == 0 {
            // It was unlocked, so we just locked it.
            return;
        }
        // Wait for the futex to change state.
        futex_wait(futex, 2);
    }
}

impl<'a, T: ?Sized> Drop for MutexGuard<'a, T> {
    fn drop(&mut self) {
        if self.mutex.futex.swap(0, Release) == 2 {
            // We only wake up one thread. When that thread locks the mutex, it
            // will mark the mutex as contended (2) (see lock_contended above),
            // which makes sure that any other waiting threads will also be
            // woken up eventually.
            futex_wake(&self.mutex.futex, 1);
        }
    }
}

impl<'a, T: ?Sized> Deref for MutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<'a, T: ?Sized> DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}

// Condition variable, version #1:

// This version doesn't keep track of the mutex that threads
// are waiting for, and doesn't make use of requeue'ing.
// It just wakes all threads on notify_all().
//
// This is the best we can do on platforms that don't have
// a requeue operation, such as Wasm.
pub struct Condvar1 {
    // The value of this atomic is simply incremented on every notification.
    // This is used by `.wait()` to not miss any notifications after
    // unlocking the mutex and before waiting for notifications.
    futex: AtomicI32,
}

impl Condvar1 {
    pub const fn new() -> Self {
        Self {
            futex: AtomicI32::new(0),
        }
    }

    // All the memory orderings here are `Relaxed`,
    // because synchronization is done by unlocking and locking the mutex.

    pub fn notify_one(&self) {
        self.futex.fetch_add(1, Relaxed);
        futex_wake(&self.futex, 1);
    }

    pub fn notify_all(&self) {
        self.futex.fetch_add(1, Relaxed);
        futex_wake(&self.futex, i32::MAX);
    }

    pub fn wait<'a, T: ?Sized>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        let mutex = guard.mutex;

        // Check the notification counter before we unlock the mutex.
        let futex_value = self.futex.load(Relaxed);

        // Unlock the mutex.
        drop(guard);

        // Wait, but only if there hasn't been any
        // notification since we unlocked the mutex.
        futex_wait(&self.futex, futex_value);

        // Lock the mutex again.
        mutex.lock()
    }
}

// Condition variable, version #2:

// This version keeps track of the mutex that waiters are waiting for,
// such that .notify_all() only needs to wake one waiter, and requeue all
// the other waiters to the mutex.
//
// This might actually be much less performant depending on the program and
// kind of system it runs on.
pub struct Condvar2 {
    futex: AtomicI32,
    mutex: AtomicPtr<AtomicI32>,
}

impl Condvar2 {
    pub const fn new() -> Self {
        Self {
            futex: AtomicI32::new(0),
            mutex: AtomicPtr::new(null_mut()),
        }
    }

    pub fn notify_one(&self) {
        self.futex.fetch_add(1, Relaxed);
        futex_wake(&self.futex, 1);
    }

    pub fn notify_all(&self) {
        self.futex.fetch_add(1, Relaxed);
        // The mutex might be null or dangling at this point. But we don't care.
        // If that's the case, the syscall might return an error, which we ignore.
        let mutex = self.mutex.load(Relaxed);
        futex_wake_and_requeue(&self.futex, mutex);
    }

    pub fn wait<'a, T: ?Sized>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        let mutex = guard.mutex;

        // Store the mutex address, to allow notify_all to
        // requeue waiters to the queue the mutex.
        self.mutex.store(&mutex.futex as *const _ as _, Relaxed);

        // Check the notification counter before we unlock the mutex.
        let futex_value = self.futex.load(Relaxed);

        // Unlock the mutex.
        drop(guard);

        // Wait, but only if there hasn't been any
        // notification since we unlocked the mutex.
        futex_wait(&self.futex, futex_value);

        // Lock the mutex again, and put it in the
        // 'other threads might be waiting waiting' state,
        // because other threads might have been requeued to it.
        lock_contended(&mutex.futex);
        MutexGuard { mutex }
    }
}
