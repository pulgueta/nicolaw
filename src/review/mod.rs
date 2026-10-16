//! The analysis of a hearing: claims of the accused, retrieval of evidence passages, NLI
//! scoring, explanation, and suggestions.

pub mod analyzer;
#[cfg(test)]
pub mod fake;
pub mod native;
pub mod parse;
pub mod prompt;
pub mod query;

use std::path::PathBuf;

use crate::store::{AnalysisState, Suggestion, SuggestionLevel};

/// What the analysis reports to the UI.
#[derive(Clone, Debug, PartialEq)]
pub enum ReviewEvent {
    SuggestionAdded(Suggestion),
    /// The analysis of a segment has a new state.
    Analyzed {
        hearing_id: i64,
        segment_id: i64,
        state: AnalysisState,
    },
    /// The number of segments that wait for analysis (with the segment in analysis), and the
    /// number of segments that the full queue dropped.
    Progress {
        pending: usize,
        dropped: u64,
    },
    /// The analysis models are not installed. The hearing records and transcribes without them.
    ModelsMissing,
    /// A message for the user.
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewError {
    ModelLoad,
    Inference,
    Storage,
}

impl ReviewError {
    /// The message for the user.
    pub fn message(self) -> &'static str {
        match self {
            Self::ModelLoad => "Nicolaw no pudo cargar el modelo.",
            Self::Inference => "Nicolaw no puede analizar una declaración del acusado.",
            Self::Storage => "Nicolaw no puede guardar las sugerencias.",
        }
    }
}

impl From<rusqlite::Error> for ReviewError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Storage
    }
}

/// The language model. Each function returns the raw output of the model; [`parse`] reads and
/// validates it. Evidence and transcript text go into the prompt as data, never as
/// instructions.
pub trait Llm: Send {
    /// A JSON array of strings: the claims of the statement, as standalone Spanish sentences.
    /// `question` is the segment of an other speaker before the statement, for context.
    fn claims(&mut self, question: Option<&str>, statement: &str) -> Result<String, ReviewError>;
    /// A JSON object that explains whether the passage contradicts the claim.
    fn explain(&mut self, claim: &str, passage: &str) -> Result<String, ReviewError>;
}

/// Natural language inference: does the premise contradict the hypothesis?
pub trait Nli: Send {
    fn score(&mut self, premise: &str, hypothesis: &str) -> Result<NliScores, ReviewError>;

    /// True when the pair fits the model input with no truncation. Without a tokenizer, the
    /// pair has at most [`parse::MAX_PAIR_WORDS`] words.
    fn fits(&self, premise: &str, hypothesis: &str) -> bool {
        premise.split_whitespace().count() + hypothesis.split_whitespace().count()
            <= parse::MAX_PAIR_WORDS
    }
}

/// The loaded analysis models.
pub struct Engines {
    pub llm: Box<dyn Llm>,
    pub nli: Box<dyn Nli>,
}

/// Loads the analysis models. The app uses [`native::NativeReview`]; tests use a fake. The
/// function blocks: do not call it on the UI thread.
pub trait ReviewBackend: Send + Sync {
    /// `nli` and `llm` are the files of [`crate::models::NLI`] and [`crate::models::LLM`], in
    /// catalog order.
    fn load(&self, nli: &[PathBuf], llm: &[PathBuf]) -> Result<Engines, ReviewError>;
}

// The thresholds of the NLI decision. These are initial values from the research for this
// layer; tune them on real hearings and real evidence.
/// A pair is a conflict at or above this probability of contradiction...
pub const CONFLICT_MIN: f32 = 0.85;
/// ...when the contradiction is also this much above the entailment.
pub const CONFLICT_MARGIN: f32 = 0.5;
/// A pair needs a check at or above this probability of contradiction.
pub const CHECK_MIN: f32 = 0.60;

/// The probabilities of the three NLI labels for one (premise, hypothesis) pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NliScores {
    pub entailment: f32,
    pub neutral: f32,
    pub contradiction: f32,
}

impl NliScores {
    /// The maximum of each label. The analysis scores each pair in both directions (evidence
    /// as premise, then claim as premise) and keeps the maxima.
    pub fn max(self, other: Self) -> Self {
        Self {
            entailment: self.entailment.max(other.entailment),
            neutral: self.neutral.max(other.neutral),
            contradiction: self.contradiction.max(other.contradiction),
        }
    }
}

/// The level of a pair from its NLI scores, or `None` when the pair is not flagged.
pub fn decide(scores: NliScores) -> Option<SuggestionLevel> {
    let contradiction = scores.contradiction;
    if contradiction >= CONFLICT_MIN && contradiction - scores.entailment >= CONFLICT_MARGIN {
        Some(SuggestionLevel::Conflict)
    } else if contradiction >= CHECK_MIN {
        Some(SuggestionLevel::Check)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{NliScores, decide};
    use crate::store::SuggestionLevel;

    fn scores(entailment: f32, contradiction: f32) -> NliScores {
        NliScores {
            entailment,
            neutral: 1.0 - entailment - contradiction,
            contradiction,
        }
    }

    #[test]
    fn a_strong_contradiction_far_above_entailment_is_a_conflict() {
        assert_eq!(decide(scores(0.02, 0.95)), Some(SuggestionLevel::Conflict));
        assert_eq!(decide(scores(0.35, 0.85)), Some(SuggestionLevel::Conflict));
    }

    #[test]
    fn a_strong_contradiction_close_to_entailment_is_a_check() {
        // Possible when the two directions disagree: the maxima come from different pairs.
        assert_eq!(decide(scores(0.40, 0.88)), Some(SuggestionLevel::Check));
    }

    #[test]
    fn a_medium_contradiction_is_a_check() {
        assert_eq!(decide(scores(0.10, 0.60)), Some(SuggestionLevel::Check));
        assert_eq!(decide(scores(0.05, 0.84)), Some(SuggestionLevel::Check));
    }

    #[test]
    fn a_weak_contradiction_is_nothing() {
        assert_eq!(decide(scores(0.10, 0.59)), None);
        assert_eq!(decide(scores(0.90, 0.05)), None);
    }

    #[test]
    fn both_directions_keep_the_maximum_of_each_label() {
        let forward = scores(0.30, 0.20);
        let backward = scores(0.05, 0.90);

        let both = forward.max(backward);

        assert_eq!((both.entailment, both.contradiction), (0.30, 0.90));
    }
}
