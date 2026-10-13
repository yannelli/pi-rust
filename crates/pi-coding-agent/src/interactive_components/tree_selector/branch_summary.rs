//! The branch summary prompt that follows a tree selection (upstream `showTreeSelector` in
//! `interactive-mode.ts`): skip when already there, ask about a summary of the abandoned
//! branch, and optionally collect custom summarization instructions.

use crate::agent_session::NavigateTreeOptions;

/// Title of the summary choice selector.
pub const BRANCH_SUMMARY_PROMPT_TITLE: &str = "Summarize branch?";
/// Title of the custom instructions editor.
pub const CUSTOM_INSTRUCTIONS_TITLE: &str = "Custom summarization instructions";
/// Status shown when the selected entry is already the current leaf.
pub const ALREADY_AT_TARGET_STATUS: &str = "Already at this point";

/// An option of the summary selector, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchSummaryChoice {
    NoSummary,
    Summarize,
    SummarizeWithCustomPrompt,
}

impl BranchSummaryChoice {
    pub const ALL: [Self; 3] = [Self::NoSummary, Self::Summarize, Self::SummarizeWithCustomPrompt];

    pub fn label(self) -> &'static str {
        match self {
            Self::NoSummary => "No summary",
            Self::Summarize => "Summarize",
            Self::SummarizeWithCustomPrompt => "Summarize with custom prompt",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|choice| choice.label() == label)
    }

    /// The selector options.
    pub fn options() -> Vec<String> {
        Self::ALL.iter().map(|choice| choice.label().to_string()).collect()
    }
}

/// What the host does next.
#[derive(Clone, Debug, PartialEq)]
pub enum BranchSummaryStep {
    /// The target is the current leaf: close the tree and show [`ALREADY_AT_TARGET_STATUS`].
    AlreadyAtTarget,
    /// Show a selector titled [`BRANCH_SUMMARY_PROMPT_TITLE`] with [`BranchSummaryChoice::options`],
    /// then call [`BranchSummaryFlow::choose`].
    ChooseSummary,
    /// Show an editor titled [`CUSTOM_INSTRUCTIONS_TITLE`], then call
    /// [`BranchSummaryFlow::custom_instructions`].
    EditInstructions,
    /// The selector was dismissed: reopen the tree with `entry_id` selected.
    ReopenTree { entry_id: String },
    /// The choice is complete: navigate to `entry_id` with these options.
    Navigate { entry_id: String, options: NavigateTreeOptions },
}

/// Drives the prompt between a tree selection and `AgentSession::navigate_tree`.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchSummaryFlow {
    entry_id: String,
    wants_summary: bool,
}

impl BranchSummaryFlow {
    /// Starts after the tree reported `entry_id`. With `skip_prompt` (the
    /// `branchSummary.skipPrompt` setting) the flow navigates without a summary.
    pub fn start(entry_id: &str, current_leaf_id: Option<&str>, skip_prompt: bool) -> (Self, BranchSummaryStep) {
        let flow = Self { entry_id: entry_id.to_string(), wants_summary: false };
        let step = if current_leaf_id == Some(entry_id) {
            BranchSummaryStep::AlreadyAtTarget
        } else if skip_prompt {
            flow.navigate(None)
        } else {
            BranchSummaryStep::ChooseSummary
        };
        (flow, step)
    }

    pub fn entry_id(&self) -> &str {
        &self.entry_id
    }

    fn navigate(&self, custom_instructions: Option<String>) -> BranchSummaryStep {
        let options = NavigateTreeOptions { summarize: self.wants_summary, custom_instructions, ..Default::default() };
        BranchSummaryStep::Navigate { entry_id: self.entry_id.clone(), options }
    }

    /// The selector result: an option label, or `None` when dismissed.
    pub fn choose(&mut self, choice: Option<&str>) -> BranchSummaryStep {
        let Some(choice) = choice else {
            return BranchSummaryStep::ReopenTree { entry_id: self.entry_id.clone() };
        };
        self.wants_summary = choice != BranchSummaryChoice::NoSummary.label();
        if choice == BranchSummaryChoice::SummarizeWithCustomPrompt.label() {
            return BranchSummaryStep::EditInstructions;
        }
        self.navigate(None)
    }

    /// The editor result: the instructions, or `None` when cancelled (back to the selector).
    pub fn custom_instructions(&mut self, instructions: Option<String>) -> BranchSummaryStep {
        match instructions {
            Some(instructions) => self.navigate(Some(instructions)),
            None => BranchSummaryStep::ChooseSummary,
        }
    }
}
