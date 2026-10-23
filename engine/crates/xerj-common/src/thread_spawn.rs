//! Spawning helper threads without taking the node down (#1122).
//!
//! The shipped release profile is `panic = "abort"`, so a panic on any thread
//! is a whole-node outage. Two different failures look the same from the
//! outside:
//!
//! * `std::thread::spawn` is `Builder::spawn(..).expect(..)`. An `EAGAIN`
//!   from `pthread_create` (thread or memory limit) aborts the process.
//!   `Builder::spawn` itself returns that `Err`.
//! * On Unix, `Builder::spawn` then **panics inside the new thread** when it
//!   cannot mmap the alternate signal stack (`failed to allocate an
//!   alternative stack: os error 12`). The parent already observed `Ok`.
//!   With `panic = "abort"` that child panic is not catchable.
//!
//! [`StdSpawn`] does not use `std::thread::Builder` on Unix. It calls
//! `pthread_create` directly and runs the job on that thread, so a stack
//! that cannot be reserved comes back as [`SpawnFailure`] and the child
//! never executes std's signal-stack setup. Windows has no alternate-stack
//! mmap; `CreateThread` failure is already an `Err` from `Builder::spawn`.
//! The global allocator aborting on its own (`memory allocation of N bytes
//! failed`) is a different failure and is not what this module handles.
//!
//! Because `Builder::spawn` / a failed `pthread_create` drops the closure,
//! the closure is handed *back* in [`SpawnFailure::job`] so the caller can
//! run it inline, retry, or drop it. Dropping it is the right degrade for
//! an infinite worker loop; running it is the right degrade for a finite
//! job such as freeing a memtable generation.
//!
//! [`try_spawn`] also consults a process-wide spawn gate (the engine
//! installs its governor's RSS-watermark check through
//! [`install_spawn_gate`]). That gate is a backstop: once the breaker is
//! already engaged, a best-effort spawn is not attempted at all. It is not
//! a substitute for the non-panicking spawn. Threads that must be tried
//! even above the watermark (the governor itself, WAL fsync) use
//! [`try_spawn_unguarded`] and still get an `Err` instead of an abort.
//!
//! The actual spawn goes through the [`Spawn`] trait so the failure paths
//! are unit-testable without exhausting a real machine.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// A process-wide "is it safe to spawn a thread right now?" predicate.
pub type SpawnGate = fn() -> bool;

static GATE: OnceLock<SpawnGate> = OnceLock::new();

/// Install the process-wide spawn gate. The first installation wins (the
/// engine's governor is process-global too); returns whether this call
/// installed it.
pub fn install_spawn_gate(gate: SpawnGate) -> bool {
    GATE.set(gate).is_ok()
}

/// Whether the installed gate currently allows spawning. With no gate
/// installed (unit tests, library embedding, the CLI) spawning is
/// unrestricted.
pub fn spawn_allowed() -> bool {
    GATE.get().map(|gate| gate()).unwrap_or(true)
}

/// The boxed body handed to a [`Spawn`] implementation.
pub type BoxedJob = Box<dyn FnOnce() + Send + 'static>;

/// How a named thread is actually started. [`StdSpawn`] in production; tests
/// inject a spawner that fails.
pub trait Spawn {
    /// Start `job` on a new thread called `name`.
    ///
    /// Failure to create the thread, including a failure while reserving its
    /// stack, is an `Err`. This must not panic: with `panic = "abort"` a
    /// panic here, or in the new thread before `job` runs, kills the process.
    fn spawn(&self, name: &str, job: BoxedJob) -> io::Result<SpawnedThread>;
}

/// `pthread_create` on Unix, `std::thread::Builder` elsewhere.
///
/// Unix does not go through `std::thread`. That path panics in the child
/// when the alternate signal stack cannot be mapped, after the parent has
/// already been told the thread exists (#1122).
#[derive(Debug, Clone, Copy, Default)]
pub struct StdSpawn;

impl Spawn for StdSpawn {
    fn spawn(&self, name: &str, job: BoxedJob) -> io::Result<SpawnedThread> {
        #[cfg(unix)]
        {
            // The OS name is applied on the new thread by the wrapper
            // `try_spawn_with` builds, before `job` runs. `std::thread`
            // would do that itself and then panic if the signal stack
            // cannot be mapped.
            let _ = name;
            spawn_pthread(job)
        }
        #[cfg(not(unix))]
        {
            spawn_std(name, job)
        }
    }
}

/// A thread started by [`Spawn::spawn`]. Dropping it detaches; [`join`](Self::join)
/// waits. There is no `std::thread::JoinHandle` on Unix because that type
/// only exists for threads std started, and those are the ones that panic
/// while mapping a signal stack.
pub struct SpawnedThread {
    inner: SpawnedInner,
}

impl std::fmt::Debug for SpawnedThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpawnedThread { .. }")
    }
}

enum SpawnedInner {
    #[cfg(unix)]
    Raw {
        thread: libc::pthread_t,
        joined: bool,
    },
    #[cfg(not(unix))]
    Std(Option<std::thread::JoinHandle<()>>),
    /// Spawn was refused or failed and the caller still needs a handle that
    /// `join`s immediately (a background loop that never started).
    Inert,
}

impl SpawnedThread {
    /// A handle for a thread that was never started. [`join`](Self::join)
    /// returns immediately.
    pub fn inert() -> Self {
        Self {
            inner: SpawnedInner::Inert,
        }
    }

    /// Wait for the thread to finish.
    ///
    /// A panic inside the job aborts the process on the release profile
    /// (`panic = "abort"`). On unwind builds the panic is caught at the
    /// `extern "C"` boundary so it cannot unwind into C; `join` then
    /// returns `Ok` and the panic hook has already printed it. Callers
    /// that detach (the normal best-effort path) do not observe it.
    pub fn join(mut self) -> std::thread::Result<()> {
        self.join_inner()
    }

    fn join_inner(&mut self) -> std::thread::Result<()> {
        match &mut self.inner {
            #[cfg(unix)]
            SpawnedInner::Raw { thread, joined } => {
                if !*joined {
                    *joined = true;
                    // SAFETY: `thread` came from a successful pthread_create
                    // in this process, and this is the only join or detach.
                    let rc = unsafe { libc::pthread_join(*thread, std::ptr::null_mut()) };
                    if rc != 0 {
                        return Err(Box::new(io::Error::from_raw_os_error(rc)));
                    }
                }
                Ok(())
            }
            #[cfg(not(unix))]
            SpawnedInner::Std(handle) => match handle.take() {
                Some(handle) => handle.join(),
                None => Ok(()),
            },
            SpawnedInner::Inert => Ok(()),
        }
    }
}

impl Drop for SpawnedThread {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let SpawnedInner::Raw { thread, joined } = &mut self.inner {
            if !*joined {
                *joined = true;
                // SAFETY: `thread` came from a successful pthread_create in
                // this process, and join has not been called. Detach is what
                // dropping a `JoinHandle` does for a std thread.
                unsafe {
                    libc::pthread_detach(*thread);
                }
            }
        }
    }
}

/// A spawn that did not happen. `job` is the closure that would have run on
/// the thread, returned to the caller so it can degrade (run it inline),
/// retry, or drop it on purpose. It is `None` only if the spawner reported a
/// failure *after* running the job, which [`StdSpawn`] never does.
pub struct SpawnFailure<F> {
    /// Why the thread was not started.
    pub error: io::Error,
    /// The un-run job, handed back.
    pub job: Option<F>,
}

impl<F> std::fmt::Debug for SpawnFailure<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnFailure")
            .field("error", &self.error)
            .field("job_returned", &self.job.is_some())
            .finish()
    }
}

impl<F> std::fmt::Display for SpawnFailure<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

/// Log one degradation for `once`. Hot paths (every memtable publish, every
/// pool call) must not emit a line per failed attempt. Returns whether this
/// call logged.
pub fn warn_best_effort_degraded(once: &AtomicBool, name: &str, error: &io::Error) -> bool {
    if once.swap(true, Ordering::Relaxed) {
        return false;
    }
    tracing::warn!(
        thread = name,
        error = %error,
        "best-effort helper thread was not started; degrading instead of aborting the process (#1122)"
    );
    true
}

/// The error a gate refusal carries.
fn gate_refused(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::OutOfMemory,
        format!(
            "not spawning thread {name}: memory is at the governor watermark, \
             so this best-effort spawn is skipped (#1122)"
        ),
    )
}

/// Spawn a named thread, honouring the process-wide spawn gate. Never
/// panics on spawn failure; see [`SpawnFailure`].
pub fn try_spawn<F>(name: &str, f: F) -> Result<SpawnedThread, SpawnFailure<F>>
where
    F: FnOnce() + Send + 'static,
{
    try_spawn_with(&StdSpawn, spawn_allowed(), name, f)
}

/// [`try_spawn`] without the watermark gate.
///
/// For threads that should still be attempted when RSS is already high
/// (the governor, a WAL fsync worker). A real OS failure is still an
/// `Err` with the job handed back — it does not abort.
pub fn try_spawn_unguarded<F>(name: &str, f: F) -> Result<SpawnedThread, SpawnFailure<F>>
where
    F: FnOnce() + Send + 'static,
{
    try_spawn_with(&StdSpawn, true, name, f)
}

/// [`try_spawn`] with an explicit spawner and gate verdict (for tests and
/// callers that already sampled the gate).
pub fn try_spawn_with<S, F>(
    spawner: &S,
    allowed: bool,
    name: &str,
    f: F,
) -> Result<SpawnedThread, SpawnFailure<F>>
where
    S: Spawn + ?Sized,
    F: FnOnce() + Send + 'static,
{
    if !allowed {
        return Err(SpawnFailure {
            error: gate_refused(name),
            job: Some(f),
        });
    }
    // The thread takes the job out of the slot when it runs. If the spawn
    // fails, the thread never ran, and the job is still in the slot even
    // though the spawner dropped the boxed wrapper.
    let slot = Arc::new(Mutex::new(Some(f)));
    let thread_slot = Arc::clone(&slot);
    #[cfg(unix)]
    let comm = os_comm_name(name);
    let wrapper: BoxedJob = Box::new(move || {
        // Before the caller's job, so a panic in the job still shows the
        // OS thread name. std would have set this and then mmap'd a signal
        // stack; that mmap is the panic #1122 hit.
        #[cfg(unix)]
        set_os_name(&comm);
        let job = thread_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(job) = job {
            job();
        }
    });
    match spawner.spawn(name, wrapper) {
        Ok(handle) => Ok(handle),
        Err(error) => {
            let job = slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            Err(SpawnFailure { error, job })
        }
    }
}

/// [`try_spawn`] retried up to `attempts` times (at least once), sleeping
/// `backoff`, then twice that, … between attempts. For threads that must
/// exist (samplers, workers) where a transient `EAGAIN` or a brief trip of
/// the memory watermark should not cost the thread for the life of the
/// process. The gate is re-sampled on every attempt.
pub fn spawn_with_retry<F>(
    name: &str,
    attempts: u32,
    backoff: Duration,
    f: F,
) -> Result<SpawnedThread, SpawnFailure<F>>
where
    F: FnOnce() + Send + 'static,
{
    spawn_with_retry_using(&StdSpawn, spawn_allowed, name, attempts, backoff, f)
}

/// [`spawn_with_retry`] with an explicit spawner and gate.
pub fn spawn_with_retry_using<S, G, F>(
    spawner: &S,
    gate: G,
    name: &str,
    attempts: u32,
    backoff: Duration,
    f: F,
) -> Result<SpawnedThread, SpawnFailure<F>>
where
    S: Spawn + ?Sized,
    G: Fn() -> bool,
    F: FnOnce() + Send + 'static,
{
    let attempts = attempts.max(1);
    let mut job = f;
    let mut delay = backoff;
    let mut attempt = 1;
    loop {
        match try_spawn_with(spawner, gate(), name, job) {
            Ok(handle) => return Ok(handle),
            Err(failure) => {
                let Some(returned) = failure.job else {
                    return Err(failure);
                };
                if attempt >= attempts {
                    return Err(SpawnFailure {
                        error: failure.error,
                        job: Some(returned),
                    });
                }
                job = returned;
                std::thread::sleep(delay);
                delay = delay.saturating_mul(2);
                attempt += 1;
            }
        }
    }
}

/// Linux `TASK_COMM_LEN`, including the trailing NUL. `std::thread` truncates
/// to the same width for the OS name; the full name still goes in our logs.
#[cfg(unix)]
fn os_comm_name(name: &str) -> [u8; 16] {
    let mut buf = [0u8; 16];
    let bytes = if name.is_empty() {
        b"xerj".as_slice()
    } else {
        name.as_bytes()
    };
    let n = bytes.len().min(15);
    buf[..n].copy_from_slice(&bytes[..n]);
    buf
}

#[cfg(unix)]
fn set_os_name(bytes: &[u8; 16]) {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        // SAFETY: `bytes` is NUL-terminated (index 15 is never written) and
        // the C string is only read for the duration of the setname call.
        let name = unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr().cast()) };
        #[cfg(any(target_os = "linux", target_os = "android"))]
        unsafe {
            // SAFETY: `name` is a NUL-terminated C string. Setting the
            // calling thread's name cannot fail the spawn.
            let _ = libc::pthread_setname_np(libc::pthread_self(), name.as_ptr());
        }
        #[cfg(target_os = "macos")]
        unsafe {
            // SAFETY: same as Linux, but Darwin's setter only names the
            // calling thread and takes no `pthread_t`.
            let _ = libc::pthread_setname_np(name.as_ptr());
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = bytes;
    }
}

/// Same default std uses for a spawned thread (`2 MiB` on Tier-1). A larger
/// stack would reserve more address space than the path this replaces,
/// which is the opposite of what a memory-pressure spawn should do.
#[cfg(unix)]
const HELPER_STACK_BYTES: usize = 2 * 1024 * 1024;

#[cfg(unix)]
struct AttrGuard(libc::pthread_attr_t);

#[cfg(unix)]
impl Drop for AttrGuard {
    fn drop(&mut self) {
        // SAFETY: the guard is built only after `pthread_attr_init` returned
        // 0, and nothing else destroys this attribute.
        unsafe {
            libc::pthread_attr_destroy(&mut self.0);
        }
    }
}

#[cfg(unix)]
fn spawn_pthread(job: BoxedJob) -> io::Result<SpawnedThread> {
    // A second box so the fat pointer of `dyn FnOnce` survives the trip
    // through `pthread_create`'s thin `void*`. This is the same shape std
    // uses. The allocation is the caller's job box plus this one; if it
    // fails, the global allocator aborts — that is every Rust allocation,
    // not the signal-stack panic this function exists to avoid.
    let job = Box::new(job);
    let job_ptr = Box::into_raw(job);

    let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
    // SAFETY: `attr` is a valid out-pointer for pthread_attr_init.
    let rc = unsafe { libc::pthread_attr_init(attr.as_mut_ptr()) };
    if rc != 0 {
        // SAFETY: the new thread was not created, so it did not take the box.
        drop(unsafe { Box::from_raw(job_ptr) });
        return Err(io::Error::from_raw_os_error(rc));
    }
    // SAFETY: pthread_attr_init returned 0, so the attribute is initialized.
    let mut attr = AttrGuard(unsafe { attr.assume_init() });
    // SAFETY: `attr` is initialized. Failure leaves the platform default
    // stack; pthread_create still returns an error if that cannot be reserved.
    unsafe {
        let _ = libc::pthread_attr_setstacksize(&mut attr.0, HELPER_STACK_BYTES);
    }

    let mut thread = std::mem::MaybeUninit::<libc::pthread_t>::uninit();
    // SAFETY: `attr` is initialized; `thread_main` has the C ABI and frees
    // `job_ptr` exactly once. On failure the thread did not start and the
    // pointer is freed below.
    let rc = unsafe {
        libc::pthread_create(
            thread.as_mut_ptr(),
            &attr.0,
            thread_main,
            job_ptr.cast::<libc::c_void>(),
        )
    };
    drop(attr);
    if rc != 0 {
        // SAFETY: pthread_create failed, so `thread_main` will not run.
        drop(unsafe { Box::from_raw(job_ptr) });
        return Err(io::Error::from_raw_os_error(rc));
    }
    // SAFETY: pthread_create returned 0, so `thread` was written.
    let thread = unsafe { thread.assume_init() };
    Ok(SpawnedThread {
        inner: SpawnedInner::Raw {
            thread,
            joined: false,
        },
    })
}

#[cfg(unix)]
extern "C" fn thread_main(ptr: *mut libc::c_void) -> *mut libc::c_void {
    // SAFETY: the parent leaked this box into pthread_create, and this
    // thread is the unique consumer. The closure is `Send`.
    let job = unsafe { Box::from_raw(ptr.cast::<BoxedJob>()) };
    run_job(*job);
    std::ptr::null_mut()
}

#[cfg(unix)]
fn run_job(job: BoxedJob) {
    // Unwinding out of `extern "C"` is undefined. Release builds abort
    // inside the job (`panic = "abort"`) and never reach the catch.
    // Unwind builds (tests) catch so a panicked helper cannot cross the
    // C boundary; the panic hook has already printed.
    #[cfg(panic = "unwind")]
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
    #[cfg(not(panic = "unwind"))]
    job();
}

#[cfg(not(unix))]
fn spawn_std(name: &str, job: BoxedJob) -> io::Result<SpawnedThread> {
    // Windows (and any other non-Unix target) has no sigaltstack mmap.
    // `CreateThread` failure is an `Err` from `Builder::spawn`, which is
    // the degrade path. A global-allocator abort is out of scope here.
    let handle = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(job)?;
    Ok(SpawnedThread {
        inner: SpawnedInner::Std(Some(handle)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize};

    /// Fails the first `fail_first` spawns with EAGAIN, then spawns for real.
    struct FlakySpawn {
        fail_first: u32,
        calls: AtomicU32,
    }

    impl FlakySpawn {
        fn new(fail_first: u32) -> Self {
            Self {
                fail_first,
                calls: AtomicU32::new(0),
            }
        }
    }

    impl Spawn for FlakySpawn {
        fn spawn(&self, name: &str, job: BoxedJob) -> io::Result<SpawnedThread> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.fail_first {
                drop(job); // exactly what Builder::spawn does on failure
                return Err(io::Error::from_raw_os_error(libc::EAGAIN));
            }
            StdSpawn.spawn(name, job)
        }
    }

    /// Models the failure std reports only by panicking in the child:
    /// the alternate-signal-stack mmap returns ENOMEM (os error 12).
    /// The production spawner must surface that as `Err` with the job
    /// handed back, so the caller can drop it inline and keep running.
    struct SigaltstackEnomem;

    impl Spawn for SigaltstackEnomem {
        fn spawn(&self, _name: &str, job: BoxedJob) -> io::Result<SpawnedThread> {
            drop(job);
            Err(io::Error::from_raw_os_error(12))
        }
    }

    #[test]
    fn a_failed_spawn_returns_the_job_instead_of_panicking() {
        let ran = Arc::new(AtomicUsize::new(0));
        let job_ran = Arc::clone(&ran);
        let failure = try_spawn_with(&FlakySpawn::new(1), true, "t1122", move || {
            job_ran.fetch_add(1, Ordering::SeqCst);
        })
        .expect_err("the spawner fails");
        assert_eq!(failure.error.raw_os_error(), Some(libc::EAGAIN));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "nothing ran on a thread");
        // The caller can degrade by running the job inline.
        (failure.job.expect("job handed back"))();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_enomem_setup_failure_returns_the_job_instead_of_panicking() {
        let ran = Arc::new(AtomicUsize::new(0));
        let job_ran = Arc::clone(&ran);
        let failure = try_spawn_with(&SigaltstackEnomem, true, "xerj-drain-free", move || {
            job_ran.fetch_add(1, Ordering::SeqCst);
        })
        .expect_err("setup failure is an Err, not a panic");
        assert_eq!(failure.error.raw_os_error(), Some(12));
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "the job did not run on a thread"
        );
        // Drain-free drops the job (the capture's destructors free the
        // memtable). Pools run it inline. Either degrade keeps the process up.
        (failure.job.expect("job handed back"))();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_closed_gate_refuses_without_calling_the_spawner() {
        let spawner = FlakySpawn::new(0);
        let failure = try_spawn_with(&spawner, false, "t1122", || {}).expect_err("gate closed");
        assert_eq!(failure.error.kind(), io::ErrorKind::OutOfMemory);
        assert!(failure.job.is_some());
        assert_eq!(spawner.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_successful_spawn_runs_the_job_once_on_the_named_thread() {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = try_spawn_with(&StdSpawn, true, "xerj-t1122", move || {
            tx.send(helper_thread_name()).unwrap();
        })
        .expect("spawn");
        handle.join().unwrap();
        assert_eq!(rx.recv().unwrap().as_deref(), Some("xerj-t1122"));
        assert!(rx.try_recv().is_err(), "ran exactly once");
    }

    #[cfg(unix)]
    #[test]
    fn a_spawned_helper_does_not_install_an_alternate_signal_stack() {
        // #1122: std::thread::Builder's child calls
        // `stack_overflow::Handler::new`, which mmap's an alternate signal
        // stack and panics on ENOMEM. The parent has already been returned
        // `Ok`, and `panic = "abort"` turns that into SIGABRT. A helper
        // thread must not install that stack, so the failure mode cannot
        // happen. A std thread in this process does install one (the test
        // binary left SIGSEGV at SIG_DFL, so std's handler is active); this
        // assertion is false for `Builder::spawn`.
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = try_spawn_with(&StdSpawn, true, "xerj-drain-free", move || {
            tx.send(alternate_signal_stack_flags()).unwrap();
        })
        .expect("spawn");
        handle.join().unwrap();
        let (rc, flags) = rx.recv().unwrap();
        assert_eq!(rc, 0, "sigaltstack query failed");
        assert_ne!(
            flags & libc::SS_DISABLE,
            0,
            "helper installed an alternate signal stack; an ENOMEM mmap of it \
             panics inside the child and aborts the process (#1122)"
        );

        // The same process's std threads do install one. If they did not,
        // this test would pass for `Builder::spawn` too and would not be a
        // regression test for the panic.
        let (std_rc, std_flags) = std::thread::spawn(alternate_signal_stack_flags)
            .join()
            .unwrap();
        assert_eq!(std_rc, 0, "sigaltstack query failed on a std thread");
        assert_eq!(
            std_flags & libc::SS_DISABLE,
            0,
            "std::thread did not install an alternate signal stack in this \
             process, so the assertion above cannot tell the two spawners apart"
        );
    }

    #[test]
    fn dropping_the_handle_detaches_and_the_job_still_runs() {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = try_spawn_with(&StdSpawn, true, "xerj-t1122", move || {
            tx.send(()).unwrap();
        })
        .expect("spawn");
        drop(handle);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("detached thread ran the job");
    }

    #[test]
    fn degradation_is_logged_once_per_flag() {
        let once = AtomicBool::new(false);
        let error = io::Error::from_raw_os_error(12);
        assert!(warn_best_effort_degraded(&once, "xerj-drain-free", &error));
        assert!(!warn_best_effort_degraded(&once, "xerj-drain-free", &error));
    }

    #[test]
    fn retry_rides_out_transient_failures() {
        let spawner = FlakySpawn::new(2);
        let ran = Arc::new(AtomicUsize::new(0));
        let job_ran = Arc::clone(&ran);
        let handle = spawn_with_retry_using(
            &spawner,
            || true,
            "t1122",
            3,
            Duration::from_millis(1),
            move || {
                job_ran.fetch_add(1, Ordering::SeqCst);
            },
        )
        .expect("third attempt succeeds");
        handle.join().unwrap();
        assert_eq!(spawner.calls.load(Ordering::SeqCst), 3);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retry_gives_up_after_the_attempt_budget_and_hands_the_job_back() {
        let spawner = FlakySpawn::new(u32::MAX);
        let failure = spawn_with_retry_using(
            &spawner,
            || true,
            "t1122",
            3,
            Duration::from_millis(1),
            || {},
        )
        .expect_err("never succeeds");
        assert_eq!(spawner.calls.load(Ordering::SeqCst), 3);
        assert!(failure.job.is_some());
    }

    #[test]
    fn retry_resamples_the_gate_each_attempt() {
        let gate_calls = AtomicU32::new(0);
        let spawner = FlakySpawn::new(0);
        let handle = spawn_with_retry_using(
            &spawner,
            || gate_calls.fetch_add(1, Ordering::SeqCst) >= 1,
            "t1122",
            2,
            Duration::from_millis(1),
            || {},
        )
        .expect("gate opens on the second attempt");
        handle.join().unwrap();
        assert_eq!(gate_calls.load(Ordering::SeqCst), 2);
        assert_eq!(spawner.calls.load(Ordering::SeqCst), 1);
    }

    /// `(rc, ss_flags)` from `sigaltstack`. `SS_DISABLE` set means the
    /// thread has no alternate signal stack — the mapping `std::thread`
    /// panics on if it cannot be created.
    #[cfg(unix)]
    fn alternate_signal_stack_flags() -> (libc::c_int, libc::c_int) {
        let mut stack = unsafe { std::mem::zeroed::<libc::stack_t>() };
        // SAFETY: a null current-stack pointer only reads the calling
        // thread's alternate signal stack into `stack`.
        let rc = unsafe { libc::sigaltstack(std::ptr::null(), &mut stack) };
        (rc, stack.ss_flags)
    }

    fn helper_thread_name() -> Option<String> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let mut buf = [0u8; 64];
            // SAFETY: `buf` is writable for `buf.len()` bytes and is
            // NUL-terminated by pthread_getname_np on success.
            let rc = unsafe {
                libc::pthread_getname_np(libc::pthread_self(), buf.as_mut_ptr().cast(), buf.len())
            };
            if rc != 0 {
                return None;
            }
            let end = buf.iter().position(|byte| *byte == 0).unwrap_or(buf.len());
            String::from_utf8(buf[..end].to_vec()).ok()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            std::thread::current().name().map(str::to_owned)
        }
    }
}
