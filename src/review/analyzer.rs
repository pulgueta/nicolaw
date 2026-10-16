//! The analysis of one segment, and the bounded queue of segments that wait for it.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_channel::TrySendError;

use super::parse::{Explanation, parse_claims, parse_explanation, premises};
use super::query::query_terms;
use super::{Engines, Llm, Nli, ReviewError, ReviewEvent, decide};
use crate::store::{AnalysisState, NewSuggestion, Segment, Speaker, Store, SuggestionLevel};

/// The search gives at most this number of chunks for each claim.
const SEARCH_LIMIT: usize = 20;
/// Each claim gives at most this number of suggestions, the strongest first, so that the queue
/// stays quiet and the language model explains few pairs.
const MAX_SUGGESTIONS_PER_CLAIM: usize = 2;
/// The explanation of a check when the explanation of the language model is not valid.
pub const UNVERIFIED_NOTE: &str =
    "Nicolaw no puede verificar la explicación. Lea el pasaje de la prueba.";

/// One final segment that waits for analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub case_id: i64,
    pub hearing_id: i64,
    pub segment_id: i64,
    /// The [`text_hash`] of the segment text when the job was made. If the text changes (the
    /// refine pass), the job is old and the analysis skips it.
    pub text_hash: u64,
}

pub fn text_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// A pair of a claim and a passage that the NLI model flagged.
struct Flagged {
    level: SuggestionLevel,
    contradiction: f32,
    chunk_id: i64,
    chunk_text: String,
    premise: String,
}

/// Finds suggestions for the claims of the accused. Synchronous, so that tests drive it; the
/// app runs it with [`run`] on a background task.
pub struct Analyzer {
    store: Arc<Mutex<Store>>,
    llm: Box<dyn Llm>,
    nli: Box<dyn Nli>,
    /// When true, the analysis stops before the next model call or the next insert.
    cancel: Arc<AtomicBool>,
    /// The right to keep the models loaded. It is the last field, so that it goes after the
    /// models.
    _permit: Option<Permit>,
}

/// Why the analysis of a segment stopped before its suggestions were stored.
enum Stop {
    /// A model or the storage failed.
    Error(ReviewError),
    /// The output of the language model is not valid.
    Malformed,
    Cancelled,
}

impl From<ReviewError> for Stop {
    fn from(error: ReviewError) -> Self {
        Self::Error(error)
    }
}

impl From<rusqlite::Error> for Stop {
    fn from(error: rusqlite::Error) -> Self {
        Self::Error(error.into())
    }
}

impl Analyzer {
    pub fn new(store: Arc<Mutex<Store>>, engines: Engines) -> Self {
        Self {
            store,
            llm: engines.llm,
            nli: engines.nli,
            cancel: Arc::default(),
            _permit: None,
        }
    }

    /// The analysis stops when `cancel` becomes true.
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = cancel;
        self
    }

    /// The analyzer keeps `permit` until it is dropped, after its models.
    pub fn with_permit(mut self, permit: Permit) -> Self {
        self._permit = Some(permit);
        self
    }

    /// Analyzes one segment: claims, retrieval, NLI, explanation, and storage. Returns the new
    /// suggestions and the analysis state of the segment, then an error for the user if the
    /// storage failed. A segment of an other speaker, or a segment whose text changed, gives
    /// nothing. The suggestions are stored only when the full analysis succeeds; earlier
    /// suggestions of the segment stay (see [`Store::replace_suggestions`]).
    pub fn analyze(&mut self, job: &Job) -> Vec<ReviewEvent> {
        let mut events = Vec::new();
        let segment = if self.cancelled() {
            Err(Stop::Cancelled)
        } else {
            self.current(job).map_err(Stop::from)
        };
        let segment = match segment {
            Ok(Some(segment)) => segment,
            Ok(None) => return events,
            Err(stop) => {
                self.stopped(job, None, stop, &mut events);
                return events;
            }
        };
        match self.try_analyze(job, &segment) {
            Ok((new, state)) => {
                let stored = self.store.lock().unwrap().replace_suggestions(
                    job.segment_id,
                    segment.revision,
                    &new,
                    state,
                );
                match stored {
                    // The text or the speaker changed during the analysis: a new job comes.
                    Ok(None) => {}
                    Ok(Some(stored)) => {
                        events.extend(stored.into_iter().map(ReviewEvent::SuggestionAdded));
                        events.push(analyzed(job, state));
                    }
                    Err(error) => {
                        self.stopped(job, Some(segment.revision), error.into(), &mut events)
                    }
                }
            }
            Err(stop) => self.stopped(job, Some(segment.revision), stop, &mut events),
        }
        events
    }

    /// Stores the state of an analysis that stopped. A storage error is also reported to
    /// the user; a model error shows only in the state of the segment.
    fn stopped(&self, job: &Job, revision: Option<i64>, stop: Stop, events: &mut Vec<ReviewEvent>) {
        let state = match stop {
            Stop::Cancelled => AnalysisState::Skipped,
            Stop::Malformed | Stop::Error(_) => AnalysisState::Failed,
        };
        let saved = self
            .store
            .lock()
            .unwrap()
            .set_analysis_state(job.segment_id, revision, state);
        if matches!(saved, Ok(true)) {
            events.push(analyzed(job, state));
        }
        if matches!(stop, Stop::Error(ReviewError::Storage)) || saved.is_err() {
            events.push(ReviewEvent::Error(ReviewError::Storage.message().into()));
        }
    }

    /// The suggestions of the segment and its state, or why the analysis stopped.
    fn try_analyze(
        &mut self,
        job: &Job,
        segment: &Segment,
    ) -> Result<(Vec<NewSuggestion>, AnalysisState), Stop> {
        let question = self
            .store
            .lock()
            .unwrap()
            .question_before(job.hearing_id, segment.t_start_ms)?;
        let output = self.llm.claims(question.as_deref(), &segment.text)?;
        let claims = parse_claims(&output).ok_or(Stop::Malformed)?;
        if claims.is_empty() {
            return Ok((Vec::new(), AnalysisState::NoClaim));
        }
        let mut new = Vec::new();
        let mut found = false;
        for claim in claims {
            let (hits, flagged) = self.flag(job.case_id, &claim)?;
            found |= hits > 0;
            for flagged in flagged {
                if self.cancelled() {
                    return Err(Stop::Cancelled);
                }
                let output = self.llm.explain(&claim, &flagged.premise)?;
                let explanation = parse_explanation(&output, &claim, &flagged.chunk_text);
                new.push(suggestion(job, &claim, flagged, explanation));
            }
        }
        if self.cancelled() {
            return Err(Stop::Cancelled);
        }
        let state = if found {
            AnalysisState::Complete
        } else {
            AnalysisState::NoSource
        };
        Ok((new, state))
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The segment of the job, if it is a final segment of the accused with the same text.
    fn current(&self, job: &Job) -> Result<Option<Segment>, ReviewError> {
        let segment = match self.store.lock().unwrap().segment(job.segment_id) {
            Ok(segment) => segment,
            // The hearing or the segment was deleted.
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let current = segment.speaker == Speaker::Accused
            && segment.is_final
            && text_hash(&segment.text) == job.text_hash;
        Ok(current.then_some(segment))
    }

    /// The number of passages that the search found for the claim, and the passages that the
    /// NLI model flags, strongest first, one for each chunk, at most
    /// [`MAX_SUGGESTIONS_PER_CLAIM`].
    fn flag(&mut self, case_id: i64, claim: &str) -> Result<(usize, Vec<Flagged>), Stop> {
        let hits =
            self.store
                .lock()
                .unwrap()
                .search_any(case_id, &query_terms(claim), SEARCH_LIMIT)?;
        let count = hits.len();
        let mut flagged: Vec<Flagged> = Vec::new();
        for hit in hits {
            let chunk = self.store.lock().unwrap().chunk(hit.chunk_id)?;
            let mut best: Option<Flagged> = None;
            for premise in premises(&chunk.text, |premise| self.nli.fits(premise, claim)) {
                if self.cancelled() {
                    return Err(Stop::Cancelled);
                }
                // Both directions: the evidence as premise, then the claim as premise.
                let scores = self
                    .nli
                    .score(&premise, claim)?
                    .max(self.nli.score(claim, &premise)?);
                let Some(level) = decide(scores) else {
                    continue;
                };
                if best
                    .as_ref()
                    .is_none_or(|best| scores.contradiction > best.contradiction)
                {
                    best = Some(Flagged {
                        level,
                        contradiction: scores.contradiction,
                        chunk_id: chunk.id,
                        chunk_text: chunk.text.clone(),
                        premise,
                    });
                }
            }
            flagged.extend(best);
        }
        flagged.sort_by(|a, b| b.contradiction.total_cmp(&a.contradiction));
        flagged.truncate(MAX_SUGGESTIONS_PER_CLAIM);
        Ok((count, flagged))
    }
}

fn analyzed(job: &Job, state: AnalysisState) -> ReviewEvent {
    ReviewEvent::Analyzed {
        hearing_id: job.hearing_id,
        segment_id: job.segment_id,
        state,
    }
}

/// A conflict needs the NLI model and the language model to agree. Every other flagged pair is
/// a check. Without a valid explanation, the suggestion shows the full premise and a note.
fn suggestion(
    job: &Job,
    claim: &str,
    flagged: Flagged,
    explanation: Option<Explanation>,
) -> NewSuggestion {
    let base = NewSuggestion {
        hearing_id: job.hearing_id,
        segment_id: job.segment_id,
        chunk_id: flagged.chunk_id,
        claim: claim.to_owned(),
        claim_quote: None,
        evidence_quote: flagged.premise,
        kind: None,
        level: SuggestionLevel::Check,
        nli_contradiction: flagged.contradiction as f64,
        explanation: Some(UNVERIFIED_NOTE.to_owned()),
        // The store sets the revision that the analysis read.
        segment_revision: 0,
    };
    let Some(explanation) = explanation else {
        return base;
    };
    let conflict = flagged.level == SuggestionLevel::Conflict && explanation.contradiction;
    NewSuggestion {
        claim_quote: Some(explanation.quote_claim),
        evidence_quote: explanation.quote_evidence,
        kind: Some(explanation.kind),
        level: if conflict {
            SuggestionLevel::Conflict
        } else {
            SuggestionLevel::Check
        },
        explanation: Some(explanation.explanation),
        ..base
    }
}

/// The segments that wait for analysis. It never blocks the transcription: when it is full, a
/// new job replaces the oldest job, and the queue counts the dropped jobs.
#[derive(Clone)]
pub struct JobQueue {
    sender: async_channel::Sender<Job>,
    receiver: async_channel::Receiver<Job>,
    dropped: Arc<AtomicU64>,
}

impl JobQueue {
    pub fn new(capacity: usize) -> Self {
        let (sender, receiver) = async_channel::bounded(capacity);
        Self {
            sender,
            receiver,
            dropped: Arc::default(),
        }
    }

    /// Adds the job. Returns the jobs that the full queue dropped.
    pub fn push(&self, mut job: Job) -> Vec<Job> {
        // A new job for a segment replaces its pending job: the text of the segment changed.
        let pending: Vec<Job> = std::iter::from_fn(|| self.receiver.try_recv().ok()).collect();
        for old in pending {
            if old.segment_id != job.segment_id {
                self.sender.try_send(old).ok();
            }
        }
        let mut dropped = Vec::new();
        loop {
            match self.sender.try_send(job) {
                Ok(()) | Err(TrySendError::Closed(_)) => return dropped,
                Err(TrySendError::Full(back)) => {
                    if let Ok(old) = self.receiver.try_recv() {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        dropped.push(old);
                    }
                    job = back;
                }
            }
        }
    }

    /// Removes and returns the jobs that wait.
    pub fn drain(&self) -> Vec<Job> {
        std::iter::from_fn(|| self.receiver.try_recv().ok()).collect()
    }

    /// True if `other` is a clone of this queue.
    pub fn same(&self, other: &Self) -> bool {
        self.sender.same_channel(&other.sender)
    }

    pub fn pending(&self) -> usize {
        self.receiver.len()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn try_next(&self) -> Option<Job> {
        self.receiver.try_recv().ok()
    }

    /// Waits for the next job. Returns `None` after [`JobQueue::close`].
    pub async fn next(&self) -> Option<Job> {
        self.receiver.recv().await.ok()
    }

    /// Stops the queue. The jobs in it are not analyzed.
    pub fn close(&self) {
        self.sender.close();
    }
}

/// Puts a final segment of the accused into the analysis queue, as `waiting`. Without a
/// queue (the analysis is off), the segment is `skipped`. A job that the full queue drops is
/// also `skipped`. Returns the new state of each changed segment.
pub fn submit(
    store: &Mutex<Store>,
    queue: Option<&JobQueue>,
    job: Job,
) -> rusqlite::Result<Vec<(Job, AnalysisState)>> {
    let store = store.lock().unwrap();
    let mut changed = Vec::new();
    let Some(queue) = queue else {
        store.set_analysis_state(job.segment_id, None, AnalysisState::Skipped)?;
        changed.push((job, AnalysisState::Skipped));
        return Ok(changed);
    };
    store.set_analysis_state(job.segment_id, None, AnalysisState::Waiting)?;
    changed.push((job.clone(), AnalysisState::Waiting));
    for dropped in queue.push(job) {
        store.set_analysis_state(dropped.segment_id, None, AnalysisState::Skipped)?;
        changed.push((dropped, AnalysisState::Skipped));
    }
    Ok(changed)
}

/// Allows one copy of the analysis models at a time. A new load waits until the models of the
/// last load are free, so that a quick off and on does not load a second copy while a model
/// call still runs.
#[derive(Clone)]
pub struct ModelGate {
    sender: async_channel::Sender<()>,
    receiver: async_channel::Receiver<()>,
}

/// The right to have the analysis models loaded. Drop gives it back.
pub struct Permit(async_channel::Receiver<()>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.try_recv().ok();
    }
}

impl Default for ModelGate {
    fn default() -> Self {
        let (sender, receiver) = async_channel::bounded(1);
        Self { sender, receiver }
    }
}

impl ModelGate {
    /// Waits until no other permit exists.
    pub async fn acquire(&self) -> Permit {
        self.sender.send(()).await.ok();
        Permit(self.receiver.clone())
    }

    /// A permit if no other permit exists.
    #[cfg(test)]
    pub fn try_acquire(&self) -> Option<Permit> {
        self.sender
            .try_send(())
            .ok()
            .map(|()| Permit(self.receiver.clone()))
    }
}

/// Analyzes the jobs of the queue until it closes or the UI stops listening.
pub async fn run(
    mut analyzer: Analyzer,
    queue: JobQueue,
    events: async_channel::Sender<ReviewEvent>,
) {
    while let Some(job) = queue.next().await {
        let progress = |pending| ReviewEvent::Progress {
            pending,
            dropped: queue.dropped(),
        };
        if events.send(progress(queue.pending() + 1)).await.is_err() {
            return;
        }
        for event in analyzer.analyze(&job) {
            events.send(event).await.ok();
        }
        if events.send(progress(queue.pending())).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use super::{Analyzer, Job, JobQueue, ModelGate, submit, text_hash};
    use crate::extract::{Page, PageSource};
    use crate::review::fake::{FakeLlm, FakeNli};
    use crate::review::{Engines, ReviewEvent};
    use crate::store::{
        AnalysisState, Speaker, Store, SuggestionKind, SuggestionLevel, SuggestionStatus,
    };

    const QUESTION: &str = "¿Dónde estaba usted el 12 de marzo?";
    const ANSWER: &str = "Ese día yo estaba en Medellín.";
    const CLAIM: &str = "El acusado estaba en Medellín el 12 de marzo.";
    const HOTEL: &str =
        "Según el registro del hotel, el acusado estaba en Bogotá el 12 de marzo de 2024.";

    struct Case {
        store: Arc<Mutex<Store>>,
        case: i64,
        hearing: i64,
        question: i64,
        answer: i64,
    }

    fn page(number: u32, text: &str) -> Page {
        Page {
            number,
            text: text.into(),
            source: PageSource::Text,
        }
    }

    /// A case with a hotel register (page 1 has the stay in Bogotá), and a hearing with a
    /// question and the answer of the accused.
    fn seeded(name: &str) -> Case {
        let store = Store::open_in_memory().unwrap();
        let case = store.create_case("Caso").unwrap().id;
        let dir =
            std::env::temp_dir().join(format!("nicolaw-review-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registro.txt");
        fs::write(&path, "x").unwrap();
        let evidence = store.import(case, &path, |_, _| {}).unwrap().id;
        store
            .save_analysis(
                evidence,
                &[
                    page(1, HOTEL),
                    page(2, "El testigo vio un carro rojo frente a la casa."),
                ],
            )
            .unwrap();
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let add = |t: i64, speaker, text: &str| {
            let id = store.insert_segment(hearing, t, speaker).unwrap();
            store.finish_segment(id, text, t + 900).unwrap();
            id
        };
        let question = add(0, Speaker::Other, QUESTION);
        let answer = add(1_000, Speaker::Accused, ANSWER);
        Case {
            store: Arc::new(Mutex::new(store)),
            case,
            hearing,
            question,
            answer,
        }
    }

    fn explanation(contradiction: bool, quote_claim: &str, quote_evidence: &str) -> String {
        serde_json::json!({
            "contradiction": contradiction,
            "kind": "location",
            "explanation_es": "El registro del hotel ubica al acusado en Bogotá ese día.",
            "quote_claim": quote_claim,
            "quote_evidence": quote_evidence,
        })
        .to_string()
    }

    fn llm(explanation: &str) -> FakeLlm {
        FakeLlm::new(&serde_json::json!([CLAIM]).to_string(), explanation)
    }

    fn nli(contradiction: f32) -> FakeNli {
        FakeNli {
            word: "Bogotá".into(),
            contradiction,
        }
    }

    fn analyzer(case: &Case, llm: &FakeLlm, nli: FakeNli) -> Analyzer {
        Analyzer::new(
            case.store.clone(),
            Engines {
                llm: Box::new(llm.clone()),
                nli: Box::new(nli),
            },
        )
    }

    fn job(case: &Case, segment: i64, text: &str) -> Job {
        Job {
            case_id: case.case,
            hearing_id: case.hearing,
            segment_id: segment,
            text_hash: text_hash(text),
        }
    }

    fn added(events: &[ReviewEvent]) -> Vec<crate::store::Suggestion> {
        events
            .iter()
            .filter_map(|event| match event {
                ReviewEvent::SuggestionAdded(suggestion) => Some(suggestion.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_accused_answer_that_conflicts_with_the_evidence_becomes_a_stored_conflict() {
        let case = seeded("conflict");
        let llm = llm(&explanation(
            true,
            "en Medellín",
            "estaba en Bogotá el 12 de marzo de 2024",
        ));
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        let suggestions = added(&events);
        assert_eq!(suggestions.len(), 1, "{events:?}");
        let suggestion = &suggestions[0];
        assert_eq!(suggestion.level, SuggestionLevel::Conflict);
        assert_eq!(suggestion.segment_id, case.answer);
        assert_eq!(suggestion.claim, CLAIM);
        assert_eq!(suggestion.claim_quote.as_deref(), Some("en Medellín"));
        assert_eq!(
            suggestion.evidence_quote,
            "estaba en Bogotá el 12 de marzo de 2024"
        );
        assert_eq!(suggestion.kind, Some(SuggestionKind::Location));
        assert_eq!(suggestion.evidence_name, "registro.txt");
        assert_eq!(suggestion.page, 1);
        assert_eq!(suggestion.nli_contradiction, 0.95f32 as f64);
        assert_eq!(
            case.store
                .lock()
                .unwrap()
                .suggestions(case.hearing)
                .unwrap(),
            suggestions
        );
        // The question gives the context of the answer, and only the flagged pair is explained.
        assert_eq!(
            llm.calls(),
            [
                format!("claims|{QUESTION}|{ANSWER}"),
                format!("explain|{CLAIM}|{HOTEL}"),
            ]
        );
    }

    #[test]
    fn a_flagged_pair_is_a_check_when_the_language_model_finds_no_contradiction() {
        let case = seeded("no-contradiction");
        let llm = llm(&explanation(false, "en Medellín", "estaba en Bogotá"));
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let suggestions = added(&analyzer.analyze(&job(&case, case.answer, ANSWER)));

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].level, SuggestionLevel::Check);
        assert_eq!(suggestions[0].claim_quote.as_deref(), Some("en Medellín"));
    }

    #[test]
    fn a_flagged_pair_is_a_check_with_a_note_when_the_explanation_is_not_valid() {
        let case = seeded("invalid");
        // The evidence quote is not in the chunk.
        let llm = llm(&explanation(true, "en Medellín", "estuvo en Bogotá"));
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let suggestions = added(&analyzer.analyze(&job(&case, case.answer, ANSWER)));

        assert_eq!(suggestions.len(), 1);
        let suggestion = &suggestions[0];
        assert_eq!(suggestion.level, SuggestionLevel::Check);
        assert_eq!(suggestion.claim_quote, None);
        assert_eq!(suggestion.kind, None);
        assert_eq!(suggestion.evidence_quote, HOTEL);
        assert_eq!(
            suggestion.explanation.as_deref(),
            Some(super::UNVERIFIED_NOTE)
        );
    }

    #[test]
    fn a_medium_contradiction_is_a_check_even_when_the_language_model_agrees() {
        let case = seeded("medium");
        let llm = llm(&explanation(true, "en Medellín", "estaba en Bogotá"));
        let mut analyzer = analyzer(&case, &llm, nli(0.7));

        let suggestions = added(&analyzer.analyze(&job(&case, case.answer, ANSWER)));

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].level, SuggestionLevel::Check);
    }

    fn analyzed(case: &Case, state: AnalysisState) -> ReviewEvent {
        ReviewEvent::Analyzed {
            hearing_id: case.hearing,
            segment_id: case.answer,
            state,
        }
    }

    fn states(case: &Case) -> Vec<(i64, AnalysisState)> {
        case.store
            .lock()
            .unwrap()
            .analysis_states(case.hearing)
            .unwrap()
    }

    #[test]
    fn a_claim_without_a_flagged_passage_gives_nothing_and_no_explanation() {
        let case = seeded("neutral");
        let llm = llm(&explanation(true, "en Medellín", "estaba en Bogotá"));
        let mut analyzer = analyzer(&case, &llm, nli(0.3));

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        assert_eq!(events, [analyzed(&case, AnalysisState::Complete)]);
        assert_eq!(llm.calls().len(), 1);
        assert!(
            case.store
                .lock()
                .unwrap()
                .suggestions(case.hearing)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_segment_of_an_other_speaker_is_ignored() {
        let case = seeded("other");
        let llm = llm("{}");
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let events = analyzer.analyze(&job(&case, case.question, QUESTION));

        assert!(events.is_empty());
        assert!(llm.calls().is_empty());
    }

    #[test]
    fn a_segment_whose_text_changed_after_the_job_is_skipped() {
        let case = seeded("stale");
        let llm = llm("{}");
        let mut analyzer = analyzer(&case, &llm, nli(0.95));
        let stale = job(&case, case.answer, ANSWER);
        case.store
            .lock()
            .unwrap()
            .set_segment_text(case.answer, "Ese día yo estaba en Cali.")
            .unwrap();

        let events = analyzer.analyze(&stale);

        assert!(events.is_empty());
        assert!(llm.calls().is_empty());
    }

    #[test]
    fn a_model_error_is_a_failed_analysis() {
        let case = seeded("error");
        let mut llm = llm("{}");
        llm.fail = true;
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        assert_eq!(events, [analyzed(&case, AnalysisState::Failed)]);
        assert_eq!(states(&case), [(case.answer, AnalysisState::Failed)]);
    }

    #[test]
    fn claims_that_are_not_valid_json_are_a_failed_analysis_not_a_statement_without_claims() {
        let case = seeded("malformed");
        let llm = FakeLlm::new("[\"El acusado estaba en", "{}");
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        assert_eq!(events, [analyzed(&case, AnalysisState::Failed)]);
        assert_eq!(llm.calls().len(), 1);
    }

    #[test]
    fn an_empty_claim_list_is_a_statement_without_claims() {
        let case = seeded("no-claim");
        let llm = FakeLlm::new("[]", "{}");
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        assert_eq!(events, [analyzed(&case, AnalysisState::NoClaim)]);
    }

    #[test]
    fn a_claim_with_no_passage_in_the_evidence_has_no_source() {
        let case = seeded("no-source");
        let llm = FakeLlm::new(
            &serde_json::json!(["Compró una bicicleta azul."]).to_string(),
            "{}",
        );
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        assert_eq!(events, [analyzed(&case, AnalysisState::NoSource)]);
        assert_eq!(states(&case), [(case.answer, AnalysisState::NoSource)]);
    }

    #[test]
    fn a_segment_without_analysis_is_skipped_and_a_queued_one_waits() {
        let case = seeded("submit");
        let job = job(&case, case.answer, ANSWER);

        let off = submit(&case.store, None, job.clone()).unwrap();
        assert_eq!(off, [(job.clone(), AnalysisState::Skipped)]);
        assert_eq!(states(&case), [(case.answer, AnalysisState::Skipped)]);

        let queue = JobQueue::new(4);
        let on = submit(&case.store, Some(&queue), job.clone()).unwrap();
        assert_eq!(on, [(job.clone(), AnalysisState::Waiting)]);
        assert_eq!(queue.try_next(), Some(job));
    }

    #[test]
    fn a_job_that_the_full_queue_drops_is_skipped() {
        let case = seeded("overflow");
        let queue = JobQueue::new(1);
        let first = job(&case, case.question, QUESTION);
        let second = job(&case, case.answer, ANSWER);
        submit(&case.store, Some(&queue), first.clone()).unwrap();

        let changed = submit(&case.store, Some(&queue), second.clone()).unwrap();

        assert_eq!(
            changed,
            [
                (second, AnalysisState::Waiting),
                (first, AnalysisState::Skipped)
            ]
        );
        assert_eq!(queue.dropped(), 1);
    }

    #[test]
    fn a_refine_during_the_last_model_call_stores_no_new_suggestion() {
        let case = seeded("refine-race");
        let mut llm = llm(&explanation(true, "en Medellín", "estaba en Bogotá"));
        let mut analyzer_1 = analyzer(&case, &llm, nli(0.95));
        let old = added(&analyzer_1.analyze(&job(&case, case.answer, ANSWER)));
        let store = case.store.clone();
        let answer = case.answer;
        // The refine pass changes the text while the language model explains the pair.
        llm.on_explain = Some(Arc::new(move || {
            store
                .lock()
                .unwrap()
                .set_segment_text(answer, "Ese día yo estaba en Cali.")
                .ok();
        }));
        let mut analyzer_2 = analyzer(&case, &llm, nli(0.95));

        let events = analyzer_2.analyze(&job(&case, case.answer, ANSWER));

        assert!(events.is_empty(), "{events:?}");
        let stored = case
            .store
            .lock()
            .unwrap()
            .suggestions(case.hearing)
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, old[0].id);
        assert_eq!(stored[0].status, SuggestionStatus::Stale);
        // The state of the old text does not apply to the new text.
        assert!(states(&case).is_empty());
    }

    #[test]
    fn a_failed_new_analysis_keeps_a_confirmed_suggestion() {
        let case = seeded("keep-confirmed");
        let mut llm = llm(&explanation(true, "en Medellín", "estaba en Bogotá"));
        let first =
            added(&analyzer(&case, &llm, nli(0.95)).analyze(&job(&case, case.answer, ANSWER)));
        let refined = "Ese día yo estaba en Medellín, señor juez.";
        {
            let store = case.store.lock().unwrap();
            store
                .set_suggestion_status(first[0].id, SuggestionStatus::Confirmed)
                .unwrap();
            store.set_segment_text(case.answer, refined).unwrap();
        }
        llm.fail = true;

        let events = analyzer(&case, &llm, nli(0.95)).analyze(&job(&case, case.answer, refined));

        assert_eq!(events, [analyzed(&case, AnalysisState::Failed)]);
        let stored = case
            .store
            .lock()
            .unwrap()
            .suggestions(case.hearing)
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].status, SuggestionStatus::Confirmed);
        assert_eq!(stored[0].segment_revision, 0);
    }

    #[test]
    fn the_model_gate_gives_one_permit_at_a_time() {
        let gate = ModelGate::default();

        let first = gate.try_acquire().expect("free gate");
        assert!(gate.try_acquire().is_none());
        drop(first);

        assert!(gate.try_acquire().is_some());
    }

    #[test]
    fn an_error_after_a_flagged_pair_stores_nothing_and_fails() {
        let case = seeded("partial-error");
        let mut llm = FakeLlm::new(
            &serde_json::json!([CLAIM, CLAIM]).to_string(),
            &explanation(true, "en Medellín", "estaba en Bogotá"),
        );
        llm.fail_explain_from = Some(1);
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        assert!(
            case.store
                .lock()
                .unwrap()
                .suggestions(case.hearing)
                .unwrap()
                .is_empty()
        );
        assert_eq!(events, [analyzed(&case, AnalysisState::Failed)]);
    }

    #[test]
    fn a_cancelled_analysis_calls_no_model_and_stores_nothing() {
        let case = seeded("cancelled");
        let llm = llm(&explanation(true, "en Medellín", "estaba en Bogotá"));
        let cancel = Arc::new(AtomicBool::new(true));
        let mut analyzer = analyzer(&case, &llm, nli(0.95)).with_cancel(cancel);

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        assert_eq!(events, [analyzed(&case, AnalysisState::Skipped)]);
        assert!(llm.calls().is_empty());
    }

    #[test]
    fn a_new_analysis_after_a_refine_adds_suggestions_and_the_old_ones_stay_stale() {
        let case = seeded("replace");
        let llm = llm(&explanation(true, "en Medellín", "estaba en Bogotá"));
        let mut analyzer = analyzer(&case, &llm, nli(0.95));
        let old = added(&analyzer.analyze(&job(&case, case.answer, ANSWER)));
        let refined = "Ese día yo estaba en Medellín, señor juez.";
        case.store
            .lock()
            .unwrap()
            .set_segment_text(case.answer, refined)
            .unwrap();

        let events = analyzer.analyze(&job(&case, case.answer, refined));

        let stored = case
            .store
            .lock()
            .unwrap()
            .suggestions(case.hearing)
            .unwrap();
        assert_eq!(stored.len(), 2);
        let new = stored.iter().find(|s| s.id != old[0].id).unwrap();
        assert_eq!(
            (new.status, new.segment_revision),
            (SuggestionStatus::Open, 1)
        );
        let stale = stored.iter().find(|s| s.id == old[0].id).unwrap();
        assert_eq!(stale.status, SuggestionStatus::Stale);
        assert_eq!(
            events,
            [
                ReviewEvent::SuggestionAdded(new.clone()),
                analyzed(&case, AnalysisState::Complete),
            ]
        );
    }

    #[test]
    fn a_new_job_for_a_segment_replaces_its_pending_job() {
        let queue = JobQueue::new(4);
        let job = |segment_id, text_hash| Job {
            case_id: 1,
            hearing_id: 1,
            segment_id,
            text_hash,
        };

        queue.push(job(1, 10));
        queue.push(job(2, 20));
        queue.push(job(1, 11));

        let next: Vec<(i64, u64)> = std::iter::from_fn(|| queue.try_next())
            .map(|job| (job.segment_id, job.text_hash))
            .collect();
        assert_eq!(next, [(2, 20), (1, 11)]);
        assert_eq!(queue.dropped(), 0);
    }

    #[test]
    fn a_claim_gets_at_most_two_suggestions() {
        let case = seeded("cap");
        {
            let store = case.store.lock().unwrap();
            let dir =
                std::env::temp_dir().join(format!("nicolaw-review-cap2-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            for name in ["a.txt", "b.txt"] {
                let path = dir.join(name);
                fs::write(&path, "x").unwrap();
                let id = store.import(case.case, &path, |_, _| {}).unwrap().id;
                store.save_analysis(id, &[page(1, HOTEL)]).unwrap();
            }
        }
        let llm = llm(&explanation(true, "en Medellín", "estaba en Bogotá"));
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let suggestions = added(&analyzer.analyze(&job(&case, case.answer, ANSWER)));

        assert_eq!(suggestions.len(), 2);
    }

    #[test]
    fn a_full_queue_drops_the_oldest_job_and_counts_it() {
        let queue = JobQueue::new(2);
        let job = |segment_id| Job {
            case_id: 1,
            hearing_id: 1,
            segment_id,
            text_hash: 0,
        };

        queue.push(job(1));
        queue.push(job(2));
        queue.push(job(3));

        assert_eq!(queue.pending(), 2);
        assert_eq!(queue.dropped(), 1);
        let next: Vec<i64> = std::iter::from_fn(|| queue.try_next())
            .map(|job| job.segment_id)
            .collect();
        assert_eq!(next, [2, 3]);
    }
}
