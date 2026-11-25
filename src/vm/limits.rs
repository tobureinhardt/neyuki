//! Execution limits and interrupt controls for sandboxed or embedded VMs.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Reason a script's execution was terminated by its configured limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitViolation {
    InstructionBudgetExceeded { executed: u64, budget: u64 },
    HeapLimitExceeded { allocated: usize, limit: usize },
    DeadlineExceeded,
    Interrupted,
}

impl fmt::Display for LimitViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LimitViolation::InstructionBudgetExceeded { executed, budget } => write!(
                f,
                "execution limit exceeded: instruction budget of {} exhausted (executed {})",
                budget, executed
            ),
            LimitViolation::HeapLimitExceeded { allocated, limit } => write!(
                f,
                "execution limit exceeded: heap limit of {} bytes exceeded ({} bytes allocated)",
                limit, allocated
            ),
            LimitViolation::DeadlineExceeded => {
                write!(f, "execution limit exceeded: execution deadline exceeded")
            }
            LimitViolation::Interrupted => write!(f, "execution interrupted"),
        }
    }
}

impl std::error::Error for LimitViolation {}

/// Configurable resource budgets and cancellation handle for a [`VM`](crate::vm::VM).
#[derive(Debug, Clone)]
pub struct ExecutionLimits {
    /// Maximum number of VM instructions permitted (`None` = unlimited).
    pub instruction_budget: Option<u64>,
    /// Number of VM instructions executed so far under the current budget.
    pub instructions_executed: u64,
    /// Maximum tracked GC heap allocation in bytes (`None` = unlimited).
    pub heap_limit_bytes: Option<usize>,
    /// Wall-clock deadline after which execution aborts (`None` = unlimited).
    pub deadline: Option<Instant>,
    /// Thread-safe external cancellation flag.
    pub interrupt_flag: Arc<AtomicBool>,
    /// Latched violation once a limit fires; prevents `pcall`/`try`/`xpcall`
    /// from swallowing the termination error.
    pub tripped: Option<LimitViolation>,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionLimits {
    pub fn new() -> Self {
        Self {
            instruction_budget: None,
            instructions_executed: 0,
            heap_limit_bytes: None,
            deadline: None,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            tripped: None,
        }
    }

    pub fn with_instruction_budget(mut self, budget: u64) -> Self {
        self.instruction_budget = Some(budget);
        self
    }

    pub fn with_heap_limit(mut self, bytes: usize) -> Self {
        self.heap_limit_bytes = Some(bytes);
        self
    }

    pub fn with_timeout(mut self, duration: Duration) -> Self {
        self.deadline = Some(Instant::now() + duration);
        self
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Returns a handle that another thread can flip to cancel execution.
    pub fn interrupt_handle(&self) -> Arc<AtomicBool> {
        self.interrupt_flag.clone()
    }

    /// Triggers cancellation immediately.
    pub fn interrupt(&self) {
        self.interrupt_flag.store(true, Ordering::Release);
    }

    /// Clears the interrupt flag and any latched `Interrupted` violation.
    pub fn clear_interrupt(&mut self) {
        self.interrupt_flag.store(false, Ordering::Release);
        if matches!(self.tripped, Some(LimitViolation::Interrupted)) {
            self.tripped = None;
        }
    }

    /// Resets the executed instruction counter and clears any latched violation.
    pub fn reset_counters(&mut self) {
        self.instructions_executed = 0;
        self.tripped = None;
    }

    /// True when any per-instruction limit needs the dispatch trap enabled.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.instruction_budget.is_some()
            || self.heap_limit_bytes.is_some()
            || self.deadline.is_some()
            || self.tripped.is_some()
    }

    /// True once a limit has fired or the interrupt flag has been raised.
    /// `pcall`/`try`/`xpcall` check this so limit errors cannot be caught.
    #[inline]
    pub fn is_tripped(&self) -> bool {
        self.tripped.is_some() || self.interrupt_flag.load(Ordering::Relaxed)
    }

    /// Returns the error message if a limit has been tripped or interrupted.
    #[inline]
    pub fn tripped_error(&self) -> Option<String> {
        if let Some(v) = &self.tripped {
            Some(v.to_string())
        } else if self.interrupt_flag.load(Ordering::Relaxed) {
            Some(LimitViolation::Interrupted.to_string())
        } else {
            None
        }
    }

    /// Latches and returns an error if the interrupt flag is set or a limit
    /// was already tripped. Safe to call on backward jumps / calls even on the
    /// fast dispatch path.
    #[inline(always)]
    pub fn check_interrupt(&mut self) -> Result<(), String> {
        if self.tripped.is_some() || self.interrupt_flag.load(Ordering::Acquire) {
            return self.check_interrupt_slow();
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn check_interrupt_slow(&mut self) -> Result<(), String> {
        if let Some(v) = &self.tripped {
            return Err(v.to_string());
        }
        if self.interrupt_flag.load(Ordering::Acquire) {
            self.tripped = Some(LimitViolation::Interrupted);
            return Err(LimitViolation::Interrupted.to_string());
        }
        Ok(())
    }

    /// Checks heap usage against `heap_limit_bytes`.
    #[inline]
    pub fn check_heap(&mut self, allocated_bytes: usize) -> Result<(), String> {
        if let Some(v) = &self.tripped {
            return Err(v.to_string());
        }
        if let Some(limit) = self.heap_limit_bytes
            && allocated_bytes > limit
        {
            let v = LimitViolation::HeapLimitExceeded {
                allocated: allocated_bytes,
                limit,
            };
            let msg = v.to_string();
            self.tripped = Some(v);
            return Err(msg);
        }
        Ok(())
    }

    /// Advances the instruction counter and enforces all active limits.
    #[inline]
    pub fn step(&mut self, allocated_bytes: usize) -> Result<(), String> {
        self.check_interrupt()?;
        self.instructions_executed = self.instructions_executed.saturating_add(1);
        if let Some(budget) = self.instruction_budget
            && self.instructions_executed > budget
        {
            let v = LimitViolation::InstructionBudgetExceeded {
                executed: self.instructions_executed,
                budget,
            };
            let msg = v.to_string();
            self.tripped = Some(v);
            return Err(msg);
        }
        if let Some(limit) = self.heap_limit_bytes
            && allocated_bytes > limit
        {
            let v = LimitViolation::HeapLimitExceeded {
                allocated: allocated_bytes,
                limit,
            };
            let msg = v.to_string();
            self.tripped = Some(v);
            return Err(msg);
        }
        if let Some(dl) = self.deadline
            && ((self.instructions_executed & 0x3F) == 0 || self.instructions_executed == 1)
            && Instant::now() >= dl
        {
            self.tripped = Some(LimitViolation::DeadlineExceeded);
            return Err(LimitViolation::DeadlineExceeded.to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instruction_budget_trips_and_latches() {
        let mut limits = ExecutionLimits::new().with_instruction_budget(3);
        assert!(limits.is_active());
        assert!(limits.step(0).is_ok());
        assert!(limits.step(0).is_ok());
        assert!(limits.step(0).is_ok());
        let err = limits.step(0).unwrap_err();
        assert!(err.contains("instruction budget of 3 exhausted"));
        assert!(limits.is_tripped());
    }

    #[test]
    fn test_heap_limit_trips() {
        let mut limits = ExecutionLimits::new().with_heap_limit(1024);
        assert!(limits.check_heap(512).is_ok());
        let err = limits.check_heap(2048).unwrap_err();
        assert!(err.contains("heap limit of 1024 bytes exceeded"));
        assert!(limits.is_tripped());
    }

    #[test]
    fn test_interrupt_handle_cancels_execution() {
        let mut limits = ExecutionLimits::new();
        let handle = limits.interrupt_handle();
        assert!(!limits.is_tripped());
        handle.store(true, Ordering::Release);
        assert!(limits.is_tripped());
        let err = limits.check_interrupt().unwrap_err();
        assert_eq!(err, "execution interrupted");
    }

    #[test]
    fn test_deadline_trips_when_expired() {
        let mut limits =
            ExecutionLimits::new().with_deadline(Instant::now() - Duration::from_millis(1));
        let err = limits.step(0).unwrap_err();
        assert!(err.contains("deadline exceeded"));
        assert!(limits.is_tripped());
    }
}
