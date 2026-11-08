//! Owned compiler-thread EGL handles. No GL object or renderer lifetime policy.
//! Pure call seam also permits a standalone fake-EGL CPU fixture.
use std::marker::PhantomData;
use std::rc::Rc;
use std::thread::{self, ThreadId};

/// A backend belongs to one existing display/share group. 0 denotes NO_*.
/// Operations must not panic. create runs only on a fresh compiler thread;
/// no unrelated current context may be displaced or released.
pub trait EglApi {
    fn create_context(&self) -> usize;
    fn create_pbuffer(&self) -> usize;
    fn make_current(&self, context: usize, surface: usize) -> bool;
    fn current_context(&self) -> usize;
    fn destroy_surface(&self, surface: usize) -> bool;
    fn destroy_context(&self, context: usize) -> bool;
    fn release_thread(&self) -> bool;
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleanupReport {
    pub already_closed: bool,
    pub wrong_thread: bool,
    pub foreign_current: bool,
    pub unbind: Option<bool>,
    pub destroy_surface: Option<bool>,
    pub destroy_context: Option<bool>,
    pub release_thread: Option<bool>,
}
impl CleanupReport {
    pub fn failed(&self) -> bool {
        self.wrong_thread || self.foreign_current || [self.unbind, self.destroy_surface, self.destroy_context, self.release_thread].contains(&Some(false))
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartStep { ForeignCurrent, CreateContext, CreatePbuffer, MakeCurrent, CurrentMismatch }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartFailure { pub step: StartStep, pub cleanup: CleanupReport }

pub struct ContextOwner<A: EglApi> {
    api: A,
    context: Option<usize>,
    surface: Option<usize>,
    thread: ThreadId,
    closed: bool,
    // Explicitly !Send + !Sync even when the backend is shared/thread-safe.
    _thread_bound: PhantomData<Rc<()>>,
}
impl<A: EglApi> ContextOwner<A> {
    pub fn create(api: A) -> Result<Self, StartFailure> {
        if api.current_context() != 0 {
            return Err(StartFailure { step: StartStep::ForeignCurrent, cleanup: CleanupReport { foreign_current: true, ..CleanupReport::default() } });
        }
        let mut owner = Self { api, context: None, surface: None, thread: thread::current().id(), closed: false, _thread_bound: PhantomData };
        let context = owner.api.create_context();
        if context == 0 { return Err(owner.failed_start(StartStep::CreateContext)); }
        owner.context = Some(context);
        if owner.api.make_current(context, 0) {
            if owner.api.current_context() != context { return Err(owner.failed_start(StartStep::CurrentMismatch)); }
            return Ok(owner);
        }
        let surface = owner.api.create_pbuffer();
        if surface == 0 { return Err(owner.failed_start(StartStep::CreatePbuffer)); }
        owner.surface = Some(surface);
        if !owner.api.make_current(context, surface) { return Err(owner.failed_start(StartStep::MakeCurrent)); }
        if owner.api.current_context() != context { return Err(owner.failed_start(StartStep::CurrentMismatch)); }
        Ok(owner)
    }
    fn failed_start(&mut self, step: StartStep) -> StartFailure { StartFailure { step, cleanup: self.close() } }
    pub fn context(&self) -> Option<usize> { self.context }
    pub fn surface(&self) -> Option<usize> { self.surface }
    pub fn is_current(&self) -> bool {
        !self.closed && thread::current().id() == self.thread && self.context.is_some_and(|context| self.api.current_context() == context)
    }
    /// Restore only this owned context when the thread is currently unbound.
    /// Never replaces an unrelated context. Required before GL-cache/Glow drop.
    pub fn ensure_current_for_cleanup(&self) -> bool {
        if self.closed || thread::current().id() != self.thread { return false; }
        let Some(context) = self.context else { return false };
        let current = self.api.current_context();
        if current == context { return true; }
        if current != 0 { return false; }
        self.api.make_current(context, self.surface.unwrap_or(0)) && self.api.current_context() == context
    }
    /// Each raw object is consumed before its one destroy attempt. Failed calls
    /// remain observable but do not cause hidden retries/double destruction.
    /// Destroy attempts continue after unbind/destroy failures. ReleaseThread
    /// never substitutes for object destruction and never releases foreign state.
    pub fn close(&mut self) -> CleanupReport {
        if self.closed { return CleanupReport { already_closed: true, ..CleanupReport::default() }; }
        if thread::current().id() != self.thread { return CleanupReport { wrong_thread: true, ..CleanupReport::default() }; }
        self.closed = true;
        let context = self.context.take(); let surface = self.surface.take();
        let current = self.api.current_context();
        let mut report = CleanupReport { foreign_current: current != 0 && Some(current) != context, ..CleanupReport::default() };
        if current != 0 && Some(current) == context { report.unbind = Some(self.api.make_current(0, 0)); }
        if let Some(surface) = surface { report.destroy_surface = Some(self.api.destroy_surface(surface)); }
        if let Some(context) = context { report.destroy_context = Some(self.api.destroy_context(context)); }
        if !report.foreign_current { report.release_thread = Some(self.api.release_thread()); }
        report
    }
}
impl<A: EglApi> Drop for ContextOwner<A> { fn drop(&mut self) { let _ = self.close(); } }

#[cfg(test)]
#[path = "compiler_context_owner_tests.rs"]
mod tests;
