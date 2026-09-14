//! Interval timers: what `setitimer`, `getitimer` and `alarm` arm.
//!
//! A process has three, and every thread in it shares them. A child made by
//! fork starts with none armed, and an exec keeps them. These are Linux's
//! rules.
//!
//! The real timer counts elapsed time and raises SIGALRM. It is kept as the
//! moment it is due on the monotonic clock, and each tick compares that with
//! the clock, so it never fires before the time it was set for and fires at
//! most a tick after.
//!
//! The virtual timer counts the time the process runs in user mode and raises
//! SIGVTALRM; the profiling timer counts the time it runs in either mode and
//! raises SIGPROF. Nothing here measures running time more finely than the
//! tick, so each tick is charged whole to the task it interrupted, in the mode
//! it interrupted it in.

use crate::abi::Errno;
use crate::sched;
use crate::signal::{SIGALRM, SIGPROF, SIGVTALRM};
use crate::task::{State, Task};
use core::sync::atomic::{AtomicU64, Ordering};

pub const ITIMER_REAL: i32 = 0;
pub const ITIMER_VIRTUAL: i32 = 1;
pub const ITIMER_PROF: i32 = 2;

/// A timer as a program sets and reads it: how long until it fires, and what
/// it is reloaded with each time it does. A value of zero is a timer that is
/// not armed; an interval of zero is one that fires once.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Setting {
    pub value_ns: u64,
    pub interval_ns: u64,
}

/// The real timer: when it is next due, in monotonic nanoseconds, and what is
/// added to that each time it fires. Due at zero is not armed.
#[derive(Clone, Copy, Default)]
struct RealTimer {
    due_ns: u64,
    interval_ns: u64,
}

impl RealTimer {
    fn setting(&self, now: u64) -> Setting {
        if self.due_ns == 0 {
            return Setting::default();
        }
        // A timer that is due but has not been taken by a tick yet is still
        // armed. It reads as having a microsecond left, because zero would
        // say it was off.
        Setting {
            value_ns: self.due_ns.saturating_sub(now).max(1_000),
            interval_ns: self.interval_ns,
        }
    }

    fn arm(&mut self, setting: Setting, now: u64) {
        *self = if setting.value_ns == 0 {
            RealTimer::default()
        } else {
            RealTimer {
                due_ns: now.saturating_add(setting.value_ns),
                interval_ns: setting.interval_ns,
            }
        };
    }

    fn is_due(&self, now: u64) -> bool {
        self.due_ns != 0 && now >= self.due_ns
    }

    /// The timer fired at `now`: it is off again, or due one interval later.
    /// Intervals that went by entirely before the tick looked are skipped
    /// rather than each fired, because the signal can be pending only once
    /// however many times it is raised.
    fn fired(&mut self, now: u64) {
        if self.interval_ns == 0 {
            self.due_ns = 0;
            return;
        }
        let missed = (now - self.due_ns) / self.interval_ns;
        self.due_ns = self
            .due_ns
            .saturating_add((missed + 1).saturating_mul(self.interval_ns));
    }
}

/// A timer that counts running time: what is left of it, and what it is
/// reloaded with.
#[derive(Clone, Copy, Default)]
struct CpuTimer {
    left_ns: u64,
    interval_ns: u64,
}

impl CpuTimer {
    fn setting(&self) -> Setting {
        Setting { value_ns: self.left_ns, interval_ns: self.interval_ns }
    }

    fn arm(&mut self, setting: Setting) {
        *self = if setting.value_ns == 0 {
            CpuTimer::default()
        } else {
            CpuTimer { left_ns: setting.value_ns, interval_ns: setting.interval_ns }
        };
    }

    /// Take `ns` of running time off. True when that used up what was left,
    /// which is when the signal is raised.
    fn charge(&mut self, ns: u64) -> bool {
        if self.left_ns == 0 {
            return false;
        }
        if self.left_ns > ns {
            self.left_ns -= ns;
            return false;
        }
        self.left_ns = self.interval_ns;
        true
    }
}

/// A process's three timers. Each of its tasks holds the same one.
#[derive(Default)]
pub struct IntervalTimers {
    real: RealTimer,
    virtual_time: CpuTimer,
    profile: CpuTimer,
}

/// No real timer anywhere is due before this, in monotonic nanoseconds, and it
/// is `u64::MAX` when none is armed. The tick reads it so that it walks the
/// process table only when some timer may be due. Arming a timer lowers it,
/// and the walk sets it to the earliest timer it found. A process that goes
/// away with a timer armed leaves it lower than it need be, which costs one
/// walk that fires nothing.
static EARLIEST_DUE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Set one of the calling process's timers, and return what it was set to.
pub fn set(which: i32, setting: Setting) -> Result<Setting, Errno> {
    let task = sched::current();
    let mut timers = task.itimers.lock();
    match which {
        ITIMER_REAL => {
            let now = crate::time::monotonic_ns();
            let old = timers.real.setting(now);
            timers.real.arm(setting, now);
            // The timers' lock holds interrupts off, so the tick cannot walk
            // the table between the arming and this, and store an earliest
            // time that does not know about it.
            if timers.real.due_ns != 0 {
                EARLIEST_DUE.fetch_min(timers.real.due_ns, Ordering::AcqRel);
            }
            Ok(old)
        }
        ITIMER_VIRTUAL => {
            let old = timers.virtual_time.setting();
            timers.virtual_time.arm(setting);
            Ok(old)
        }
        ITIMER_PROF => {
            let old = timers.profile.setting();
            timers.profile.arm(setting);
            Ok(old)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// What one of the calling process's timers is set to now.
pub fn get(which: i32) -> Result<Setting, Errno> {
    let task = sched::current();
    let timers = task.itimers.lock();
    match which {
        ITIMER_REAL => Ok(timers.real.setting(crate::time::monotonic_ns())),
        ITIMER_VIRTUAL => Ok(timers.virtual_time.setting()),
        ITIMER_PROF => Ok(timers.profile.setting()),
        _ => Err(Errno::EINVAL),
    }
}

/// Charge the tick that has just arrived, and raise what it makes due.
///
/// Called from the timer interrupt, with `from_user` saying which mode the
/// tick found the running task in.
pub fn on_tick(from_user: bool) {
    if !sched::has_current() {
        return;
    }
    charge_running_task(from_user);
    let earliest = EARLIEST_DUE.load(Ordering::Acquire);
    if earliest != u64::MAX {
        let now = crate::time::monotonic_ns();
        if now >= earliest {
            fire_real_timers(now);
        }
    }
}

fn charge_running_task(from_user: bool) {
    let task = sched::current();
    // The idle task runs for no process.
    if task.pid == 0 {
        return;
    }
    let tick = crate::time::ticks_to_ns(1);
    let raised = {
        let mut timers = task.itimers.lock();
        let mut raised = 0u64;
        if from_user && timers.virtual_time.charge(tick) {
            raised |= SIGVTALRM.bit();
        }
        if timers.profile.charge(tick) {
            raised |= SIGPROF.bit();
        }
        raised
    };
    if raised != 0 {
        task.add_pending(raised);
    }
}

/// Raise SIGALRM for each process whose real timer is due.
///
/// The timers belong to the process and the signal is taken by one of its
/// tasks: the first in the table that is not a zombie and does not block
/// SIGALRM, or, when every live one blocks it, the first live one, where it
/// stays pending until it is unblocked. A process whose tasks are all zombies
/// has nobody to take it, and nothing is raised.
fn fire_real_timers(now: u64) {
    sched::with_tasks(|table| {
        // Two walks, because a task that blocks the signal can come before one
        // that does not, and which of them takes it is known only after looking
        // at both.
        table.for_each(|task| {
            let blocks = task.blocked() & SIGALRM.bit() != 0;
            if task.state() != State::Zombie && !blocks && take_if_due(task, now) {
                sched::post_signal(task, SIGALRM, table);
            }
        });
        let mut earliest = u64::MAX;
        table.for_each(|task| {
            if task.state() == State::Zombie {
                return;
            }
            if take_if_due(task, now) {
                sched::post_signal(task, SIGALRM, table);
            }
            let due = task.itimers.lock().real.due_ns;
            if due != 0 {
                earliest = earliest.min(due);
            }
        });
        EARLIEST_DUE.store(earliest, Ordering::Release);
    });
}

/// Move `task`'s real timer on if it is due at `now`, and say whether it was.
fn take_if_due(task: &Task, now: u64) -> bool {
    let mut timers = task.itimers.lock();
    if !timers.real.is_due(now) {
        return false;
    }
    timers.real.fired(now);
    true
}
