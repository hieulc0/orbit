//! Per-role repository resources, separate from provider token accounting.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleBudget {
    pub max_total_calls: u64,
    pub max_mutating_calls: u64,
    pub max_terminal_calls: u64,
    pub max_file_read_bytes: u64,
    pub max_output_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleUsage {
    pub total_calls: u64,
    pub mutating_calls: u64,
    pub terminal_calls: u64,
    pub file_read_bytes: u64,
    pub output_bytes: u64,
    pub exhausted: bool,
}

#[derive(Debug)]
pub struct ToolBudgetExhausted;
impl std::fmt::Display for ToolBudgetExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TOOL_BUDGET_EXHAUSTED")
    }
}
impl std::error::Error for ToolBudgetExhausted {}

impl RoleBudget {
    pub fn for_role(role: &str) -> Self {
        let implementer = role == "implementer";
        Self {
            max_total_calls: if implementer { 300 } else { 150 },
            max_mutating_calls: if implementer { 200 } else { 0 },
            max_terminal_calls: if implementer { 40 } else { 0 },
            max_file_read_bytes: if implementer {
                16 * 1024 * 1024
            } else {
                8 * 1024 * 1024
            },
            max_output_bytes: 8 * 1024 * 1024,
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=1024).contains(&self.max_total_calls)
                && self.max_mutating_calls <= self.max_total_calls
                && self.max_terminal_calls <= self.max_mutating_calls
                && (1..=64 * 1024 * 1024).contains(&self.max_file_read_bytes)
                && (4096..=8 * 1024 * 1024).contains(&self.max_output_bytes),
            "invalid role budget"
        );
        Ok(())
    }
    pub fn reserve_call(
        &self,
        usage: &mut RoleUsage,
        mutating: bool,
        terminal: bool,
    ) -> Result<()> {
        self.validate()?;
        if usage.exhausted
            || usage.total_calls >= self.max_total_calls
            || (mutating && usage.mutating_calls >= self.max_mutating_calls)
            || (terminal && usage.terminal_calls >= self.max_terminal_calls)
            || self.max_output_bytes.saturating_sub(usage.output_bytes) < 256
        {
            usage.exhausted = true;
            return Err(ToolBudgetExhausted.into());
        }
        usage.total_calls += 1;
        usage.mutating_calls += u64::from(mutating);
        usage.terminal_calls += u64::from(terminal);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservations_are_atomic_and_exhaustion_sticks() -> Result<()> {
        let mut budget = RoleBudget::for_role("implementer");
        budget.max_mutating_calls = 1;
        budget.max_terminal_calls = 1;
        let mut usage = RoleUsage::default();
        budget.reserve_call(&mut usage, true, true)?;
        assert!(
            budget
                .reserve_call(&mut usage, true, false)
                .unwrap_err()
                .is::<ToolBudgetExhausted>()
        );
        assert_eq!(usage.total_calls, 1);
        assert_eq!(usage.mutating_calls, 1);
        assert!(budget.reserve_call(&mut usage, false, false).is_err());
        Ok(())
    }
}
