//! Windows: which session holds each output's Desktop Duplication.
//!
//! Windows allows one duplication per output per process. A client whose link died can keep its
//! session (and so the output) alive for minutes, and the tablet's reconnect then fails with
//! "Invalid output duplication argument" (WORKERPC, 09/28). So the latest client wins: a session
//! about to build the GPU path on an output another session holds writes its own id into that
//! session's taken_by flag and waits for it to let go. The holder's video thread checks its flag
//! every tick, drops its encoder (ending the duplication) and with it this claim.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

struct Owner {
    output: u32,
    session: u64,
    /// the holder's flag: 0, or the id of the session that wants the output
    taken_by: Arc<AtomicU64>,
}

static OWNERS: Mutex<Vec<Owner>> = Mutex::new(Vec::new());
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

fn owners() -> MutexGuard<'static, Vec<Owner>> {
    // a panic elsewhere must not stop every later session from capturing
    OWNERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A new id for a client session's video thread (never 0, which means "nobody" in taken_by).
pub fn new_session_id() -> u64 {
    NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
}

/// A session's hold on an output. It lives inside the GPU VideoEncoder, so it is dropped (and
/// the output freed for the next session) only after the encoder has ended the duplication,
/// however the encoder goes: a rebuild, a takeover, a dead client or the session's end.
pub struct DdaClaim {
    output: u32,
    session: u64,
}

impl Drop for DdaClaim {
    fn drop(&mut self) {
        owners().retain(|o| !(o.output == self.output && o.session == self.session));
    }
}

/// What claim() found.
pub struct Takeover {
    /// the session that held the output, if another did
    pub from: Option<u64>,
    pub waited: Duration,
    /// false: that session did not let go within the wait (the build may then fail and retry)
    pub released: bool,
}

/// Claim `output` for `session` (whose own flag is `taken_by`). If another session holds it, write
/// `session` into that session's flag and wait up to `wait` for it to let go; after that the
/// claim is taken anyway.
pub fn claim(
    output: u32,
    session: u64,
    taken_by: &Arc<AtomicU64>,
    wait: Duration,
) -> (DdaClaim, Takeover) {
    let start = Instant::now();
    let mut from = None;
    // the holder last told to let go: a new holder (two sessions racing for one output) is told too
    let mut told = None;
    loop {
        {
            let mut owners = owners();
            let holder = owners
                .iter()
                .find(|o| o.output == output && o.session != session);
            let released = match holder {
                None => true,
                Some(o) => {
                    if told != Some(o.session) {
                        o.taken_by.store(session, Ordering::SeqCst);
                        told = Some(o.session);
                    }
                    if from.is_none() {
                        from = Some(o.session);
                    }
                    false
                }
            };
            if released || start.elapsed() >= wait {
                owners.retain(|o| o.output != output);
                owners.push(Owner {
                    output,
                    session,
                    taken_by: taken_by.clone(),
                });
                return (
                    DdaClaim { output, session },
                    Takeover {
                        from,
                        waited: start.elapsed(),
                        released,
                    },
                );
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
