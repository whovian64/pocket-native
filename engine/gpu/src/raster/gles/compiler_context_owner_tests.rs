//! Fake-only tests for the sibling's real ContextOwner seam; no EGL driver calls.
//! Exercises the enclosing production ContextOwner through a fake backend.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};
use super::{ContextOwner, EglApi, StartStep};

const WORKER_CONTEXT: usize = 11;
const WORKER_SURFACE: usize = 22;
const MAIN_CONTEXT: usize = 33;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call { Create, Pbuffer, Bind(usize, usize), Current, DestroySurface(usize), DestroyContext(usize), Release }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Event { thread: ThreadId, call: Call }
#[derive(Default)]
struct Failures { context: bool, pbuffer: bool, surfaceless: bool, pbuffer_bind: bool, lie_surfaceless: bool, unbind: bool, surface_destroy: bool, context_destroy: bool, release: bool }
#[derive(Default)]
struct State {
    failures: Failures,
    events: Vec<Event>,
    current: HashMap<ThreadId, usize>,
    contexts: HashSet<usize>,
    surfaces: HashSet<usize>,
    // Synthetic sentinel only: no GL operation is exposed through the EGL seam.
    main_program_alive: bool,
}
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<State>>);
impl Fake {
    fn configured(failures: Failures) -> Self {
        let fake = Self::default();
        { let mut s = fake.0.lock().unwrap(); s.failures = failures; s.main_program_alive = true; }
        fake
    }
    fn foreign_current(&self, context: usize) { self.0.lock().unwrap().current.insert(thread::current().id(), context); }
    fn events(&self) -> Vec<Event> { self.0.lock().unwrap().events.clone() }
    fn calls(&self) -> Vec<Call> { self.events().into_iter().map(|e| e.call).filter(|e| *e != Call::Current).collect() }
    fn program_alive(&self) -> bool { self.0.lock().unwrap().main_program_alive }
}
fn record(s: &mut State, call: Call) { s.events.push(Event { thread: thread::current().id(), call }); }

impl EglApi for Fake {
    fn create_context(&self) -> usize {
        let mut s=self.0.lock().unwrap(); record(&mut s, Call::Create);
        if s.failures.context { 0 } else { s.contexts.insert(WORKER_CONTEXT); WORKER_CONTEXT }
    }
    fn create_pbuffer(&self) -> usize {
        let mut s=self.0.lock().unwrap(); record(&mut s, Call::Pbuffer);
        if s.failures.pbuffer { 0 } else { s.surfaces.insert(WORKER_SURFACE); WORKER_SURFACE }
    }
    fn make_current(&self, context: usize, surface: usize) -> bool {
        let mut s=self.0.lock().unwrap(); record(&mut s, Call::Bind(context,surface));
        let failure = if context == 0 { s.failures.unbind } else if surface == 0 { s.failures.surfaceless } else { s.failures.pbuffer_bind };
        if !failure && !(context != 0 && surface == 0 && s.failures.lie_surfaceless) { s.current.insert(thread::current().id(), context); }
        !failure
    }
    fn current_context(&self) -> usize {
        let mut s=self.0.lock().unwrap(); record(&mut s, Call::Current);
        *s.current.get(&thread::current().id()).unwrap_or(&0)
    }
    fn destroy_surface(&self, surface: usize) -> bool {
        let mut s=self.0.lock().unwrap(); record(&mut s, Call::DestroySurface(surface));
        if s.failures.surface_destroy { false } else { s.surfaces.remove(&surface); true }
    }
    fn destroy_context(&self, context: usize) -> bool {
        let mut s=self.0.lock().unwrap(); record(&mut s, Call::DestroyContext(context));
        if s.failures.context_destroy { false } else { s.contexts.remove(&context); true }
    }
    fn release_thread(&self) -> bool {
        let mut s=self.0.lock().unwrap(); record(&mut s, Call::Release);
        if s.failures.release { false } else { s.current.remove(&thread::current().id()); true }
        // Deliberately does not erase contexts or surfaces: release is not destroy.
    }
}

#[test]
fn created_pbuffer_failed_bind_is_destroyed_before_context_once() {
    let fake=Fake::configured(Failures { surfaceless:true, pbuffer_bind:true, ..Default::default() });
    let failure=ContextOwner::create(fake.clone()).err().expect("bind failure");
    assert_eq!(failure.step,StartStep::MakeCurrent);
    assert_eq!((failure.cleanup.destroy_surface,failure.cleanup.destroy_context,failure.cleanup.release_thread),(Some(true),Some(true),Some(true)));
    let calls=fake.calls();
    assert_eq!(calls, vec![Call::Create,Call::Bind(WORKER_CONTEXT,0),Call::Pbuffer,Call::Bind(WORKER_CONTEXT,WORKER_SURFACE),Call::DestroySurface(WORKER_SURFACE),Call::DestroyContext(WORKER_CONTEXT),Call::Release]);
    let s=fake.0.lock().unwrap(); assert!(s.surfaces.is_empty()); assert!(s.contexts.is_empty());
}

#[test]
fn successful_surfaceless_worker_exit_cleans_on_owner_thread_once() {
    let fake=Fake::configured(Failures::default()); fake.foreign_current(MAIN_CONTEXT);
    let observed=fake.clone(); let main=thread::current().id();
    let worker=thread::spawn(move || {
        let thread_id=thread::current().id();
        let owner=ContextOwner::create(observed).ok().expect("owner");
        assert!(owner.is_current());
        drop(owner); thread_id
    }).join().unwrap();
    assert_ne!(worker,main);
    assert_eq!(fake.calls(), vec![Call::Create,Call::Bind(WORKER_CONTEXT,0),Call::Bind(0,0),Call::DestroyContext(WORKER_CONTEXT),Call::Release]);
    assert!(fake.events().iter().all(|event| event.thread==worker));
    assert_eq!(fake.0.lock().unwrap().current[&main], MAIN_CONTEXT);
    assert!(fake.program_alive()); // Fake main share-group sentinel, not a real GL observation.
}

#[test]
fn successful_pbuffer_cleanup_does_not_repeat_on_explicit_close_and_drop() {
    let fake=Fake::configured(Failures { surfaceless:true, ..Default::default() });
    let mut owner=ContextOwner::create(fake.clone()).ok().expect("owner");
    assert!(owner.is_current()); let report=owner.close(); assert!(!report.failed());
    let first=fake.calls(); assert!(owner.close().already_closed); drop(owner);
    assert_eq!(first, fake.calls());
    assert_eq!(first, vec![Call::Create,Call::Bind(WORKER_CONTEXT,0),Call::Pbuffer,Call::Bind(WORKER_CONTEXT,WORKER_SURFACE),Call::Bind(0,0),Call::DestroySurface(WORKER_SURFACE),Call::DestroyContext(WORKER_CONTEXT),Call::Release]);
}

#[test]
fn existing_foreign_current_is_not_mutated_or_released() {
    let fake=Fake::configured(Failures::default()); fake.foreign_current(MAIN_CONTEXT);
    let failure=ContextOwner::create(fake.clone()).err().expect("foreign context");
    assert_eq!(failure.step,StartStep::ForeignCurrent); assert!(failure.cleanup.foreign_current);
    assert!(fake.calls().is_empty()); assert_eq!(fake.0.lock().unwrap().current[&thread::current().id()], MAIN_CONTEXT);
    assert!(fake.program_alive());
}

#[test]
fn cleanup_failures_do_not_repeat_or_skip_later_attempts() {
    let fake=Fake::configured(Failures { surfaceless:true, unbind:true, surface_destroy:true, context_destroy:true, release:true, ..Default::default() });
    let mut owner=ContextOwner::create(fake.clone()).ok().expect("owner");
    let report=owner.close(); assert!(report.failed());
    assert_eq!((report.unbind,report.destroy_surface,report.destroy_context,report.release_thread),(Some(false),Some(false),Some(false),Some(false)));
    let first=fake.calls(); assert!(owner.close().already_closed); drop(owner);
    assert_eq!(first, fake.calls());
    assert_eq!(first, vec![Call::Create,Call::Bind(WORKER_CONTEXT,0),Call::Pbuffer,Call::Bind(WORKER_CONTEXT,WORKER_SURFACE),Call::Bind(0,0),Call::DestroySurface(WORKER_SURFACE),Call::DestroyContext(WORKER_CONTEXT),Call::Release]);
    let s=fake.0.lock().unwrap(); assert!(s.contexts.contains(&WORKER_CONTEXT)); assert!(s.surfaces.contains(&WORKER_SURFACE));
}

#[test]
fn context_creation_failure_never_destroys_unowned_handles() {
    let fake=Fake::configured(Failures { context:true, ..Default::default() });
    let failure=ContextOwner::create(fake.clone()).err().expect("context failure");
    assert_eq!(failure.step,StartStep::CreateContext); assert_eq!(failure.cleanup.release_thread,Some(true));
    let calls=fake.calls();
    assert_eq!(calls.iter().filter(|call| **call==Call::Create).count(),1);
    assert!(!calls.iter().any(|call| matches!(call,Call::DestroyContext(_) | Call::DestroySurface(_) | Call::Pbuffer | Call::Bind(_, _))));
    assert!(fake.program_alive());
}

#[test]
fn pbuffer_creation_failure_destroys_created_context_once() {
    let fake=Fake::configured(Failures { surfaceless:true, pbuffer:true, ..Default::default() });
    let failure=ContextOwner::create(fake.clone()).err().expect("pbuffer failure");
    assert_eq!(failure.step,StartStep::CreatePbuffer); assert_eq!(failure.cleanup.destroy_context,Some(true));
    let calls=fake.calls();
    assert_eq!(calls.iter().filter(|call| **call==Call::Create).count(),1);
    assert_eq!(calls.iter().filter(|call| **call==Call::Pbuffer).count(),1);
    assert_eq!(calls.iter().filter(|call| **call==Call::DestroyContext(WORKER_CONTEXT)).count(),1);
    assert!(!calls.iter().any(|call| matches!(call,Call::DestroySurface(_))));
}

#[test]
fn restore_owned_context_only_when_thread_unbound() {
    let fake=Fake::configured(Failures::default());
    let mut owner=ContextOwner::create(fake.clone()).ok().expect("owner");
    fake.foreign_current(0);
    assert!(!owner.is_current()); assert!(owner.ensure_current_for_cleanup()); assert!(owner.is_current());
    let binds=fake.calls().into_iter().filter(|c| *c==Call::Bind(WORKER_CONTEXT,0)).count();
    assert_eq!(binds,2); assert!(!owner.close().failed());
}

#[test]
fn foreign_replacement_is_never_rebound_or_released() {
    let fake=Fake::configured(Failures::default());
    let mut owner=ContextOwner::create(fake.clone()).ok().expect("owner");
    fake.foreign_current(MAIN_CONTEXT);
    assert!(!owner.ensure_current_for_cleanup());
    let report=owner.close(); assert!(report.foreign_current); assert!(report.failed());
    assert_eq!((report.unbind,report.destroy_context,report.release_thread),(None,Some(true),None));
    assert_eq!(fake.calls(),vec![Call::Create,Call::Bind(WORKER_CONTEXT,0),Call::DestroyContext(WORKER_CONTEXT)]);
    assert_eq!(fake.0.lock().unwrap().current[&thread::current().id()],MAIN_CONTEXT);
    assert!(fake.program_alive());
}

#[test]
fn unwind_drops_guard_once_on_owner_thread() {
    let fake=Fake::configured(Failures::default()); let tid=thread::current().id();
    let caught=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _owner=ContextOwner::create(fake.clone()).ok().expect("owner");
        panic!("fixture unwind");
    }));
    assert!(caught.is_err());
    assert_eq!(fake.calls(),vec![Call::Create,Call::Bind(WORKER_CONTEXT,0),Call::Bind(0,0),Call::DestroyContext(WORKER_CONTEXT),Call::Release]);
    assert!(fake.events().iter().all(|event| event.thread==tid));
}

#[test]
fn fake_release_thread_does_not_destroy_objects() {
    let fake=Fake::configured(Failures::default());
    assert_eq!(fake.create_context(),WORKER_CONTEXT); assert_eq!(fake.create_pbuffer(),WORKER_SURFACE);
    assert!(fake.make_current(WORKER_CONTEXT,WORKER_SURFACE)); assert!(fake.release_thread());
    let s=fake.0.lock().unwrap(); assert!(s.contexts.contains(&WORKER_CONTEXT)); assert!(s.surfaces.contains(&WORKER_SURFACE));
}

#[test]
fn successful_bind_with_wrong_current_is_failed_and_cleaned() {
    let fake=Fake::configured(Failures { lie_surfaceless:true, ..Default::default() });
    let failure=ContextOwner::create(fake.clone()).err().expect("wrong actual current");
    assert_eq!(failure.step,StartStep::CurrentMismatch);
    assert_eq!((failure.cleanup.unbind,failure.cleanup.destroy_context,failure.cleanup.release_thread),(None,Some(true),Some(true)));
    assert_eq!(fake.calls(),vec![Call::Create,Call::Bind(WORKER_CONTEXT,0),Call::DestroyContext(WORKER_CONTEXT),Call::Release]);
}

#[test]
fn each_individual_cleanup_failure_is_reported_and_remaining_calls_continue() {
    for which in 0..4 {
        let fake=Fake::configured(Failures { surfaceless:true, unbind:which==0, surface_destroy:which==1, context_destroy:which==2, release:which==3, ..Default::default() });
        let mut owner=ContextOwner::create(fake.clone()).ok().expect("owner");
        let report=owner.close();
        assert!(report.failed());
        assert_eq!([report.unbind,report.destroy_surface,report.destroy_context,report.release_thread],
                   [Some(which!=0),Some(which!=1),Some(which!=2),Some(which!=3)]);
        let events=fake.calls(); drop(owner); assert_eq!(events,fake.calls());
    }
}
