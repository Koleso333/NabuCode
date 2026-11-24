//! «Interrupt and send»: the queued messages reach the model now instead of after the step.
//! A streaming reply is cut where it is; running tools are told to wrap up: commands and subagents go on
//! in the background, network calls and waits on tasks stop, file tools finish.
use std::pin::pin;
use std::sync::Mutex;

use ncore::CancelToken;

/// One running turn: its stop button and its interruption.
pub struct Interrupt {
    turn: CancelToken,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Cancels the model run in progress; each run of the turn gets a fresh one.
    run: CancelToken,
    /// Tells the running tool calls to wrap up; fresh again once none runs.
    wrap_up: CancelToken,
    calls: usize,
    /// The run was cut by the person, so the turn goes on instead of stopping.
    cut: bool,
}

/// A tool call in progress; ends when dropped.
pub struct Call<'a> {
    interrupt: &'a Interrupt,
    pub wrap_up: CancelToken,
}

impl Interrupt {
    pub fn new(turn: CancelToken) -> Self {
        Self { turn, inner: Mutex::default() }
    }

    /// The token of the next model run; an interruption before it starts is already answered by it.
    pub fn next_run(&self) -> CancelToken {
        let mut inner = self.inner.lock().unwrap();
        inner.run = CancelToken::new();
        inner.cut = false;
        if self.turn.is_cancelled() {
            inner.run.cancel();
        }
        inner.run.clone()
    }

    /// Wraps up the running tool calls, or cuts the model run when none runs.
    pub fn fire(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.calls > 0 {
            inner.wrap_up.cancel();
        } else {
            inner.cut = true;
            inner.run.cancel();
        }
    }

    /// Whether the last run was cut by the person rather than stopped.
    pub fn take_cut(&self) -> bool {
        std::mem::take(&mut self.inner.lock().unwrap().cut)
    }

    /// Stops the turn: the stop button.
    pub fn abort(&self) {
        let inner = self.inner.lock().unwrap();
        self.turn.cancel();
        inner.run.cancel();
    }

    /// Awaits `work` of the current run; the turn's token cancelled elsewhere, like a subagent's by its agent, stops the run too.
    pub async fn guard<T>(&self, work: impl Future<Output = T>) -> T {
        let mut work = pin!(work);
        tokio::select! {
            out = &mut work => out,
            _ = self.turn.cancelled() => {
                self.abort();
                work.await
            }
        }
    }

    pub fn call(&self) -> Call<'_> {
        let mut inner = self.inner.lock().unwrap();
        inner.calls += 1;
        Call { interrupt: self, wrap_up: inner.wrap_up.clone() }
    }
}

impl Default for Interrupt {
    fn default() -> Self {
        Self::new(CancelToken::new())
    }
}

impl Drop for Call<'_> {
    fn drop(&mut self) {
        let mut inner = self.interrupt.inner.lock().unwrap();
        inner.calls -= 1;
        if inner.calls == 0 && inner.wrap_up.is_cancelled() {
            inner.wrap_up = CancelToken::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_cuts_the_run_without_tools_and_wraps_them_up_with() {
        let interrupt = Interrupt::default();
        let run = interrupt.next_run();
        let call = interrupt.call();
        interrupt.fire();
        assert!(call.wrap_up.is_cancelled() && !run.is_cancelled() && !interrupt.take_cut());
        drop(call);
        assert!(!interrupt.call().wrap_up.is_cancelled(), "the next step's tools start fresh");
        interrupt.fire();
        assert!(run.is_cancelled() && interrupt.take_cut() && !interrupt.take_cut());
        interrupt.fire();
        assert!(!interrupt.next_run().is_cancelled(), "a new run answers an earlier interruption");
        interrupt.abort();
        assert!(interrupt.next_run().is_cancelled() && !interrupt.take_cut());
    }
}
