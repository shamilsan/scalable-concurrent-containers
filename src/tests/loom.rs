#[cfg(all(test, loom))]
mod loom_tests {
    use loom::thread;
    use std::sync::atomic::Ordering;

    use crate::ebr::{AtomicShared, Guard, Shared};

    #[test]
    fn guard_works() {
        loom::model(|| {
            let item = AtomicShared::from(Shared::new(String::from("boom")));
            let item2 = <AtomicShared<String> as Clone>::clone(&item);
            let guard = Guard::new();

            let jh = thread::spawn(move || {
                let guard = Guard::new();
                guard.defer_execute(move || {
                    let mut item = item2.into_shared(Ordering::Relaxed).unwrap();
                    unsafe { item.get_mut().unwrap().retain(|c| c == 'o') };
                    drop(item);
                });
            });

            let item = item.load(Ordering::SeqCst, &guard);
            assert_eq!(item.as_ref().unwrap(), "boom");
            drop(guard);

            jh.join().unwrap();
        });
    }

    #[test]
    fn treiber_stack() {}
}
