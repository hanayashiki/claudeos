//! The order address spaces are loaded and freed in, checked against spaces
//! that write down what happens to them.
//!
//! In the kernel an address space is an `Arc<Mm>`: each task running in it
//! holds one, and the processor holds one on the space it has loaded. The last
//! to go frees the tables, and freeing the tables the processor is walking is
//! the defect the processor's reference is there to rule out. A `Space` here
//! stands in for the `Mm`: its drop is the free, and it records whether the
//! machine was on it at that moment.

use mmtest::active::{Active, Deferred};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Load(u32),
    LoadKernel,
    Free { space: u32, while_loaded: bool },
}

/// What happened, in order, and which space the processor is on (`None` for
/// the kernel's own tables).
#[derive(Clone, Default)]
struct Machine {
    log: Rc<RefCell<Vec<Event>>>,
    loaded: Rc<Cell<Option<u32>>>,
}

impl Machine {
    fn space(&self, id: u32) -> Arc<Space> {
        Arc::new(Space { id, machine: self.clone() })
    }

    fn load(&self, space: &Space) {
        self.loaded.set(Some(space.id));
        self.log.borrow_mut().push(Event::Load(space.id));
    }

    fn load_kernel(&self) {
        self.loaded.set(None);
        self.log.borrow_mut().push(Event::LoadKernel);
    }

    fn events(&self) -> Vec<Event> {
        self.log.borrow().clone()
    }

    fn frees(&self, id: u32) -> usize {
        let freed = |e: &&Event| matches!(e, Event::Free { space, .. } if *space == id);
        self.events().iter().filter(freed).count()
    }
}

struct Space {
    id: u32,
    machine: Machine,
}

impl Drop for Space {
    fn drop(&mut self) {
        let while_loaded = self.machine.loaded.get() == Some(self.id);
        self.machine.log.borrow_mut().push(Event::Free { space: self.id, while_loaded });
    }
}

fn free(space: u32) -> Event {
    Event::Free { space, while_loaded: false }
}

#[test]
fn threads_share_a_space_and_the_last_reference_frees_it() {
    let machine = Machine::default();
    let leader = machine.space(1);
    let thread = leader.clone();
    drop(leader);
    assert_eq!(machine.events(), vec![], "a thread still runs in it");
    drop(thread);
    assert_eq!(machine.events(), vec![free(1)]);
}

#[test]
fn the_processor_keeps_the_space_it_is_on_after_the_task_lets_go() {
    let machine = Machine::default();
    let mut active = Active::new();
    let task = machine.space(1);
    assert!(active.switch(&task, |s| machine.load(s)).is_none(), "it was on the kernel's tables");

    // An exit or an exec drops the task's reference while still on the tables.
    drop(task);
    assert_eq!(machine.events(), vec![Event::Load(1)]);

    let left = active.leave(|| machine.load_kernel());
    assert_eq!(machine.events(), vec![Event::Load(1), Event::LoadKernel], "nothing freed yet");
    drop(left);
    assert_eq!(machine.events(), vec![Event::Load(1), Event::LoadKernel, free(1)]);
}

#[test]
fn a_switch_loads_the_next_space_before_the_last_reference_to_the_old_one_goes() {
    let machine = Machine::default();
    let mut active = Active::new();
    let old = machine.space(1);
    drop(active.switch(&old, |s| machine.load(s)));
    drop(old);

    let next = machine.space(2);
    let left = active.switch(&next, |s| machine.load(s));
    assert_eq!(machine.events(), vec![Event::Load(1), Event::Load(2)]);
    assert!(left.is_some());
    // What the caller drops, where it chooses: with interrupts on, or into a
    // `Deferred` when they are masked.
    drop(left);
    assert_eq!(machine.events(), vec![Event::Load(1), Event::Load(2), free(1)]);
    assert!(active.is(&next));
}

#[test]
fn a_switch_to_the_space_already_loaded_loads_nothing_and_lets_go_of_nothing() {
    let machine = Machine::default();
    let mut active = Active::new();
    let space = machine.space(1);
    assert!(active.switch(&space, |s| machine.load(s)).is_none());
    assert!(active.switch(&space, |s| machine.load(s)).is_none());
    assert_eq!(machine.events(), vec![Event::Load(1)]);
    assert_eq!(Arc::strong_count(&space), 2, "the task's and the processor's");
}

#[test]
fn leaving_the_kernel_tables_for_the_kernel_tables_loads_nothing() {
    let machine = Machine::default();
    let mut active: Active<Space> = Active::new();
    assert!(active.leave(|| machine.load_kernel()).is_none());
    assert_eq!(machine.events(), vec![]);
}

/// Exec: the task's reference moves to a new space and the processor follows.
/// A sibling thread still running in the old space keeps it; without one, the
/// old space is freed once both references are dropped, after the new one is
/// loaded.
#[test]
fn exec_frees_the_old_space_after_the_new_one_is_loaded_and_only_without_a_sibling() {
    for sibling in [false, true] {
        let machine = Machine::default();
        let mut active = Active::new();
        let mut task = Some(machine.space(1));
        let other_thread = if sibling { task.clone() } else { None };
        drop(active.switch(task.as_ref().unwrap(), |s| machine.load(s)));

        let new = machine.space(2);
        let processor = active.switch(&new, |s| machine.load(s));
        let previous = task.replace(new);
        drop((processor, previous));

        if sibling {
            assert_eq!(machine.events(), vec![Event::Load(1), Event::Load(2)]);
            drop(other_thread);
        }
        assert_eq!(machine.events(), vec![Event::Load(1), Event::Load(2), free(1)]);
    }
}

/// An exec that fails goes back to the old space, and the half-built one goes
/// when exec's own reference to it does.
#[test]
fn an_abandoned_exec_frees_the_new_space_after_going_back() {
    let machine = Machine::default();
    let mut active = Active::new();
    let mut task = Some(machine.space(1));
    drop(active.switch(task.as_ref().unwrap(), |s| machine.load(s)));

    let old = task.clone();
    let new = machine.space(2);
    drop((active.switch(&new, |s| machine.load(s)), task.replace(new.clone())));

    drop((active.switch(old.as_ref().unwrap(), |s| machine.load(s)), task.replace(old.unwrap())));
    assert_eq!(machine.frees(2), 0, "exec still holds it");
    drop(new);
    assert_eq!(machine.events(), vec![Event::Load(1), Event::Load(2), Event::Load(1), free(2)]);
    assert_eq!(machine.frees(1), 0);
}

/// However the references to one space are let go of -- threads exiting in
/// any order, the processor moving off before, between or after them -- the
/// space is freed once, when the last goes, and never while it is loaded.
#[test]
fn a_space_is_freed_once_whatever_order_its_references_go_in() {
    const THREADS: usize = 3;
    // Each release is a thread's exit (0..THREADS) or the processor moving to
    // another space (THREADS).
    fn orders(left: Vec<usize>) -> Vec<Vec<usize>> {
        if left.is_empty() {
            return vec![vec![]];
        }
        let mut out = Vec::new();
        for (i, first) in left.iter().enumerate() {
            let mut rest = left.clone();
            rest.remove(i);
            for mut tail in orders(rest) {
                tail.insert(0, *first);
                out.push(tail);
            }
        }
        out
    }
    let orders = orders((0..=THREADS).collect());
    assert_eq!(orders.len(), 24);

    for order in orders {
        let machine = Machine::default();
        let mut active = Active::new();
        let mut threads: Vec<Option<Arc<Space>>> = Vec::new();
        let first = machine.space(1);
        drop(active.switch(&first, |s| machine.load(s)));
        for _ in 1..THREADS {
            threads.push(Some(first.clone()));
        }
        threads.push(Some(first));
        let elsewhere = machine.space(2);

        for (step, release) in order.iter().enumerate() {
            if *release == THREADS {
                drop(active.switch(&elsewhere, |s| machine.load(s)));
            } else {
                drop(threads[*release].take());
            }
            let expected = if step == THREADS { 1 } else { 0 };
            assert_eq!(machine.frees(1), expected, "order {:?}, after step {}", order, step);
        }
        assert!(
            machine.events().iter().all(|e| !matches!(e, Event::Free { while_loaded: true, .. })),
            "order {:?} freed a loaded space: {:?}",
            order,
            machine.events()
        );
    }
}

/// A last reference dropped where interrupts are masked goes into a
/// `Deferred` instead, and is freed only when a caller with them on takes it.
#[test]
fn deferred_frees_wait_until_they_are_taken() {
    let machine = Machine::default();
    let mut deferred = Deferred::new();
    let space = machine.space(1);
    let only = Arc::try_unwrap(space).ok().expect("the last reference");
    deferred.push(only);
    deferred.push(Arc::try_unwrap(machine.space(2)).ok().unwrap());
    assert_eq!(machine.events(), vec![]);

    let waiting = deferred.take();
    assert_eq!(machine.events(), vec![], "taken but not yet dropped");
    assert!(deferred.take().is_empty(), "a second take finds nothing");
    drop(waiting);
    assert_eq!(machine.events(), vec![free(1), free(2)]);
}
