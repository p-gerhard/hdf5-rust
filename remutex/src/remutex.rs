// Copyright 2015 The Rust Project Developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::fmt;
use std::marker;
use std::ops::Deref;

use crate::sys;
use crate::poison::{self, TryLockError, TryLockResult, LockResult};

/// A re-entrant mutual exclusion
///
/// This mutex will block *other* threads waiting for the lock to become available. The thread
/// which has already locked the mutex can lock it multiple times without blocking, preventing a
/// common source of deadlocks.
pub struct ReentrantMutex<T> {
    inner: Box<sys::ReentrantMutex>,
    poison: poison::Flag,
    data: T,
}

unsafe impl<T: Send> Send for ReentrantMutex<T> {}
unsafe impl<T: Send> Sync for ReentrantMutex<T> {}

#[must_use]
pub struct ReentrantMutexGuard<'a, T: 'a> {
    __lock: &'a ReentrantMutex<T>,
    __poison: poison::Guard,
    __marker: marker::PhantomData<*mut ()>,  // !Send
}

impl<T> ReentrantMutex<T> {
    /// Creates a new reentrant mutex in an unlocked state.
    pub fn new(t: T) -> ReentrantMutex<T> {
        unsafe {
            let mut mutex = ReentrantMutex {
                inner: Box::new(sys::ReentrantMutex::uninitialized()),
                poison: poison::Flag::new(),
                data: t,
            };
            mutex.inner.init();
            mutex
        }
    }

    /// Acquires a mutex, blocking the current thread until it is able to do so.
    ///
    /// This function will block the caller until it is available to acquire the mutex.
    /// Upon returning, the thread is the only thread with the mutex held. When the thread
    /// calling this method already holds the lock, the call shall succeed without
    /// blocking.
    ///
    /// # Failure
    ///
    /// If another user of this mutex panicked while holding the mutex, then
    /// this call will return failure if the mutex would otherwise be
    /// acquired.
    pub fn lock(&self) -> LockResult<ReentrantMutexGuard<T>> {
        unsafe { self.inner.lock() }
        ReentrantMutexGuard::new(&self)
    }

    /// Attempts to acquire this lock.
    ///
    /// If the lock could not be acquired at this time, then `Err` is returned.
    /// Otherwise, an RAII guard is returned.
    ///
    /// This function does not block.
    ///
    /// # Failure
    ///
    /// If another user of this mutex panicked while holding the mutex, then
    /// this call will return failure if the mutex would otherwise be
    /// acquired.
    pub fn try_lock(&self) -> TryLockResult<ReentrantMutexGuard<T>> {
        if unsafe { self.inner.try_lock() } {
            Ok(ReentrantMutexGuard::new(&self)?)
        } else {
            Err(TryLockError::WouldBlock)
        }
    }
}

impl<T> Drop for ReentrantMutex<T> {
    fn drop(&mut self) {
        unsafe { self.inner.destroy() }
    }
}

impl<T: fmt::Debug + 'static> fmt::Debug for ReentrantMutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.try_lock() {
            Ok(guard) => write!(f, "ReentrantMutex {{ data: {:?} }}", &*guard),
            Err(TryLockError::Poisoned(err)) => {
                write!(f, "ReentrantMutex {{ data: Poisoned({:?}) }}", &**err.get_ref())
            },
            Err(TryLockError::WouldBlock) => write!(f, "ReentrantMutex {{ <locked> }}")
        }
    }
}

impl<'mutex, T> ReentrantMutexGuard<'mutex, T> {
    fn new(lock: &'mutex ReentrantMutex<T>)
            -> LockResult<ReentrantMutexGuard<'mutex, T>> {
        poison::map_result(lock.poison.borrow(), |guard| {
            ReentrantMutexGuard {
                __lock: lock,
                __poison: guard,
                __marker: marker::PhantomData,
            }
        })
    }
}

impl<'mutex, T> Deref for ReentrantMutexGuard<'mutex, T> {
    type Target = T;

    fn deref<'a>(&'a self) -> &'a T {
        &self.__lock.data
    }
}

impl<'a, T> Drop for ReentrantMutexGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            self.__lock.poison.done(&self.__poison);
            self.__lock.inner.unlock();
        }
    }
}


#[cfg(test)]
mod test {
    use super::{ReentrantMutex, ReentrantMutexGuard};
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn smoke() {
        let m = ReentrantMutex::new(());
        {
            let a = m.lock().unwrap();
            {
                let b = m.lock().unwrap();
                {
                    let c = m.lock().unwrap();
                    assert_eq!(*c, ());
                }
                assert_eq!(*b, ());
            }
            assert_eq!(*a, ());
        }
    }

    #[test]
    fn is_mutex() {
        let m = Arc::new(ReentrantMutex::new(RefCell::new(0)));
        let lock = m.lock().unwrap();
        {
            let mc = m.clone();
            let handle = thread::spawn(move || {
                let lock = mc.lock().unwrap();
                assert_eq!(*lock.borrow(), 4950);
            });
            for i in 0..100 {
                let mut lock = m.lock().unwrap();
                *lock.borrow_mut() += i;
            }
            drop(lock);
            drop(handle);
        }
    }

    #[test]
    #[allow(unused_must_use)]
    fn trylock_works() {
        let m = Arc::new(ReentrantMutex::new(()));
        let _lock1 = m.try_lock().unwrap();
        let _lock2 = m.try_lock().unwrap();
        {
            let m = m.clone();
            thread::spawn(move || {
                let lock = m.try_lock();
                assert!(lock.is_err());
            }).join();
        }
        let _lock3 = m.try_lock().unwrap();
    }

    pub struct Answer<'a>(pub ReentrantMutexGuard<'a, RefCell<u32>>);
    impl<'a> Drop for Answer<'a> {
        fn drop(&mut self) {
            *self.0.borrow_mut() = 42;
        }
    }

    #[test]
    fn poison_works() {
        let m = Arc::new(ReentrantMutex::new(RefCell::new(0)));
        {
            let mc = m.clone();
            let _result = thread::spawn(move ||{
                let lock = mc.lock().unwrap();
                *lock.borrow_mut() = 1;
                let lock2 = mc.lock().unwrap();
                *lock.borrow_mut() = 2;
                let _answer = Answer(lock2);
                panic!("What the answer to my lifetimes dilemma is?");
                drop(_answer);
            }).join();
        }
        let r = m.lock().err().unwrap().into_inner();
        assert_eq!(*r.borrow(), 42);
    }
}
