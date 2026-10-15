//! Fake language and NLI models for tests.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::{Engines, Llm, Nli, NliScores, ReviewBackend, ReviewError};

/// A language model with fixed answers. It records each call as `claims|<question>|<text>` or
/// `explain|<claim>|<passage>`.
#[derive(Clone)]
pub struct FakeLlm {
    pub claims: String,
    pub explanation: String,
    pub fail: bool,
    pub calls: Arc<Mutex<Vec<String>>>,
}

impl FakeLlm {
    pub fn new(claims: &str, explanation: &str) -> Self {
        Self {
            claims: claims.to_owned(),
            explanation: explanation.to_owned(),
            fail: false,
            calls: Arc::default(),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Llm for FakeLlm {
    fn claims(&mut self, question: Option<&str>, statement: &str) -> Result<String, ReviewError> {
        self.calls.lock().unwrap().push(format!(
            "claims|{}|{statement}",
            question.unwrap_or_default()
        ));
        if self.fail {
            return Err(ReviewError::Inference);
        }
        Ok(self.claims.clone())
    }

    fn explain(&mut self, claim: &str, passage: &str) -> Result<String, ReviewError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("explain|{claim}|{passage}"));
        Ok(self.explanation.clone())
    }
}

/// An NLI model that gives `contradiction` when the premise or the hypothesis has `word`, and
/// a neutral result for other pairs.
#[derive(Clone)]
pub struct FakeNli {
    pub word: String,
    pub contradiction: f32,
}

impl Nli for FakeNli {
    fn score(&mut self, premise: &str, hypothesis: &str) -> Result<NliScores, ReviewError> {
        Ok(
            if premise.contains(&self.word) || hypothesis.contains(&self.word) {
                NliScores {
                    entailment: 0.01,
                    neutral: 0.99 - self.contradiction,
                    contradiction: self.contradiction,
                }
            } else {
                NliScores {
                    entailment: 0.1,
                    neutral: 0.85,
                    contradiction: 0.05,
                }
            },
        )
    }
}

/// Loads the fake models. It records the files of the last load.
#[derive(Clone)]
pub struct FakeReviewBackend {
    pub llm: FakeLlm,
    pub nli: FakeNli,
    pub loaded: Arc<Mutex<Option<(Vec<PathBuf>, Vec<PathBuf>)>>>,
}

impl FakeReviewBackend {
    pub fn new(llm: FakeLlm, nli: FakeNli) -> Self {
        Self {
            llm,
            nli,
            loaded: Arc::default(),
        }
    }
}

impl ReviewBackend for FakeReviewBackend {
    fn load(&self, nli: &[PathBuf], llm: &[PathBuf]) -> Result<Engines, ReviewError> {
        *self.loaded.lock().unwrap() = Some((nli.to_vec(), llm.to_vec()));
        Ok(Engines {
            llm: Box::new(self.llm.clone()),
            nli: Box::new(self.nli.clone()),
        })
    }
}
