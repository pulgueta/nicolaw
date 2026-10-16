//! The real analysis models with candle: mDeBERTa-v3 NLI and Qwen3 4B (GGUF, Q4_K_M). Each
//! model has its own thread, which loads the model and serves the calls. The language model
//! and the decision step share the one Qwen3 thread.

use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use candle_core::quantized::gguf_file;
use candle_core::{D, DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::generation::LogitsProcessor;
use candle_transformers::models::debertav2::{Config, DebertaV2SeqClassificationModel};
use candle_transformers::models::quantized_qwen3::ModelWeights;
use tokenizers::{Tokenizer, TruncationParams};

use super::prompt::{claims_prompt, decision_prompt, explain_prompt, json_end};
use super::{
    Decider, Decision, DecisionScores, Engines, Llm, Nli, NliScores, ReviewBackend, ReviewError,
};

/// The NLI model reads at most this number of tokens for a pair.
const NLI_MAX_TOKENS: usize = 512;
/// The language model writes at most this number of tokens for one answer.
const LLM_MAX_TOKENS: usize = 384;
const TEMPERATURE: f64 = 0.1;
/// A fixed seed, so that the same input gives the same output.
const SEED: u64 = 299_792_458;

/// Loads the analysis models on the GPU (Metal) when it is available, else on the CPU.
pub struct NativeReview;

impl ReviewBackend for NativeReview {
    fn load(&self, nli: &[PathBuf], llm: &[PathBuf]) -> Result<Engines, ReviewError> {
        let nli = nli.to_vec();
        let llm = llm.to_vec();
        // A copy of the tokenizer, with no truncation, measures the pairs.
        let mut tokenizer = nli
            .get(1)
            .and_then(|path| Tokenizer::from_file(path).ok())
            .ok_or(ReviewError::ModelLoad)?;
        tokenizer.with_truncation(None).map_err(load_error)?;
        let qwen = Arc::new(ModelThread::spawn("nicolaw-llm", move || {
            Qwen::load(&llm, device())
        })?);
        Ok(Engines {
            nli: Box::new(NativeNli {
                thread: ModelThread::spawn("nicolaw-nli", move || Deberta::load(&nli, device()))?,
                tokenizer,
            }),
            llm: Box::new(NativeLlm(qwen.clone())),
            decider: Box::new(NativeDecider(qwen)),
        })
    }
}

fn device() -> Device {
    Device::new_metal(0).unwrap_or(Device::Cpu)
}

type Call<M> = Box<dyn FnOnce(&mut M) + Send>;

/// One model on its own thread. The thread loads the model, so the model does not need to be
/// `Send`. Drop stops the thread and waits for it, so that the model is free before the
/// process exits. A shared thread (in an `Arc`) stops when its last user drops it.
struct ModelThread<M> {
    /// `None` only during drop.
    calls: Option<mpsc::Sender<Call<M>>>,
    thread: Option<JoinHandle<()>>,
}

impl<M: 'static> ModelThread<M> {
    /// Starts the thread and waits until `load` ends.
    fn spawn(
        name: &str,
        load: impl FnOnce() -> Result<M, ReviewError> + Send + 'static,
    ) -> Result<Self, ReviewError> {
        let (calls, receiver) = mpsc::channel::<Call<M>>();
        let (loaded, load_result) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut model = match load() {
                    Ok(model) => model,
                    Err(error) => {
                        loaded.send(Err(error)).ok();
                        return;
                    }
                };
                loaded.send(Ok(())).ok();
                while let Ok(call) = receiver.recv() {
                    call(&mut model);
                }
            })
            .map_err(|_| ReviewError::ModelLoad)?;
        let this = Self {
            calls: Some(calls),
            thread: Some(thread),
        };
        // A panic during the load closes the channel.
        load_result.recv().map_err(|_| ReviewError::ModelLoad)??;
        Ok(this)
    }

    /// Runs `f` on the model thread and waits for the result.
    fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut M) -> R + Send + 'static,
    ) -> Result<R, ReviewError> {
        let (sender, receiver) = mpsc::channel();
        self.calls
            .as_ref()
            .ok_or(ReviewError::Inference)?
            .send(Box::new(move |model| {
                sender.send(f(model)).ok();
            }))
            .map_err(|_| ReviewError::Inference)?;
        receiver.recv().map_err(|_| ReviewError::Inference)
    }
}

impl<M> Drop for ModelThread<M> {
    fn drop(&mut self) {
        self.calls.take();
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

fn load_error(_: impl std::fmt::Debug) -> ReviewError {
    ReviewError::ModelLoad
}

fn inference_error(_: impl std::fmt::Debug) -> ReviewError {
    ReviewError::Inference
}

struct NativeNli {
    thread: ModelThread<Deberta>,
    tokenizer: Tokenizer,
}

impl Nli for NativeNli {
    fn score(&mut self, premise: &str, hypothesis: &str) -> Result<NliScores, ReviewError> {
        let (premise, hypothesis) = (premise.to_owned(), hypothesis.to_owned());
        self.thread
            .call(move |model| model.score(&premise, &hypothesis))?
    }

    fn fits(&self, premise: &str, hypothesis: &str) -> bool {
        self.tokenizer
            .encode((premise, hypothesis), true)
            .is_ok_and(|encoding| encoding.len() <= NLI_MAX_TOKENS)
    }
}

/// The probabilities of the options from their logits, in [`Decision::ALL`] order: a softmax
/// over these four values only.
fn option_scores(logits: [f32; 4]) -> DecisionScores {
    let max = logits.into_iter().fold(f32::NEG_INFINITY, f32::max);
    let exp = logits.map(|logit| (logit - max).exp());
    let sum: f32 = exp.iter().sum();
    let [contradicts, consistent, unrelated, insufficient] = exp.map(|value| value / sum);
    DecisionScores {
        contradicts,
        consistent,
        unrelated,
        insufficient,
    }
}

struct NativeLlm(Arc<ModelThread<Qwen>>);

impl Llm for NativeLlm {
    fn claims(&mut self, question: Option<&str>, statement: &str) -> Result<String, ReviewError> {
        let prompt = claims_prompt(question, statement);
        self.0.call(move |model| model.answer(&prompt, "["))?
    }

    fn explain(&mut self, claim: &str, passage: &str) -> Result<String, ReviewError> {
        let prompt = explain_prompt(claim, passage);
        self.0.call(move |model| model.answer(&prompt, "{"))?
    }
}

/// The decision step on the shared Qwen3 thread: option-logit scoring, no text generation.
struct NativeDecider(Arc<ModelThread<Qwen>>);

impl Decider for NativeDecider {
    fn decide(&mut self, claim: &str, passage: &str) -> Result<DecisionScores, ReviewError> {
        let prompt = decision_prompt(claim, passage);
        self.0.call(move |model| model.decide(&prompt))?
    }
}

/// mDeBERTa-v3 sequence classification for NLI, in float32.
struct Deberta {
    model: DebertaV2SeqClassificationModel,
    tokenizer: Tokenizer,
    device: Device,
    /// The output index of entailment, neutral, and contradiction.
    labels: [usize; 3],
}

impl Deberta {
    /// `files` are the config, the tokenizer, and the weights of [`crate::models::NLI`].
    fn load(files: &[PathBuf], device: Device) -> Result<Self, ReviewError> {
        let [config, tokenizer, weights] = files else {
            return Err(ReviewError::ModelLoad);
        };
        let config: Config =
            serde_json::from_slice(&fs::read(config).map_err(load_error)?).map_err(load_error)?;
        let id2label = config.id2label.clone().ok_or(ReviewError::ModelLoad)?;
        let label = |name: &str| {
            id2label
                .iter()
                .find(|(_, label)| label.as_str() == name)
                .map(|(&index, _)| index as usize)
                .ok_or(ReviewError::ModelLoad)
        };
        let labels = [
            label("entailment")?,
            label("neutral")?,
            label("contradiction")?,
        ];
        let mut tokenizer = Tokenizer::from_file(tokenizer).map_err(load_error)?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: NLI_MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(load_error)?;
        tokenizer.with_padding(None);
        // The weights are float16 on disk. DeBERTa-v3 is not stable in float16, so it runs in
        // float32.
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[weights], DType::F32, &device) }
            .map_err(load_error)?;
        let model = DebertaV2SeqClassificationModel::load(vb.pp("deberta"), &config, None)
            .map_err(load_error)?;
        Ok(Self {
            model,
            tokenizer,
            device,
            labels,
        })
    }

    fn score(&self, premise: &str, hypothesis: &str) -> Result<NliScores, ReviewError> {
        let encoding = self
            .tokenizer
            .encode((premise, hypothesis), true)
            .map_err(inference_error)?;
        if !encoding.get_overflowing().is_empty() {
            eprintln!("Nicolaw NLI: truncated: the pair is longer than {NLI_MAX_TOKENS} tokens");
        }
        let ids = Tensor::new(encoding.get_ids(), &self.device)
            .and_then(|ids| ids.unsqueeze(0))
            .map_err(inference_error)?;
        let probabilities = self
            .model
            .forward(&ids, None, None)
            .and_then(|logits| candle_nn::ops::softmax(&logits, D::Minus1))
            .and_then(|probabilities| probabilities.squeeze(0))
            .and_then(|probabilities| probabilities.to_vec1::<f32>())
            .map_err(inference_error)?;
        let [entailment, neutral, contradiction] = self.labels.map(|index| probabilities[index]);
        Ok(NliScores {
            entailment,
            neutral,
            contradiction,
        })
    }
}

/// Qwen3 4B from its GGUF file.
struct Qwen {
    weights: ModelWeights,
    tokenizer: Tokenizer,
    device: Device,
    /// The tokens that end an answer.
    stop: Vec<u32>,
    /// The first token of each decision label with a leading space, in [`Decision::ALL`]
    /// order.
    labels: [u32; 4],
}

impl Qwen {
    /// `files` are the GGUF file and the tokenizer of [`crate::models::LLM`].
    fn load(files: &[PathBuf], device: Device) -> Result<Self, ReviewError> {
        let [gguf, tokenizer] = files else {
            return Err(ReviewError::ModelLoad);
        };
        let mut file = File::open(gguf).map_err(load_error)?;
        let content = gguf_file::Content::read(&mut file).map_err(load_error)?;
        let weights = ModelWeights::from_gguf(content, &mut file, &device).map_err(load_error)?;
        let tokenizer = Tokenizer::from_file(tokenizer).map_err(load_error)?;
        let stop = ["<|im_end|>", "<|endoftext|>"]
            .iter()
            .filter_map(|token| tokenizer.token_to_id(token))
            .collect();
        let mut labels = [0; 4];
        for (slot, decision) in labels.iter_mut().zip(Decision::ALL) {
            // The prompt ends with "Decision:", so the label comes after a space.
            let encoding = tokenizer
                .encode(format!(" {}", decision.as_str()), false)
                .map_err(load_error)?;
            *slot = *encoding.get_ids().first().ok_or(ReviewError::ModelLoad)?;
        }
        // Two labels with the same first token cannot be told apart.
        if (1..4).any(|i| labels[..i].contains(&labels[i])) {
            return Err(ReviewError::ModelLoad);
        }
        Ok(Self {
            weights,
            tokenizer,
            device,
            stop,
            labels,
        })
    }

    /// Scores the decision options after `prompt` ([`decision_prompt`]): one forward pass, the
    /// logit of the first token of each label, and a softmax over these four logits only.
    fn decide(&mut self, prompt: &str) -> Result<DecisionScores, ReviewError> {
        self.weights.clear_kv_cache();
        let prompt = self
            .tokenizer
            .encode(prompt, false)
            .map_err(inference_error)?
            .get_ids()
            .to_vec();
        let logits = Tensor::new(prompt.as_slice(), &self.device)
            .and_then(|tensor| tensor.unsqueeze(0))
            .and_then(|tensor| self.weights.forward(&tensor, 0))
            .and_then(|logits| logits.squeeze(0))
            .and_then(|logits| logits.to_dtype(DType::F32))
            .and_then(|logits| logits.to_vec1::<f32>())
            .map_err(inference_error)?;
        let mut options = [0.0; 4];
        for (option, token) in options.iter_mut().zip(self.labels) {
            *option = *logits.get(token as usize).ok_or(ReviewError::Inference)?;
        }
        Ok(option_scores(options))
    }

    /// Generates the answer to `prompt`, which ends with `start`. Returns `start` and the
    /// generated text, until the JSON value is complete, a stop token, or the token limit.
    fn answer(&mut self, prompt: &str, start: &str) -> Result<String, ReviewError> {
        self.weights.clear_kv_cache();
        let prompt = self
            .tokenizer
            .encode(prompt, false)
            .map_err(inference_error)?
            .get_ids()
            .to_vec();
        let mut sampler = LogitsProcessor::new(SEED, Some(TEMPERATURE), None);
        let mut generated: Vec<u32> = Vec::new();
        let mut input = prompt.clone();
        let mut position = 0;
        let mut text = start.to_owned();
        for _ in 0..LLM_MAX_TOKENS {
            let tensor = Tensor::new(input.as_slice(), &self.device)
                .and_then(|tensor| tensor.unsqueeze(0))
                .map_err(inference_error)?;
            let logits = self
                .weights
                .forward(&tensor, position)
                .and_then(|logits| logits.squeeze(0))
                .map_err(inference_error)?;
            position += input.len();
            let token = sampler.sample(&logits).map_err(inference_error)?;
            if self.stop.contains(&token) {
                break;
            }
            generated.push(token);
            text = format!(
                "{start}{}",
                self.tokenizer
                    .decode(&generated, true)
                    .map_err(inference_error)?
            );
            if let Some(end) = json_end(&text) {
                text.truncate(end);
                break;
            }
            input = vec![token];
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::{Deberta, NativeReview, Qwen, device, option_scores};
    use crate::models::{self, LLM, ModelDirs, NLI};
    use crate::review::ReviewBackend;
    use crate::review::parse::{parse_claims, parse_explanation};

    #[test]
    fn option_scores_are_a_softmax_over_the_four_logits() {
        let scores = option_scores([2.0, 0.0, 0.0, 0.0]);

        let sum = scores.contradicts + scores.consistent + scores.unrelated + scores.insufficient;
        assert!((sum - 1.0).abs() < 1e-6, "{scores:?}");
        // e^2 / (e^2 + 3)
        assert!((scores.contradicts - 0.711_235).abs() < 1e-5, "{scores:?}");
        assert_eq!(scores.consistent, scores.insufficient);
        // Large logits do not overflow.
        assert!((option_scores([1000.0, 1000.0, 0.0, 0.0]).consistent - 0.5).abs() < 1e-6);
    }

    fn files(model: &models::AnalysisModel) -> Vec<std::path::PathBuf> {
        models::find_files(&ModelDirs::system(), model)
            .expect("the model is not installed: run real_analysis_models_download first")
    }

    /// Downloads the NLI model (575 MB) and the language model (2.5 GB) into the shared Hugging
    /// Face cache, as the Descargar buttons do. Needs the internet:
    ///
    /// ```sh
    /// cargo test -- --ignored --nocapture real_analysis_models_download
    /// ```
    #[test]
    #[ignore = "downloads about 3 GB"]
    fn real_analysis_models_download() {
        for model in models::ANALYSIS_MODELS {
            let mut last = 0;
            let paths = models::download_files(model, &ModelDirs::system(), |part| {
                let percent = (part * 100.0) as u32;
                if percent >= last + 10 {
                    last = percent;
                    println!("{}: {percent}%", model.name);
                }
            })
            .unwrap();
            println!("{}: {paths:?}", model.name);
            assert_eq!(models::find_files(&ModelDirs::system(), model), Some(paths));
        }
    }

    /// Scores a Spanish contradiction in both directions with the installed NLI model:
    ///
    /// ```sh
    /// cargo test -- --ignored --nocapture real_nli
    /// ```
    #[test]
    #[ignore = "needs the installed NLI model"]
    fn real_nli_scores_a_spanish_contradiction() {
        let evidence = "El 12 de marzo el acusado viajó a Medellín y no estuvo en Bogotá.";
        let claim = "El acusado estaba en Bogotá el 12 de marzo.";
        let device = device();
        println!("device: {device:?}");
        let started = std::time::Instant::now();
        let model = Deberta::load(&files(&NLI), device).unwrap();
        println!("load: {:?}", started.elapsed());

        let started = std::time::Instant::now();
        let forward = model.score(evidence, claim).unwrap();
        let backward = model.score(claim, evidence).unwrap();
        println!("two pairs: {:?}", started.elapsed());
        let started = std::time::Instant::now();
        let neutral = model.score("El testigo vio un carro rojo.", claim).unwrap();
        println!("one warm pair: {:?}", started.elapsed());

        println!("evidence -> claim: {forward:?}");
        println!("claim -> evidence: {backward:?}");
        println!("unrelated: {neutral:?}");
        assert!(forward.max(backward).contradiction > 0.5);
        assert!(neutral.contradiction < forward.max(backward).contradiction);
    }

    /// Extracts the claims of a Spanish statement, and explains one conflict, with the
    /// installed language model:
    ///
    /// ```sh
    /// cargo test -- --ignored --nocapture real_llm
    /// ```
    #[test]
    #[ignore = "needs the installed language model"]
    fn real_llm_extracts_claims_and_explains_a_conflict() {
        let mut model = Qwen::load(&files(&LLM), device()).unwrap();
        let question = "¿Dónde estaba usted el 12 de marzo?";
        let statement = "Ese día yo estaba en Bogotá, en la casa de mi hermana, y le pagué \
                         dos millones de pesos.";

        let started = std::time::Instant::now();
        let claims = model
            .answer(
                &crate::review::prompt::claims_prompt(Some(question), statement),
                "[",
            )
            .unwrap();
        println!("claims ({:?}): {claims}", started.elapsed());
        let claim = parse_claims(&claims)
            .into_iter()
            .flatten()
            .next()
            .expect("no claim");

        let chunk = "Según el registro del hotel, el 12 de marzo el acusado viajó a Medellín \
                     y no estuvo en Bogotá.";
        let started = std::time::Instant::now();
        let explanation = model
            .answer(&crate::review::prompt::explain_prompt(&claim, chunk), "{")
            .unwrap();
        println!("explanation ({:?}): {explanation}", started.elapsed());
        println!(
            "valid: {:?}",
            parse_explanation(&explanation, &claim, chunk)
        );
    }

    /// Scores the decision options of a clear Spanish contradiction, and of a consistent and an
    /// unrelated pair for comparison, with the installed language model:
    ///
    /// ```sh
    /// cargo test real_decider -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs the installed language model"]
    fn real_decider_marks_a_spanish_contradiction() {
        use crate::review::Decision;
        use crate::review::prompt::decision_prompt;

        let mut model = Qwen::load(&files(&LLM), device()).unwrap();
        let claim = "El acusado estaba en Bogotá el 12 de marzo de 2024.";
        let pairs = [
            (
                "contradiction",
                "Según el registro del hotel, el 12 de marzo de 2024 el acusado estaba en \
                 Medellín y no estuvo en Bogotá.",
            ),
            (
                "consistent",
                "El 12 de marzo de 2024 el acusado almorzó en un restaurante de Bogotá.",
            ),
            (
                "unrelated",
                "El testigo vio un carro rojo frente a la casa.",
            ),
        ];
        let mut scores = Vec::new();
        for (name, passage) in pairs {
            let started = std::time::Instant::now();
            let decision = model.decide(&decision_prompt(claim, passage)).unwrap();
            println!(
                "{name} ({:?}): {decision:?} best={:?}",
                started.elapsed(),
                decision.best()
            );
            scores.push(decision);
        }

        assert_eq!(scores[0].best().0, Decision::Contradicts);
    }

    /// Runs the full analysis with the real models: a hearing answer of the accused against a
    /// hotel register in the case.
    ///
    /// ```sh
    /// cargo test -- --ignored --nocapture real_pipeline
    /// ```
    #[test]
    #[ignore = "needs the installed analysis models"]
    fn real_pipeline_finds_a_conflict_with_its_source() {
        use std::sync::{Arc, Mutex};

        use crate::extract::{Page, PageSource};
        use crate::review::ReviewEvent;
        use crate::review::analyzer::{Analyzer, Job, text_hash};
        use crate::store::{Speaker, Store};

        let store = Store::open_in_memory().unwrap();
        let case = store.create_case("Caso").unwrap().id;
        let dir = std::env::temp_dir().join(format!("nicolaw-real-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registro-hotel.txt");
        std::fs::write(&path, "x").unwrap();
        let evidence = store.import(case, &path, |_, _| {}).unwrap().id;
        let pages = [
            "Registro de huéspedes del Hotel Andino, Medellín.",
            "El señor Carlos Pérez se registró el 12 de marzo de 2024 a las 8:00 p. m. en la \
             habitación 204 y salió el 14 de marzo. Pagó 450.000 pesos en efectivo.",
        ];
        let pages: Vec<Page> = pages
            .iter()
            .enumerate()
            .map(|(i, text)| Page {
                number: i as u32 + 1,
                text: (*text).into(),
                source: PageSource::Text,
            })
            .collect();
        store.save_analysis(evidence, &pages).unwrap();
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let question = store.insert_segment(hearing, 0, Speaker::Other).unwrap();
        store
            .finish_segment(
                question,
                "¿Dónde estuvo usted entre el 12 y el 14 de marzo?",
                3_000,
            )
            .unwrap();
        let answer_text = "Esos días yo estuve en Bogotá, en mi casa. Nunca fui a Medellín.";
        let answer = store
            .insert_segment(hearing, 4_000, Speaker::Accused)
            .unwrap();
        store.finish_segment(answer, answer_text, 8_000).unwrap();
        let store = Arc::new(Mutex::new(store));
        let engines = NativeReview.load(&files(&NLI), &files(&LLM)).unwrap();
        let mut analyzer = Analyzer::new(store.clone(), engines);

        let started = std::time::Instant::now();
        let events = analyzer.analyze(&Job {
            case_id: case,
            hearing_id: hearing,
            segment_id: answer,
            text_hash: text_hash(answer_text),
        });

        println!("analysis: {:?}", started.elapsed());
        for event in &events {
            match event {
                ReviewEvent::SuggestionAdded(s) => println!(
                    "{:?} {:?} p(contradiction)={:.3} decision={:?} p={:?}\n  claim: {}\n  \
                     claim quote: {:?}\n  evidence: {} (página {}): {}\n  explanation: {:?}",
                    s.level,
                    s.kind,
                    s.nli_contradiction,
                    s.decision,
                    s.decision_p,
                    s.claim,
                    s.claim_quote,
                    s.evidence_name,
                    s.page,
                    s.evidence_quote,
                    s.explanation
                ),
                other => println!("{other:?}"),
            }
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ReviewEvent::SuggestionAdded(_)))
        );
    }

    /// Loads both models through the backend, as the app does, and uses the NLI model and the
    /// decision step.
    ///
    /// ```sh
    /// cargo test -- --ignored --nocapture real_backend
    /// ```
    #[test]
    #[ignore = "needs the installed analysis models"]
    fn real_backend_loads_both_models() {
        let mut engines = NativeReview.load(&files(&NLI), &files(&LLM)).unwrap();

        let scores = engines
            .nli
            .score(
                "El acusado estaba en Medellín.",
                "El acusado estaba en Bogotá.",
            )
            .unwrap();

        println!("scores: {scores:?}");
        // The language model and the decision step share one Qwen3 thread.
        let decision = engines
            .decider
            .decide(
                "El acusado estaba en Bogotá.",
                "El acusado estaba en Medellín.",
            )
            .unwrap();
        println!("decision: {decision:?}");
        drop(engines);
    }
}
