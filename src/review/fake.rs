//! Fake language and NLI models for tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::{Engines, Llm, Nli, NliScores, ReviewBackend, ReviewError};

/// A language model with fixed answers. It records each call as `claims|<question>|<text>` or
/// `explain|<claim>|<passage>`.
#[derive(Clone)]
pub struct FakeLlm {
    pub claims: String,
    pub explanation: String,
    pub fail: bool,
    /// The explanation call with this index (from 0), and each call after it, fails.
    pub fail_explain_from: Option<usize>,
    /// Runs during each explanation call, for example to change the store while the model
    /// works.
    pub on_explain: Option<Arc<dyn Fn() + Send + Sync>>,
    pub calls: Arc<Mutex<Vec<String>>>,
    /// Held by the models of one load, so that the backend counts the loaded copies.
    loaded: Option<Arc<Loaded>>,
}

/// One loaded copy of the fake models. Drop counts it as free.
struct Loaded(Arc<AtomicUsize>);

impl Drop for Loaded {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl FakeLlm {
    pub fn new(claims: &str, explanation: &str) -> Self {
        Self {
            claims: claims.to_owned(),
            explanation: explanation.to_owned(),
            fail: false,
            fail_explain_from: None,
            on_explain: None,
            calls: Arc::default(),
            loaded: None,
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
        let mut calls = self.calls.lock().unwrap();
        let index = calls
            .iter()
            .filter(|call| call.starts_with("explain|"))
            .count();
        calls.push(format!("explain|{claim}|{passage}"));
        drop(calls);
        if let Some(on_explain) = &self.on_explain {
            on_explain();
        }
        if self.fail_explain_from.is_some_and(|from| index >= from) {
            return Err(ReviewError::Inference);
        }
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

/// The NLI files and the language model files of one load.
type LoadedFiles = (Vec<PathBuf>, Vec<PathBuf>);

/// Loads the fake models. It records the files of the last load.
#[derive(Clone)]
pub struct FakeReviewBackend {
    pub llm: FakeLlm,
    pub nli: FakeNli,
    /// When true, the load fails as with damaged model files.
    pub fail: bool,
    pub loaded: Arc<Mutex<Option<LoadedFiles>>>,
    /// The number of loads.
    pub loads: Arc<AtomicUsize>,
    /// The number of loaded copies now, and the largest number at one time.
    pub copies: Arc<AtomicUsize>,
    pub max_copies: Arc<AtomicUsize>,
}

impl FakeReviewBackend {
    pub fn new(llm: FakeLlm, nli: FakeNli) -> Self {
        Self {
            llm,
            nli,
            fail: false,
            loaded: Arc::default(),
            loads: Arc::default(),
            copies: Arc::default(),
            max_copies: Arc::default(),
        }
    }
}

impl ReviewBackend for FakeReviewBackend {
    fn load(&self, nli: &[PathBuf], llm: &[PathBuf]) -> Result<Engines, ReviewError> {
        *self.loaded.lock().unwrap() = Some((nli.to_vec(), llm.to_vec()));
        self.loads.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(ReviewError::ModelLoad);
        }
        let copies = self.copies.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_copies.fetch_max(copies, Ordering::SeqCst);
        let mut llm = self.llm.clone();
        llm.loaded = Some(Arc::new(Loaded(self.copies.clone())));
        Ok(Engines {
            llm: Box::new(llm),
            nli: Box::new(self.nli.clone()),
        })
    }
}
