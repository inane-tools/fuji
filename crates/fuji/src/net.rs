//! A small thread pool for blocking network calls.
//!
//! The iCloud client uses blocking HTTP. Running that on GPUI's background
//! executor would tie up the threads it also uses to decode images, so
//! network work gets its own threads.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

use futures::channel::oneshot;

type Job = Box<dyn FnOnce() + Send>;

const THREADS: usize = 8;

fn sender() -> &'static Mutex<Sender<Job>> {
    static POOL: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..THREADS {
            let rx = rx.clone();
            std::thread::Builder::new()
                .name(format!("fuji-net-{i}"))
                .spawn(move || {
                    loop {
                        let job = match rx.lock() {
                            Ok(rx) => rx.recv(),
                            Err(_) => return,
                        };
                        match job {
                            Ok(job) => job(),
                            Err(_) => return,
                        }
                    }
                })
                .expect("spawn network thread");
        }
        Mutex::new(tx)
    })
}

/// Run `f` on a network thread; await the result from any executor.
/// `None` if `f` panicked (the thread survives).
pub async fn run<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = oneshot::channel();
    let job: Job = Box::new(move || {
        if let Ok(value) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            let _ = tx.send(value);
        }
    });
    sender().lock().ok()?.send(job).ok()?;
    rx.await.ok()
}
