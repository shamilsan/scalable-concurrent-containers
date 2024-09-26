#![allow(unused_imports)]
pub(crate) use self::inner::*;

#[cfg(all(test, loom))]
mod inner {
    pub(crate) mod atomic {
        pub use loom::sync::atomic::*;
        pub use std::sync::atomic::Ordering;

        // FIXME: loom does not support compiler_fence at the moment.
        // https://github.com/tokio-rs/loom/issues/117
        // we use fence as a stand-in for compiler_fence for the time being.
        // this may miss some races since fence is stronger than compiler_fence,
        // but it's the best we can do for the time being.
        pub(crate) use self::fence as compiler_fence;
    }
    pub(crate) use loom::{
        cell::UnsafeCell, hint, lazy_static, sync::Mutex, thread::yield_now, thread_local,
    };
}

#[cfg(not(all(loom, test)))]
mod inner {
    pub(crate) use std::{
        cell::UnsafeCell,
        sync::{atomic, Mutex},
        thread::yield_now,
        thread_local,
    };
}
