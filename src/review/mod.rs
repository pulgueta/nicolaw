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

/// The second stage of the analysis, in the shape of a Jev `choice` question: for one claim
/// and one passage, the probability of each [`Decision`].
pub trait Decider: Send {
    fn decide(&mut self, claim: &str, passage: &str) -> Result<DecisionScores, ReviewError>;
}

/// The loaded analysis models.
pub struct Engines {
    pub llm: Box<dyn Llm>,
    pub nli: Box<dyn Nli>,
    pub decider: Box<dyn Decider>,
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

/// A conflict stays a conflict only when the decision is [`Decision::Contradicts`] with at
/// least this probability. This is an initial value: tune it on labeled Spanish hearing data.
pub const DECISION_MIN: f32 = 0.60;

/// The options of the decision step. The store keeps them as TEXT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The claim cannot be true if the passage is true.
    Contradicts,
    /// The passage supports or permits the claim.
    Consistent,
    /// The passage does not address the claim.
    Unrelated,
    /// The passage is not clear enough to decide (abstain).
    Insufficient,
}

impl Decision {
    pub const ALL: [Self; 4] = [
        Self::Contradicts,
        Self::Consistent,
        Self::Unrelated,
        Self::Insufficient,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Contradicts => "contradicts",
            Self::Consistent => "consistent",
            Self::Unrelated => "unrelated",
            Self::Insufficient => "insufficient",
        }
    }

    /// Returns `None` for a value that is not a decision.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|decision| decision.as_str() == value)
    }
}

/// The probabilities of the four decision options for one (claim, passage) pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecisionScores {
    pub contradicts: f32,
    pub consistent: f32,
    pub unrelated: f32,
    pub insufficient: f32,
}

impl DecisionScores {
    /// The option with the highest probability, and that probability. A tie goes to the
    /// first option in [`Decision::ALL`] order.
    pub fn best(&self) -> (Decision, f32) {
        let scores = [
            self.contradicts,
            self.consistent,
            self.unrelated,
            self.insufficient,
        ];
        let mut best = (Decision::Contradicts, scores[0]);
        for (decision, score) in Decision::ALL.into_iter().zip(scores).skip(1) {
            if score > best.1 {
                best = (decision, score);
            }
        }
        best
    }
}

/// The level after the decision step. A conflict stays a conflict only when the decision is
/// [`Decision::Contradicts`] at or above [`DECISION_MIN`]. Else it is a check.
pub fn confirm(level: SuggestionLevel, scores: DecisionScores) -> SuggestionLevel {
    // A score that is not a finite number means a failed model call: it gives no conflict.
    let finite = [
        scores.contradicts,
        scores.consistent,
        scores.unrelated,
        scores.insufficient,
    ]
    .iter()
    .all(|score| score.is_finite());
    if !finite {
        return SuggestionLevel::Check;
    }
    match (level, scores.best()) {
        (SuggestionLevel::Conflict, (Decision::Contradicts, p)) if p >= DECISION_MIN => {
            SuggestionLevel::Conflict
        }
        _ => SuggestionLevel::Check,
    }
}

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
    use super::{DECISION_MIN, Decision, DecisionScores, NliScores, confirm, decide};
    use crate::store::SuggestionLevel;

    fn decision(contradicts: f32, consistent: f32, unrelated: f32) -> DecisionScores {
        DecisionScores {
            contradicts,
            consistent,
            unrelated,
            insufficient: 1.0 - contradicts - consistent - unrelated,
        }
    }

    #[test]
    fn the_best_decision_is_the_most_probable_option() {
        assert_eq!(decision(0.7, 0.1, 0.1).best(), (Decision::Contradicts, 0.7));
        assert_eq!(decision(0.1, 0.6, 0.2).best().0, Decision::Consistent);
        assert_eq!(decision(0.1, 0.1, 0.5).best().0, Decision::Unrelated);
        assert_eq!(decision(0.1, 0.1, 0.1).best().0, Decision::Insufficient);
    }

    #[test]
    fn a_decision_reads_back_from_its_text() {
        for decision in Decision::ALL {
            assert_eq!(Decision::parse(decision.as_str()), Some(decision));
        }
        assert_eq!(Decision::parse("contradiction"), None);
    }

    #[test]
    fn a_conflict_stays_a_conflict_when_the_decision_contradicts_with_enough_probability() {
        let level = confirm(SuggestionLevel::Conflict, decision(0.60, 0.2, 0.1));

        assert_eq!(level, SuggestionLevel::Conflict);
    }

    #[test]
    fn a_conflict_becomes_a_check_when_the_contradiction_is_not_probable_enough() {
        let level = confirm(SuggestionLevel::Conflict, decision(0.59, 0.2, 0.1));

        assert_eq!(level, SuggestionLevel::Check);
    }

    #[test]
    fn a_conflict_becomes_a_check_when_an_other_option_is_best() {
        // Abstain.
        assert_eq!(
            confirm(SuggestionLevel::Conflict, decision(0.30, 0.0, 0.0)),
            SuggestionLevel::Check
        );
        assert_eq!(
            confirm(SuggestionLevel::Conflict, decision(0.1, 0.8, 0.05)),
            SuggestionLevel::Check
        );
    }

    #[test]
    fn the_decision_minimum_is_the_edge_of_a_conflict() {
        let at = decision(DECISION_MIN, 0.2, 0.1);
        let below = decision(DECISION_MIN.next_down(), 0.2, 0.1);

        assert_eq!(
            confirm(SuggestionLevel::Conflict, at),
            SuggestionLevel::Conflict
        );
        assert_eq!(
            confirm(SuggestionLevel::Conflict, below),
            SuggestionLevel::Check
        );
        assert_eq!(
            confirm(SuggestionLevel::Conflict, decision(1.0, 0.0, 0.0)),
            SuggestionLevel::Conflict
        );
    }

    #[test]
    fn scores_that_are_not_finite_never_give_a_conflict() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let in_contradicts = DecisionScores {
                contradicts: bad,
                ..decision(0.9, 0.05, 0.05)
            };
            let in_other = DecisionScores {
                consistent: bad,
                ..decision(0.9, 0.05, 0.05)
            };

            for scores in [in_contradicts, in_other] {
                assert_eq!(
                    confirm(SuggestionLevel::Conflict, scores),
                    SuggestionLevel::Check,
                    "{scores:?}"
                );
            }
        }
    }

    #[test]
    fn a_check_stays_a_check_even_when_the_decision_contradicts() {
        let level = confirm(SuggestionLevel::Check, decision(0.95, 0.02, 0.02));

        assert_eq!(level, SuggestionLevel::Check);
    }

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
