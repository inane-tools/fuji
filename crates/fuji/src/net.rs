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
/// Threads for bulk work (indexing a whole library), kept apart so it never
/// holds up what's on screen. iCloud takes about 35 ms per record on index
/// pages, however many run at once, so many in flight is what makes a large
/// library index quickly.
pub const BULK_THREADS: usize = 24;

fn sender() -> &'static Mutex<Sender<Job>> {
    static POOL: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    POOL.get_or_init(|| pool("fuji-net", THREADS))
}

fn bulk_sender() -> &'static Mutex<Sender<Job>> {
    static POOL: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    POOL.get_or_init(|| pool("fuji-bulk", BULK_THREADS))
}

fn pool(name: &str, threads: usize) -> Mutex<Sender<Job>> {
    let (tx, rx) = channel::<Job>();
    let rx = Arc::new(Mutex::new(rx));
    for i in 0..threads {
        let rx = rx.clone();
        std::thread::Builder::new()
            .name(format!("{name}-{i}"))
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
}

/// Run `f` on a network thread; await the result from any executor.
/// `None` if `f` panicked (the thread survives).
pub async fn run<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    run_on(sender(), f).await
}

/// Like [`run`], on the bulk threads: for work nobody is looking at yet.
pub async fn run_bulk<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    run_on(bulk_sender(), f).await
}

async fn run_on<T: Send + 'static>(
    pool: &Mutex<Sender<Job>>,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = oneshot::channel();
    let job: Job = Box::new(move || {
        if let Ok(value) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            let _ = tx.send(value);
        }
    });
    pool.lock().ok()?.send(job).ok()?;
    rx.await.ok()
}
