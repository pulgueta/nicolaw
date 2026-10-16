//! The analysis of one segment, and the bounded queue of segments that wait for it.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_channel::TrySendError;

use super::parse::{Explanation, parse_claims, parse_explanation, premises};
use super::query::query_terms;
use super::{Engines, Llm, Nli, ReviewError, ReviewEvent, decide};
use crate::store::{NewSuggestion, Segment, Speaker, Store, SuggestionLevel};

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
}

impl Analyzer {
    pub fn new(store: Arc<Mutex<Store>>, engines: Engines) -> Self {
        Self {
            store,
            llm: engines.llm,
            nli: engines.nli,
            cancel: Arc::default(),
        }
    }

    /// The analysis stops when `cancel` becomes true.
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Analyzes one segment: claims, retrieval, NLI, explanation, and storage. Returns the new
    /// suggestions, then an error if the analysis failed. A segment of an other speaker, or a
    /// segment whose text changed, gives nothing. The new suggestions of a segment replace its
    /// old suggestions.
    pub fn analyze(&mut self, job: &Job) -> Vec<ReviewEvent> {
        let mut events = Vec::new();
        if let Err(error) = self.try_analyze(job, &mut events) {
            events.push(ReviewEvent::Error(error.message().into()));
        }
        events
    }

    fn try_analyze(&mut self, job: &Job, events: &mut Vec<ReviewEvent>) -> Result<(), ReviewError> {
        if self.cancelled() {
            return Ok(());
        }
        let Some(segment) = self.current(job)? else {
            return Ok(());
        };
        let removed = self
            .store
            .lock()
            .unwrap()
            .delete_segment_suggestions(job.segment_id)?;
        if removed > 0 {
            events.push(ReviewEvent::SuggestionsRemoved {
                hearing_id: job.hearing_id,
            });
        }
        let question = self
            .store
            .lock()
            .unwrap()
            .question_before(job.hearing_id, segment.t_start_ms)?;
        let claims = parse_claims(&self.llm.claims(question.as_deref(), &segment.text)?);
        for claim in claims {
            for flagged in self.flag(job.case_id, &claim)? {
                if self.cancelled() {
                    return Ok(());
                }
                let output = self.llm.explain(&claim, &flagged.premise)?;
                let explanation = parse_explanation(&output, &claim, &flagged.chunk_text);
                // The refine pass can change the text during the analysis.
                if self.cancelled() || self.current(job)?.is_none() {
                    return Ok(());
                }
                let new = suggestion(job, &claim, flagged, explanation);
                let stored = self.store.lock().unwrap().insert_suggestion(&new)?;
                events.push(ReviewEvent::SuggestionAdded(stored));
            }
        }
        Ok(())
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

    /// The passages of the case that the NLI model flags for the claim, strongest first, one
    /// for each chunk, at most [`MAX_SUGGESTIONS_PER_CLAIM`].
    fn flag(&mut self, case_id: i64, claim: &str) -> Result<Vec<Flagged>, ReviewError> {
        let hits =
            self.store
                .lock()
                .unwrap()
                .search_any(case_id, &query_terms(claim), SEARCH_LIMIT)?;
        let mut flagged: Vec<Flagged> = Vec::new();
        for hit in hits {
            let chunk = self.store.lock().unwrap().chunk(hit.chunk_id)?;
            let mut best: Option<Flagged> = None;
            for premise in premises(&chunk.text) {
                if self.cancelled() {
                    return Ok(Vec::new());
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
        Ok(flagged)
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

    pub fn push(&self, mut job: Job) {
        // A new job for a segment replaces its pending job: the text of the segment changed.
        let pending: Vec<Job> = std::iter::from_fn(|| self.receiver.try_recv().ok()).collect();
        for old in pending {
            if old.segment_id != job.segment_id {
                self.sender.try_send(old).ok();
            }
        }
        loop {
            match self.sender.try_send(job) {
                Ok(()) | Err(TrySendError::Closed(_)) => return,
                Err(TrySendError::Full(back)) => {
                    if self.receiver.try_recv().is_ok() {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    job = back;
                }
            }
        }
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

    use super::{Analyzer, Job, JobQueue, text_hash};
    use crate::extract::{Page, PageSource};
    use crate::review::fake::{FakeLlm, FakeNli};
    use crate::review::{Engines, ReviewError, ReviewEvent};
    use crate::store::{Speaker, Store, SuggestionKind, SuggestionLevel};

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

    #[test]
    fn a_claim_without_a_flagged_passage_gives_nothing_and_no_explanation() {
        let case = seeded("neutral");
        let llm = llm(&explanation(true, "en Medellín", "estaba en Bogotá"));
        let mut analyzer = analyzer(&case, &llm, nli(0.3));

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        assert!(events.is_empty(), "{events:?}");
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
    fn a_model_error_is_reported() {
        let case = seeded("error");
        let mut llm = llm("{}");
        llm.fail = true;
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        assert_eq!(
            events,
            [ReviewEvent::Error(ReviewError::Inference.message().into())]
        );
    }

    #[test]
    fn an_error_after_a_stored_suggestion_reports_the_suggestion_and_the_error() {
        let case = seeded("partial-error");
        let mut llm = FakeLlm::new(
            &serde_json::json!([CLAIM, CLAIM]).to_string(),
            &explanation(true, "en Medellín", "estaba en Bogotá"),
        );
        llm.fail_explain_from = Some(1);
        let mut analyzer = analyzer(&case, &llm, nli(0.95));

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        let stored = case
            .store
            .lock()
            .unwrap()
            .suggestions(case.hearing)
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(
            events,
            [
                ReviewEvent::SuggestionAdded(stored[0].clone()),
                ReviewEvent::Error(ReviewError::Inference.message().into()),
            ]
        );
    }

    #[test]
    fn a_cancelled_analysis_calls_no_model_and_stores_nothing() {
        let case = seeded("cancelled");
        let llm = llm(&explanation(true, "en Medellín", "estaba en Bogotá"));
        let cancel = Arc::new(AtomicBool::new(true));
        let mut analyzer = analyzer(&case, &llm, nli(0.95)).with_cancel(cancel);

        let events = analyzer.analyze(&job(&case, case.answer, ANSWER));

        assert!(events.is_empty(), "{events:?}");
        assert!(llm.calls().is_empty());
    }

    #[test]
    fn a_new_analysis_of_a_segment_replaces_its_old_suggestions() {
        let case = seeded("replace");
        let llm = llm(&explanation(true, "en Medellín", "estaba en Bogotá"));
        let mut analyzer = analyzer(&case, &llm, nli(0.95));
        analyzer.analyze(&job(&case, case.answer, ANSWER));
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
        assert_eq!(stored.len(), 1);
        assert_eq!(
            events,
            [
                ReviewEvent::SuggestionsRemoved {
                    hearing_id: case.hearing
                },
                ReviewEvent::SuggestionAdded(stored[0].clone()),
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
