//! The hearings screen: the hearings of a case, live recording and transcription, the
//! transcript with the speaker of each segment, and the suggestions of the analysis. The
//! [`Recording`] controller owns the live recording. The app owns it, so that it outlives the
//! window and the quit can complete it.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Placement, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::audio::pipeline::{
    Event, Health, Input as AudioInput, Processor, STT_QUEUE, Storage, WavSource, Work,
    refine_hearing,
};
use crate::audio::{AudioError, Backend, Capture, Engines, Microphone, ModelPaths, SAMPLE_RATE};
use crate::models::{
    self, ANALYSIS_MODELS, AnalysisModel, CATALOG, COHERE, CatalogModel, ModelDirs, ModelStatus,
    NEMOTRON, SILERO_VAD,
};
use crate::review::analyzer::{self, Analyzer, Job, JobQueue, text_hash};
use crate::review::{ReviewBackend, ReviewEvent};
use crate::store::{
    Hearing, Segment, Speaker, Store, Suggestion, SuggestionCounts, SuggestionKind,
    SuggestionLevel, SuggestionStatus,
};

actions!(
    nicolaw,
    [
        /// Starts the recording of the selected hearing.
        StartRecording,
        /// Pauses or continues the live recording.
        TogglePause,
        /// Changes the speaker of the next speech.
        SwitchSpeaker
    ]
);

const LIST_ERROR: &str = "Nicolaw no puede leer las audiencias.";
const CREATE_ERROR: &str = "Nicolaw no puede crear la audiencia. Intente de nuevo.";
const DELETE_ERROR: &str = "Nicolaw no puede eliminar la audiencia. Intente de nuevo.";
const SPEAKER_ERROR: &str = "Nicolaw no puede cambiar el hablante. Intente de nuevo.";
const SAVE_ERROR: &str = "Nicolaw no pudo completar el guardado. Mantenga la aplicación abierta.";
const REFINE_ERROR: &str =
    "Nicolaw no pudo refinar la transcripción. El texto en vivo se mantiene.";
const DOWNLOAD_ERROR: &str =
    "La descarga falló. Revise la conexión a internet y seleccione Descargar.";
const DOWNLOAD_BLOCKED: &str = "Finalice la audiencia para descargar modelos.";
const TITLE_ERROR: &str = "Escriba el título de la audiencia.";
const SAVING: &str = "Guardando el audio y la transcripción…";
const INTERRUPTED: &str = "La grabación se interrumpió. Revise el audio guardado.";

/// At quit, Nicolaw waits this long for the audio file, the end of the hearing, and the last
/// segments.
const QUIT_TIMEOUT: Duration = Duration::from_secs(10);
/// After the quit timeout, the transcription stops at its next step. Nicolaw waits this long
/// for it, so that the models are free before the process exits.
const ABORT_TIMEOUT: Duration = Duration::from_secs(3);
/// The capture health is checked two times per second.
const TICK: Duration = Duration::from_millis(500);
/// A transcription that is late by this many seconds shows a notice.
const DELAY_NOTICE_S: u64 = 5;
/// Below this window width, the hearing list is a drawer and the live strip has two rows.
const NARROW_WIDTH: f32 = 1000.;
/// The setting with the microphone that the user selected. An empty value is the system
/// microphone.
const MICROPHONE_SETTING: &str = "microphone";
/// The value of "Micrófono del sistema" in the microphone select.
const SYSTEM_MICROPHONE: &str = "";
const SUGGESTION_ERROR: &str = "Nicolaw no puede guardar la decisión. Intente de nuevo.";
/// The analysis queue keeps at most this number of segments. When it is full, a new segment
/// replaces the oldest one, so that the analysis never slows the transcription.
const REVIEW_QUEUE: usize = 32;

/// What the hearings screen needs from the app.
#[derive(Clone)]
pub struct HearingsEnv {
    pub backend: Arc<dyn Backend>,
    /// Loads the analysis models.
    pub review: Arc<dyn ReviewBackend>,
    pub model_dirs: ModelDirs,
    /// `<data_dir>/nicolaw`. The audio of each hearing goes to `hearings/<id>.wav` in it.
    pub data_dir: PathBuf,
}

/// Binds the keys of the recording actions. Bare letters are not used, so that typing in a
/// field never controls the recording.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-shift-r", StartRecording, None),
        KeyBinding::new("cmd-shift-p", TogglePause, None),
        KeyBinding::new("cmd-shift-a", SwitchSpeaker, None),
    ]);
}

/// Connects the recording actions to the recording and to the hearings screen. They work on
/// all screens.
pub fn register_actions(
    recording: &Entity<Recording>,
    hearings: WeakEntity<HearingsView>,
    cx: &mut App,
) {
    cx.on_action(move |_: &StartRecording, cx| {
        hearings.update(cx, |hearings, cx| hearings.start(cx)).ok();
    });
    let pause = recording.downgrade();
    cx.on_action(move |_: &TogglePause, cx| {
        pause
            .update(cx, |recording, cx| recording.toggle_pause(cx))
            .ok();
    });
    let speaker = recording.downgrade();
    cx.on_action(move |_: &SwitchSpeaker, cx| {
        speaker
            .update(cx, |recording, cx| recording.switch_speaker(cx))
            .ok();
    });
}

/// What the recording reports to the screens.
#[derive(Clone, Debug, PartialEq)]
pub enum RecordingEvent {
    /// An event of the audio workers or of the refine pass.
    Audio {
        hearing_id: i64,
        case_id: i64,
        event: Event,
    },
    /// The recording started.
    Started { hearing_id: i64 },
    /// The recording could not start.
    Failed(AudioError),
    /// After Finalizar: the audio file, the end of the hearing, and the segments are saved, or
    /// the save failed.
    Saved {
        hearing_id: i64,
        result: Result<(), AudioError>,
    },
    /// The refine pass ended. The result is the number of refined segments.
    Refined {
        hearing_id: i64,
        result: Result<usize, AudioError>,
    },
}

/// The hearing that records now.
pub struct Live {
    pub hearing_id: i64,
    pub case_id: i64,
    pub case_name: String,
    pub title: String,
    /// The name of the open microphone.
    pub device: String,
    /// The live transcription model.
    pub model: PathBuf,
    pub paused: bool,
    /// True after the microphone or the audio file stopped working.
    pub interrupted: bool,
    pub health: Health,
    /// The mic level, from 0 to 1.
    pub level: f32,
    /// The samples in the audio file, and the samples that the transcription processed.
    written: Arc<AtomicU64>,
    processed: Arc<AtomicU64>,
    capture: Box<dyn Capture>,
    input: async_channel::Sender<AudioInput>,
    storage: Task<()>,
    transcription: Task<()>,
    /// Sends the health checks. It holds an input sender, so it must stop before the input
    /// can close.
    ticker: Task<()>,
}

impl Live {
    /// The captured audio, in milliseconds.
    pub fn elapsed_ms(&self) -> i64 {
        (self.written.load(Ordering::Relaxed) * 1000 / SAMPLE_RATE) as i64
    }

    /// The audio that waits for the transcription, in seconds.
    pub fn delay_s(&self) -> u64 {
        let written = self.written.load(Ordering::Relaxed);
        written.saturating_sub(self.processed.load(Ordering::Relaxed)) / SAMPLE_RATE
    }
}

/// A recording that is ready to start: the models are loaded and the microphone is open.
struct Opened {
    hearing_id: i64,
    case_id: i64,
    case_name: String,
    title: String,
    device: String,
    model: PathBuf,
    capture: Box<dyn Capture>,
    storage: Storage,
    work: async_channel::Sender<Work>,
    work_receiver: async_channel::Receiver<Work>,
    processor: Processor,
    written: Arc<AtomicU64>,
    processed: Arc<AtomicU64>,
}

/// A recording after Finalizar, until its save ends.
struct Saving {
    hearing_id: i64,
    task: Task<()>,
    /// The result of the end of the hearing: set after the audio file is complete, before the
    /// last segments.
    ended: Arc<Mutex<Option<Result<(), AudioError>>>>,
}

/// The refine pass of a hearing.
struct RefineRun {
    hearing_id: i64,
    model: PathBuf,
    cancel: Arc<AtomicBool>,
    task: Task<()>,
}

enum RefineMessage {
    Event(Event),
    Done(Result<usize, AudioError>),
}

/// The live recording and the refine pass. The app owns one: see [`Recording::install`].
pub struct Recording {
    store: Arc<Mutex<Store>>,
    backend: Arc<dyn Backend>,
    data_dir: PathBuf,
    live: Option<Live>,
    /// The hearing whose recording starts now.
    starting: Option<i64>,
    /// Ends when the start work ends.
    start_done: Option<async_channel::Receiver<()>>,
    /// The result of the start work, so that the quit can also complete it.
    opened: Arc<Mutex<Option<Opened>>>,
    /// Set at quit: a start that ends after it does not record.
    closed: Arc<AtomicBool>,
    /// Set when the quit timeout ends: the transcription stops at its next step.
    abort: Arc<AtomicBool>,
    /// The speaker of the next speech.
    speaker: Speaker,
    saving: Vec<Saving>,
    refine: Option<RefineRun>,
}

impl EventEmitter<RecordingEvent> for Recording {}

impl Recording {
    fn new(store: Arc<Mutex<Store>>, env: &HearingsEnv) -> Self {
        Self {
            store,
            backend: env.backend.clone(),
            data_dir: env.data_dir.clone(),
            live: None,
            starting: None,
            start_done: None,
            opened: Arc::default(),
            closed: Arc::default(),
            abort: Arc::default(),
            speaker: Speaker::Other,
            saving: Vec::new(),
            refine: None,
        }
    }

    /// Creates the recording of the app, and completes it at quit: Cmd+Q, the menu, and the
    /// window close all quit through the app. The app keeps the recording, so the window can
    /// close first.
    pub fn install(store: Arc<Mutex<Store>>, env: &HearingsEnv, cx: &mut App) -> Entity<Self> {
        let recording = cx.new(|_| Self::new(store, env));
        let handle = recording.clone();
        App::on_app_quit(cx, move |cx| {
            if let Err(error) = handle.update(cx, |recording, cx| recording.stop_all(cx)) {
                eprintln!("Nicolaw did not complete the save of the recording at quit: {error:?}");
            }
            async {}
        })
        .detach();
        recording
    }

    pub fn live(&self) -> Option<&Live> {
        self.live.as_ref()
    }

    pub fn live_hearing(&self) -> Option<i64> {
        self.live.as_ref().map(|live| live.hearing_id)
    }

    pub fn starting(&self) -> Option<i64> {
        self.starting
    }

    /// True while a recording runs or starts.
    pub fn busy(&self) -> bool {
        self.live.is_some() || self.starting.is_some()
    }

    /// True while the hearing saves after Finalizar.
    pub fn is_saving(&self, hearing_id: i64) -> bool {
        self.saving
            .iter()
            .any(|saving| saving.hearing_id == hearing_id)
    }

    pub fn any_saving(&self) -> bool {
        !self.saving.is_empty()
    }

    pub fn refining(&self) -> Option<i64> {
        self.refine.as_ref().map(|refine| refine.hearing_id)
    }

    pub fn speaker(&self) -> Speaker {
        self.speaker
    }

    /// True while the model file is loaded for a recording or a refine pass.
    pub fn in_use(&self, path: &std::path::Path) -> bool {
        self.live.as_ref().is_some_and(|live| live.model == path)
            || self
                .refine
                .as_ref()
                .is_some_and(|refine| refine.model == path)
    }

    /// Loads the models, opens the microphone, and starts the recording of the hearing.
    /// `device` is a microphone id, or `None` for the system microphone.
    pub fn start(
        &mut self,
        hearing_id: i64,
        paths: ModelPaths,
        device: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.busy() {
            cx.emit(RecordingEvent::Failed(AudioError::Busy));
            return;
        }
        if self.refine.is_some() {
            cx.emit(RecordingEvent::Failed(AudioError::Refining));
            return;
        }
        self.starting = Some(hearing_id);
        cx.notify();
        let (done, start_done) = async_channel::bounded(1);
        self.start_done = Some(start_done);
        let backend = self.backend.clone();
        let store = self.store.clone();
        let slot = self.opened.clone();
        let closed = self.closed.clone();
        let speaker = self.speaker;
        let now = cx.background_executor().now();
        let wav = self
            .data_dir
            .join("hearings")
            .join(format!("{hearing_id}.wav"));
        let (input, input_receiver) = async_channel::unbounded();
        let work = cx.background_spawn({
            let input = input.clone();
            async move {
                let opened = open(
                    hearing_id, &paths, device, &wav, speaker, &backend, &store, &closed, input,
                    now,
                );
                let result = opened.map(|opened| {
                    *slot.lock().unwrap() = Some(opened);
                });
                done.try_send(()).ok();
                result
            }
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this, cx| {
                this.starting = None;
                this.start_done = None;
                let opened = this.opened.lock().unwrap().take();
                match (result, opened) {
                    (Ok(()), Some(opened)) => {
                        this.begin_live(opened, input, input_receiver, cx);
                        cx.emit(RecordingEvent::Started { hearing_id });
                    }
                    // The quit already took the recording.
                    (Ok(()), None) => {}
                    (Err(error), _) => cx.emit(RecordingEvent::Failed(error)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Starts the workers of an open recording.
    fn begin_live(
        &mut self,
        opened: Opened,
        input: async_channel::Sender<AudioInput>,
        input_receiver: async_channel::Receiver<AudioInput>,
        cx: &mut Context<Self>,
    ) {
        let Opened {
            hearing_id,
            case_id,
            case_name,
            title,
            device,
            model,
            capture,
            mut storage,
            work,
            work_receiver,
            processor,
            written,
            processed,
        } = opened;
        let (events, event_receiver) = async_channel::unbounded();
        let executor = cx.background_executor().clone();
        let storage = cx.background_spawn({
            let events = events.clone();
            let executor = executor.clone();
            async move {
                while let Ok(input) = input_receiver.recv().await {
                    for event in storage.push(input, executor.now()) {
                        events.send(event).await.ok();
                    }
                }
                let (finished, pending) = storage.finish();
                for event in finished {
                    events.send(event).await.ok();
                }
                for item in pending {
                    work.send(item).await.ok();
                }
            }
        });
        let abort = self.abort.clone();
        let transcription = cx.background_spawn(async move {
            let mut processor = processor;
            while let Ok(item) = work_receiver.recv().await {
                if abort.load(Ordering::Relaxed) {
                    break;
                }
                for event in processor.push(item) {
                    events.send(event).await.ok();
                }
            }
            for event in processor.finish() {
                events.send(event).await.ok();
            }
        });
        let ticker = cx.background_spawn({
            let input = input.clone();
            async move {
                loop {
                    executor.timer(TICK).await;
                    if input.send(AudioInput::Tick).await.is_err() {
                        break;
                    }
                }
            }
        });
        cx.spawn(async move |this, cx| {
            while let Ok(event) = event_receiver.recv().await {
                if this
                    .update(cx, |this, cx| this.apply(hearing_id, case_id, event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        self.live = Some(Live {
            hearing_id,
            case_id,
            case_name,
            title,
            device,
            model,
            paused: false,
            interrupted: false,
            health: Health::Waiting,
            level: 0.0,
            written,
            processed,
            capture,
            input,
            storage,
            transcription,
            ticker,
        });
    }

    fn apply(&mut self, hearing_id: i64, case_id: i64, event: Event, cx: &mut Context<Self>) {
        if let Some(live) = self
            .live
            .as_mut()
            .filter(|live| live.hearing_id == hearing_id)
        {
            match &event {
                Event::Level(level) => live.level = *level,
                Event::Health(health) => live.health = *health,
                Event::Interrupted(_) => {
                    // The audio until this point is saved. Finalizar completes the hearing.
                    live.interrupted = true;
                    live.level = 0.0;
                    live.capture.stop();
                }
                _ => {}
            }
        }
        cx.emit(RecordingEvent::Audio {
            hearing_id,
            case_id,
            event,
        });
        cx.notify();
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        let Some(live) = self
            .live
            .as_mut()
            .filter(|live| !live.paused && !live.interrupted)
        else {
            return;
        };
        live.capture.stop();
        live.input.try_send(AudioInput::Pause).ok();
        live.paused = true;
        live.level = 0.0;
        cx.notify();
    }

    pub fn resume(&mut self, cx: &mut Context<Self>) {
        let Some(live) = self
            .live
            .as_mut()
            .filter(|live| live.paused && !live.interrupted)
        else {
            return;
        };
        live.input.try_send(AudioInput::Resume).ok();
        match live.capture.start() {
            Ok(()) => live.paused = false,
            Err(error) => cx.emit(RecordingEvent::Failed(error)),
        }
        cx.notify();
    }

    pub fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        match self.live.as_ref().map(|live| live.paused) {
            Some(true) => self.resume(cx),
            Some(false) => self.pause(cx),
            None => {}
        }
    }

    /// Changes the speaker of the next speech. During a recording, the change goes into the
    /// audio stream at this point, so the open segment ends here.
    pub fn switch_speaker(&mut self, cx: &mut Context<Self>) {
        self.speaker = match self.speaker {
            Speaker::Accused => Speaker::Other,
            Speaker::Other => Speaker::Accused,
        };
        if let Some(live) = &self.live {
            live.input.try_send(AudioInput::Speaker(self.speaker)).ok();
        }
        cx.notify();
    }

    /// Stops the microphone and saves the recording: the audio file, the end of the hearing,
    /// then the last segments. Does nothing without a live recording.
    pub fn finish(&mut self, cx: &mut Context<Self>) {
        let Some(live) = self.live.take() else {
            return;
        };
        let Live {
            hearing_id,
            mut capture,
            input,
            storage,
            transcription,
            ticker,
            ..
        } = live;
        // The input closes when the ticker, the capture, and this sender are gone.
        drop(ticker);
        capture.stop();
        drop(capture);
        drop(input);
        let store = self.store.clone();
        let ended = Arc::new(Mutex::new(None));
        let (saved, saved_receiver) = async_channel::bounded(1);
        let task = cx.background_spawn({
            let ended = ended.clone();
            async move {
                storage.await;
                let result = store
                    .lock()
                    .unwrap()
                    .end_hearing(hearing_id)
                    .map_err(|_| AudioError::Storage);
                *ended.lock().unwrap() = Some(result);
                transcription.await;
                saved.try_send(result).ok();
            }
        });
        self.saving.push(Saving {
            hearing_id,
            task,
            ended,
        });
        cx.spawn(async move |this, cx| {
            let result = saved_receiver
                .recv()
                .await
                .unwrap_or(Err(AudioError::Storage));
            this.update(cx, |this, cx| {
                this.saving.retain(|saving| saving.hearing_id != hearing_id);
                cx.emit(RecordingEvent::Saved { hearing_id, result });
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Transcribes the segments of an ended hearing again with the refine model, then unloads
    /// the model. The store keeps the live text of each segment.
    pub fn refine(
        &mut self,
        hearing_id: i64,
        case_id: i64,
        model: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.refine.is_some() || self.busy() {
            return;
        }
        let backend = self.backend.clone();
        let store = self.store.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let (messages, receiver) = async_channel::unbounded();
        let task = cx.background_spawn({
            let cancel = cancel.clone();
            let model = model.clone();
            async move {
                let result = (|| {
                    let wav = store
                        .lock()
                        .unwrap()
                        .hearing(hearing_id)
                        .map_err(|_| AudioError::Storage)?
                        .audio_path
                        .ok_or(AudioError::AudioFile)?;
                    let mut transcriber = backend.load_refiner(&model)?;
                    refine_hearing(
                        transcriber.as_mut(),
                        &store,
                        hearing_id,
                        &wav,
                        &cancel,
                        |event| {
                            messages.try_send(RefineMessage::Event(event)).ok();
                        },
                    )
                    // The model unloads when the transcriber drops, here.
                })();
                messages.try_send(RefineMessage::Done(result)).ok();
            }
        });
        self.refine = Some(RefineRun {
            hearing_id,
            model,
            cancel,
            task,
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            while let Ok(message) = receiver.recv().await {
                let done = this.update(cx, |this, cx| match message {
                    RefineMessage::Event(event) => {
                        cx.emit(RecordingEvent::Audio {
                            hearing_id,
                            case_id,
                            event,
                        });
                        false
                    }
                    RefineMessage::Done(result) => {
                        this.refine = None;
                        cx.emit(RecordingEvent::Refined { hearing_id, result });
                        cx.notify();
                        true
                    }
                });
                if done.unwrap_or(true) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Completes all work at quit, and waits for it: a start that runs, the live recording,
    /// the saves after Finalizar, and the refine pass, which stops at its next segment. The
    /// audio file and the end of the hearing come first; the last segments follow. Safe to
    /// call more than one time. Returns the first save error.
    pub fn stop_all(&mut self, cx: &mut Context<Self>) -> Result<(), AudioError> {
        self.closed.store(true, Ordering::Relaxed);
        if let Some(start_done) = self.start_done.take() {
            cx.foreground_executor()
                .block_with_timeout(QUIT_TIMEOUT, async move {
                    start_done.recv().await.ok();
                })
                .ok();
            self.starting = None;
            // A start that ended before the quit: record nothing more, and save it.
            let opened = self.opened.lock().unwrap().take();
            if let Some(opened) = opened {
                let (input, input_receiver) = async_channel::unbounded();
                self.begin_live(opened, input, input_receiver, cx);
            }
        }
        self.finish(cx);
        if let Some(refine) = &self.refine {
            refine.cancel.store(true, Ordering::Relaxed);
        }
        let saving = std::mem::take(&mut self.saving);
        let mut tasks: Vec<Task<()>> = Vec::new();
        let mut results = Vec::new();
        for Saving { task, ended, .. } in saving {
            tasks.push(task);
            results.push(ended);
        }
        tasks.extend(self.refine.take().map(|refine| refine.task));
        let mut wait = Box::pin(async move {
            for task in tasks {
                task.await;
            }
        });
        let executor = cx.foreground_executor().clone();
        if let Err(rest) = executor.block_with_timeout(QUIT_TIMEOUT, wait.as_mut()) {
            // The transcription is too slow: stop it at its next step, so that its model is
            // free before the process exits. The audio file is complete.
            self.abort.store(true, Ordering::Relaxed);
            executor.block_with_timeout(ABORT_TIMEOUT, rest).ok();
        }
        results
            .iter()
            .map(|ended| ended.lock().unwrap().unwrap_or(Err(AudioError::Storage)))
            .find(Result::is_err)
            .unwrap_or(Ok(()))
    }
}

/// The start work: checks the microphone, loads the models, opens the microphone, creates
/// the audio file, and starts the capture. Blocks: run it on a background task.
#[allow(clippy::too_many_arguments)]
fn open(
    hearing_id: i64,
    paths: &ModelPaths,
    device: Option<String>,
    wav: &std::path::Path,
    speaker: Speaker,
    backend: &Arc<dyn Backend>,
    store: &Arc<Mutex<Store>>,
    closed: &AtomicBool,
    input: async_channel::Sender<AudioInput>,
    now: std::time::Instant,
) -> Result<Opened, AudioError> {
    if let Some(id) = &device
        && !backend
            .microphones()?
            .iter()
            .any(|microphone| microphone.id == *id)
    {
        return Err(AudioError::MicrophoneMissing);
    }
    let Engines { vad, transcriber } = backend.load(paths)?;
    if closed.load(Ordering::Relaxed) {
        return Err(AudioError::Busy);
    }
    let mut capture = backend.open_microphone(device.as_deref(), input)?;
    let (case_id, case_name) = store
        .lock()
        .unwrap()
        .hearing_case(hearing_id)
        .map_err(|_| AudioError::Storage)?;
    let title = store
        .lock()
        .unwrap()
        .hearing(hearing_id)
        .map_err(|_| AudioError::Storage)?
        .title;
    let written = Arc::new(AtomicU64::new(0));
    let processed = Arc::new(AtomicU64::new(0));
    let (work, work_receiver) = async_channel::bounded(STT_QUEUE);
    let storage = Storage::create(wav, written.clone(), work.clone(), now)?;
    let source = WavSource::open(wav, storage.data_offset()).map_err(|_| AudioError::AudioFile)?;
    let processor = Processor::new(
        hearing_id,
        store.clone(),
        speaker,
        vad,
        transcriber,
        Some(source),
        processed.clone(),
    );
    if closed.load(Ordering::Relaxed) {
        return Err(AudioError::Busy);
    }
    capture.start()?;
    store
        .lock()
        .unwrap()
        .start_hearing(hearing_id, wav)
        .map_err(|_| AudioError::Storage)?;
    Ok(Opened {
        hearing_id,
        case_id,
        case_name,
        title,
        device: capture.device_name(),
        model: paths.live.clone(),
        capture,
        storage,
        work,
        work_receiver,
        processor,
        written,
        processed,
    })
}

/// The status text of the live recording, for the live strip.
pub fn status_text(live: &Live) -> &'static str {
    if live.interrupted {
        INTERRUPTED
    } else if live.paused {
        "Grabación en pausa"
    } else {
        match live.health {
            Health::Waiting => "Grabando · Esperando la señal del micrófono",
            Health::Ok => "Grabando · Señal recibida",
            Health::NoFrames | Health::Silent => {
                "El micrófono no entrega señal. Revise la entrada seleccionada."
            }
            Health::Quiet => "La señal es baja. Acérquese al micrófono.",
        }
    }
}

pub fn speaker_label(speaker: Speaker) -> &'static str {
    match speaker {
        Speaker::Accused => "Acusado",
        Speaker::Other => "Otra persona",
    }
}

/// Shows the live hearing, from the live strip.
pub type OnReturn = Rc<dyn Fn(&mut Window, &mut App)>;

/// The persistent strip with the controls of the live recording, on all screens. It shows
/// nothing without a live or saving recording. `on_return` shows the live hearing; give it
/// when the user is on an other case or screen.
pub fn render_live_strip(
    recording: &Entity<Recording>,
    on_return: Option<OnReturn>,
    window: &Window,
    cx: &App,
) -> Option<AnyElement> {
    let state = recording.read(cx);
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let strip = h_flex()
        .id("live-strip")
        .flex_none()
        .w_full()
        .px_4()
        .gap_3()
        .items_center()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.secondary);
    let Some(live) = state.live() else {
        if !state.any_saving() {
            return None;
        }
        return Some(
            strip
                .h(px(48.))
                .text_sm()
                .child(Icon::new(IconName::LoaderCircle).small())
                .child(div().id("live-saving").child(SAVING).test_support())
                .test_support()
                .into_any_element(),
        );
    };
    let narrow = window.viewport_size().width < px(NARROW_WIDTH);
    let problem = live.interrupted
        || matches!(
            live.health,
            Health::NoFrames | Health::Silent | Health::Quiet
        );
    let status_color =
        if live.interrupted || matches!(live.health, Health::NoFrames | Health::Silent) {
            theme.danger
        } else if problem {
            theme.warning
        } else {
            muted
        };
    let dot =
        div()
            .flex_none()
            .size(px(10.))
            .rounded_full()
            .bg(if live.paused || live.interrupted {
                muted
            } else {
                theme.danger
            });
    let title = div()
        .id("live-title")
        .min_w_0()
        .text_sm()
        .font_semibold()
        .truncate()
        .child(format!("Grabando: {} · {}", live.case_name, live.title))
        .test_support();
    let status = h_flex()
        .id("live-health")
        .min_w_0()
        .gap_2()
        .text_xs()
        .child(
            div()
                .flex_none()
                .text_color(muted)
                .child(format_time(live.elapsed_ms())),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(status_color)
                .child(status_text(live)),
        )
        .test_support();
    let device = h_flex()
        .id("live-device")
        .flex_none()
        .w(px(200.))
        .gap_2()
        .items_center()
        .text_xs()
        .text_color(muted)
        .child(
            div()
                .min_w_0()
                .truncate()
                .child(format!("Entrada: {}", live.device)),
        )
        .child(
            div().flex_none().w(px(48.)).child(
                Progress::new("mic-level")
                    .xsmall()
                    .value(live.level * 100.)
                    .accessibility_label("Nivel del micrófono"),
            ),
        )
        .test_support();
    let handle = recording.clone();
    let speaker = (!live.interrupted).then(|| {
        Button::new("speaker")
            .outline()
            .flex_none()
            .w(px(176.))
            .label(format!("Habla: {}", speaker_label(state.speaker())))
            .tooltip("Cambiar hablante · ⌘⇧A")
            .on_click(move |_, _, cx| {
                handle.update(cx, |recording, cx| recording.switch_speaker(cx))
            })
    });
    let handle = recording.clone();
    let toggle = (!live.interrupted).then(|| {
        let button = if live.paused {
            Button::new("resume")
                .primary()
                .icon(IconName::Play)
                .label("Reanudar")
                .tooltip("Reanudar la grabación · ⌘⇧P")
        } else {
            Button::new("pause")
                .outline()
                .icon(IconName::Pause)
                .label("Pausar")
                .tooltip("Pausar la grabación · ⌘⇧P")
        };
        button
            .flex_none()
            .min_w(px(96.))
            .on_click(move |_, _, cx| handle.update(cx, |recording, cx| recording.toggle_pause(cx)))
    });
    let handle = recording.clone();
    let finish = Button::new("finish")
        .outline()
        .danger()
        .flex_none()
        .min_w(px(96.))
        .icon(IconName::Square)
        .label("Finalizar")
        .on_click(move |_, _, cx| handle.update(cx, |recording, cx| recording.finish(cx)));
    let back = on_return.map(|on_return| {
        Button::new("live-return")
            .ghost()
            .flex_none()
            .label("Volver a la audiencia")
            .on_click(move |_, window, cx| on_return(window, cx))
    });

    let strip = if narrow {
        // Two stable rows: the recording and its actions, then the input and the signal.
        strip.h(px(80.)).child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    h_flex()
                        .h(px(36.))
                        .gap_3()
                        .items_center()
                        .child(dot)
                        .child(title.flex_1())
                        .children(speaker)
                        .children(toggle)
                        .child(finish),
                )
                .child(
                    h_flex()
                        .h(px(28.))
                        .gap_3()
                        .items_center()
                        .child(status.flex_1())
                        .child(device)
                        .children(back),
                ),
        )
    } else {
        strip
            .h(px(48.))
            .child(dot)
            .child(v_flex().flex_1().min_w_0().child(title).child(status))
            .child(device)
            .children(speaker)
            .children(toggle)
            .child(finish)
            .children(back)
    };
    Some(strip.test_support().into_any_element())
}

/// One model in the model select.
#[derive(Clone)]
struct ModelItem {
    key: &'static str,
    name: SharedString,
}

impl SelectItem for ModelItem {
    type Value = &'static str;

    fn title(&self) -> SharedString {
        self.name.clone()
    }

    fn value(&self) -> &&'static str {
        &self.key
    }
}

/// One microphone in the microphone select.
#[derive(Clone)]
struct MicItem {
    id: String,
    name: SharedString,
}

impl SelectItem for MicItem {
    type Value = String;

    fn title(&self) -> SharedString {
        self.name.clone()
    }

    fn value(&self) -> &String {
        &self.id
    }
}

/// The suggestions that the panel shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    Open,
    Confirmed,
    Dismissed,
    All,
}

impl Filter {
    fn matches(self, status: SuggestionStatus) -> bool {
        match self {
            Self::Open => status == SuggestionStatus::Open,
            Self::Confirmed => status == SuggestionStatus::Confirmed,
            Self::Dismissed => status == SuggestionStatus::Dismissed,
            Self::All => true,
        }
    }
}

/// One filter in the filter select.
#[derive(Clone)]
struct FilterItem {
    filter: Filter,
    title: SharedString,
}

impl SelectItem for FilterItem {
    type Value = Filter;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Filter {
        &self.filter
    }
}

fn filter_items() -> Vec<FilterItem> {
    [
        (Filter::Open, "Abiertas"),
        (Filter::Confirmed, "Confirmadas"),
        (Filter::Dismissed, "Descartadas"),
        (Filter::All, "Todas"),
    ]
    .into_iter()
    .map(|(filter, title)| FilterItem {
        filter,
        title: title.into(),
    })
    .collect()
}

/// The user selects the source of a suggestion: the app shows the evidence at the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenEvidence {
    pub evidence_id: i64,
    pub page: u32,
}

/// An analysis model and its files, or `None` when it is not installed.
type AnalysisFiles = (&'static AnalysisModel, Option<Vec<PathBuf>>);

/// The analysis while the switch "Analizar en vivo" is on.
struct Review {
    queue: JobQueue,
    /// The analysis task. `None` while the models load: the queue keeps the segments.
    task: Option<Task<()>>,
    /// Stops the analysis of the current segment when the switch turns off.
    cancel: Arc<AtomicBool>,
}

/// The text of the open segment.
struct PartialRow {
    segment_id: i64,
    speaker: Speaker,
    t_start_ms: i64,
    committed: String,
    tentative: String,
}

pub struct HearingsView {
    store: Arc<Mutex<Store>>,
    env: HearingsEnv,
    recording: Entity<Recording>,
    case_id: Option<i64>,
    hearings: Vec<Hearing>,
    /// False until the first list of hearings arrives.
    loaded: bool,
    selected: Option<i64>,
    segments: Vec<Segment>,
    partial: Option<PartialRow>,
    /// The catalog models and their files. `None` while Nicolaw looks for them.
    models: Option<Vec<ModelStatus>>,
    model_select: Entity<SelectState<Vec<ModelItem>>>,
    /// `None` while Nicolaw looks for them.
    microphones: Option<Vec<Microphone>>,
    mic_select: Entity<SelectState<Vec<MicItem>>>,
    /// The microphone that the user selected, from the settings. `None` is the system
    /// microphone.
    saved_mic: Option<String>,
    /// The download progress in percent, while a download runs.
    download: Option<f32>,
    /// True while the model panel is open.
    show_models: bool,
    /// The message in the alert above the transcript.
    message: Option<SharedString>,
    /// A short confirmation above the transcript. It goes away after a few seconds. A toast
    /// is not used: it can cover the live controls.
    note: Option<&'static str>,
    note_task: Option<Task<()>>,
    transcript_scroll: ScrollHandle,
    /// True when new text arrived while the user reads earlier text.
    new_text: bool,
    /// The files of each analysis model, or `None` for a model that is not installed. `None`
    /// while Nicolaw looks for them.
    analysis_models: Option<Vec<AnalysisFiles>>,
    /// The key of the analysis model that downloads now, and the progress in percent.
    analysis_download: Option<(&'static str, f32)>,
    /// The switch "Analizar en vivo".
    live_analysis: bool,
    review: Option<Review>,
    /// The segments that wait for analysis, and the segments that the full queue dropped.
    review_pending: usize,
    review_dropped: u64,
    /// The suggestions of the selected hearing: open first, then newest first.
    suggestions: Vec<Suggestion>,
    counts: SuggestionCounts,
    filter_select: Entity<SelectState<Vec<FilterItem>>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<OpenEvidence> for HearingsView {}

impl HearingsView {
    pub fn new(
        store: Arc<Mutex<Store>>,
        env: HearingsEnv,
        recording: Entity<Recording>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let model_select = cx.new(|cx| SelectState::new(Vec::<ModelItem>::new(), None, window, cx));
        let mic_select = cx.new(|cx| SelectState::new(Vec::<MicItem>::new(), None, window, cx));
        // The analysis models must be free before the process exits.
        let quit = cx.on_app_quit(|this, _| {
            this.stop_review();
            async {}
        });
        let filter_select = cx.new(|cx| {
            let mut select = SelectState::new(filter_items(), None, window, cx);
            select.set_selected_value(&Filter::Open, window, cx);
            select
        });
        let subscriptions = vec![
            quit,
            cx.subscribe(
                &filter_select,
                |_, _, _: &SelectEvent<Vec<FilterItem>>, cx| cx.notify(),
            ),
            cx.subscribe_in(&recording, window, |this, _, event, window, cx| {
                this.on_recording(event.clone(), window, cx)
            }),
            cx.observe(&recording, |_, _, cx| cx.notify()),
            cx.subscribe(
                &mic_select,
                |this, _, event: &SelectEvent<Vec<MicItem>>, cx| {
                    if let SelectEvent::Confirm(Some(id)) = event {
                        this.save_microphone(id.clone(), cx);
                    }
                },
            ),
        ];

        let view = Self {
            store,
            env,
            recording,
            case_id: None,
            hearings: Vec::new(),
            loaded: false,
            selected: None,
            segments: Vec::new(),
            partial: None,
            models: None,
            model_select,
            microphones: None,
            mic_select,
            saved_mic: None,
            download: None,
            show_models: false,
            message: None,
            note: None,
            note_task: None,
            transcript_scroll: ScrollHandle::new(),
            new_text: false,
            analysis_models: None,
            analysis_download: None,
            live_analysis: true,
            review: None,
            review_pending: 0,
            review_dropped: 0,
            suggestions: Vec::new(),
            counts: SuggestionCounts::default(),
            filter_select,
            _subscriptions: subscriptions,
        };
        view.load_models(window, cx);
        view.load_microphones(window, cx);
        view.load_analysis_models(cx);
        view
    }

    /// Shows the hearings of the case. The recording of an other case continues.
    pub fn set_case(&mut self, case_id: Option<i64>, window: &mut Window, cx: &mut Context<Self>) {
        if self.case_id == case_id && self.loaded {
            return;
        }
        self.case_id = case_id;
        self.hearings.clear();
        self.clear_selection();
        self.loaded = false;
        cx.notify();
        self.load_hearings(None, window, cx);
    }

    /// Shows the hearing of the case, for example the live hearing from the live strip.
    pub fn show_hearing(
        &mut self,
        case_id: i64,
        hearing_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.case_id != Some(case_id) {
            self.case_id = Some(case_id);
            self.hearings.clear();
            self.clear_selection();
            self.loaded = false;
            cx.notify();
            self.load_hearings(Some(hearing_id), window, cx);
        } else {
            self.select(hearing_id, window, cx);
        }
    }

    pub fn selected(&self) -> Option<i64> {
        self.selected
    }

    /// True when the models and the microphones are known, so that Iniciar can start.
    #[cfg(test)]
    pub fn ready(&self) -> bool {
        self.models.is_some() && self.microphones.is_some()
    }

    fn clear_selection(&mut self) {
        self.selected = None;
        self.segments.clear();
        self.partial = None;
        self.message = None;
        self.new_text = false;
        self.suggestions.clear();
        self.counts = SuggestionCounts::default();
    }

    fn on_recording(&mut self, event: RecordingEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            RecordingEvent::Audio {
                hearing_id,
                case_id,
                event,
            } => self.apply(hearing_id, case_id, event, cx),
            RecordingEvent::Started { hearing_id } => {
                if self.selected == Some(hearing_id) {
                    self.set_filter(Filter::Open, window, cx);
                }
                self.start_review(cx);
                self.load_hearings(None, window, cx);
            }
            RecordingEvent::Failed(error) => self.show_error(error, cx),
            RecordingEvent::Saved { hearing_id, result } => {
                if result.is_err() {
                    self.message = Some(SAVE_ERROR.into());
                }
                if self.selected == Some(hearing_id) {
                    self.set_filter(Filter::All, window, cx);
                }
                self.load_hearings(None, window, cx);
            }
            RecordingEvent::Refined { result, .. } => match result {
                Ok(_) => self.show_note("Transcripción refinada.", cx),
                Err(_) => {
                    self.message = Some(REFINE_ERROR.into());
                    cx.notify();
                }
            },
        }
    }

    fn load_models(&self, window: &mut Window, cx: &mut Context<Self>) {
        let dirs = self.env.model_dirs.clone();
        cx.spawn_in(window, async move |this, cx| {
            let models = cx
                .background_spawn(async move { models::discover(&dirs) })
                .await;
            this.update_in(cx, |this, window, cx| this.set_models(models, window, cx))
                .ok();
        })
        .detach();
    }

    fn set_models(
        &mut self,
        models: Vec<ModelStatus>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items: Vec<ModelItem> = models
            .iter()
            .filter(|status| status.model.live && status.path.is_some())
            .map(|status| ModelItem {
                key: status.model.key,
                name: status.model.name.into(),
            })
            .collect();
        let selected = self
            .model_select
            .read(cx)
            .selected_value()
            .copied()
            .filter(|key| items.iter().any(|item| item.key == *key))
            .or(items.first().map(|item| item.key));
        self.model_select.update(cx, |select, cx| {
            select.set_items(items, window, cx);
            if let Some(key) = selected {
                select.set_selected_value(&key, window, cx);
            }
        });
        self.models = Some(models);
        cx.notify();
    }

    /// Lists the microphones. The select has "Micrófono del sistema" first, then each device.
    /// It shows the microphone that the user selected before, if it is connected.
    fn load_microphones(&self, window: &mut Window, cx: &mut Context<Self>) {
        let backend = self.env.backend.clone();
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (microphones, saved) = cx
                .background_spawn(async move {
                    let saved = store
                        .lock()
                        .unwrap()
                        .setting(MICROPHONE_SETTING)
                        .ok()
                        .flatten()
                        .filter(|id| id != SYSTEM_MICROPHONE);
                    (backend.microphones().unwrap_or_default(), saved)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                let items = std::iter::once(MicItem {
                    id: SYSTEM_MICROPHONE.to_owned(),
                    name: "Micrófono del sistema".into(),
                })
                .chain(microphones.iter().map(|microphone| MicItem {
                    id: microphone.id.clone(),
                    name: microphone.name.clone().into(),
                }))
                .collect();
                let selected = match &saved {
                    None => Some(SYSTEM_MICROPHONE.to_owned()),
                    Some(id) if microphones.iter().any(|microphone| microphone.id == *id) => {
                        Some(id.clone())
                    }
                    // The microphone is not connected: the select shows no value.
                    Some(_) => None,
                };
                this.mic_select.update(cx, |select, cx| {
                    select.set_items(items, window, cx);
                    if let Some(id) = selected {
                        select.set_selected_value(&id, window, cx);
                    }
                });
                this.saved_mic = saved;
                this.microphones = Some(microphones);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save_microphone(&mut self, id: String, cx: &mut Context<Self>) {
        self.saved_mic = (id != SYSTEM_MICROPHONE).then(|| id.clone());
        let store = self.store.clone();
        cx.background_spawn(async move {
            store
                .lock()
                .unwrap()
                .set_setting(MICROPHONE_SETTING, &id)
                .ok();
        })
        .detach();
    }

    /// The device for the recording: `None` for the system microphone.
    fn selected_device(&self, cx: &App) -> Option<String> {
        match self.mic_select.read(cx).selected_value() {
            Some(id) if id == SYSTEM_MICROPHONE => None,
            Some(id) => Some(id.clone()),
            // A selected microphone that is not connected now.
            None => self.saved_mic.clone(),
        }
    }

    /// Reloads the hearings of the case, then selects `select` if it is in the list.
    fn load_hearings(&mut self, select: Option<i64>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(case_id) = self.case_id else {
            self.loaded = true;
            return;
        };
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let hearings = cx
                .background_spawn(async move { store.lock().unwrap().hearings(case_id) })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.case_id != Some(case_id) {
                    return;
                }
                this.loaded = true;
                match hearings {
                    Ok(hearings) => this.hearings = hearings,
                    Err(_) => this.message = Some(LIST_ERROR.into()),
                }
                if let Some(id) = select {
                    this.select(id, window, cx);
                } else if this.selected_hearing().is_none() {
                    this.clear_selection();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn selected_hearing(&self) -> Option<&Hearing> {
        let id = self.selected?;
        self.hearings.iter().find(|hearing| hearing.id == id)
    }

    fn select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected == Some(id) {
            return;
        }
        self.clear_selection();
        self.selected = Some(id);
        // During the recording, the panel is a queue of open suggestions. After it, the panel
        // lists all of them.
        let live = self.recording.read(cx).live_hearing() == Some(id);
        self.set_filter(if live { Filter::Open } else { Filter::All }, window, cx);
        self.load_suggestions(id, cx);
        cx.notify();
        self.load_segments(id, window, cx);
    }

    fn load_segments(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let segments = cx
                .background_spawn(async move { store.lock().unwrap().segments(id) })
                .await;
            this.update(cx, |this, cx| {
                if this.selected != Some(id) {
                    return;
                }
                match segments {
                    // A segment that is still open shows as the partial row, not in the list.
                    Ok(segments) => {
                        let open = this.partial.as_ref().map(|partial| partial.segment_id);
                        let mut loaded: Vec<Segment> = segments
                            .into_iter()
                            .filter(|segment| segment.is_final && Some(segment.id) != open)
                            .collect();
                        // A segment that ended while the list loaded is already in the view.
                        let shown = std::mem::take(&mut this.segments);
                        let stored: Vec<i64> = loaded.iter().map(|segment| segment.id).collect();
                        loaded.extend(
                            shown
                                .into_iter()
                                .filter(|segment| !stored.contains(&segment.id)),
                        );
                        loaded.sort_by_key(|segment| (segment.t_start_ms, segment.id));
                        this.segments = loaded;
                    }
                    Err(_) => this.message = Some(LIST_ERROR.into()),
                }
                this.transcript_scroll.scroll_to_bottom();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_new_hearing_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let today = cx
                .background_spawn(async move { store.lock().unwrap().today().unwrap_or_default() })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.show_new_hearing_dialog(format!("Audiencia {today}"), window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn show_new_hearing_dialog(
        &mut self,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Título de la audiencia")
                .default_value(title)
        });
        let view = cx.entity().downgrade();
        // True after a submit with an empty title: the message shows under the field.
        let empty = Rc::new(Cell::new(false));
        // Returns false if the title is empty, so that the dialog stays open.
        let submit = Rc::new({
            let input = input.clone();
            let empty = empty.clone();
            move |window: &mut Window, cx: &mut App| {
                let title = input.read(cx).value().trim().to_owned();
                if title.is_empty() {
                    empty.set(true);
                    input.focus_handle(cx).focus(window, cx);
                    window.refresh();
                    return false;
                }
                view.update(cx, |this, cx| this.create_hearing(title, window, cx))
                    .ok();
                true
            }
        });
        let initial_focus = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                input.focus_handle(cx).focus(window, cx);
            }
            let on_click = submit.clone();
            let on_ok = submit.clone();
            let error = empty.get().then(|| {
                div()
                    .id("hearing-title-error")
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(TITLE_ERROR)
                    .test_support()
            });
            dialog
                .title("Nueva audiencia")
                .w(px(420.))
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .id("hearing-title")
                                .child(Input::new(&input))
                                .test_support(),
                        )
                        .children(error),
                )
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|button| button.label("Cancelar")))
                        .child(
                            Button::new("create-hearing")
                                .primary()
                                .label("Crear")
                                .on_click(move |_, window, cx| {
                                    if on_click(window, cx) {
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
                .on_ok(move |_, window, cx| on_ok(window, cx))
        });
    }

    fn create_hearing(&mut self, title: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(case_id) = self.case_id else {
            return;
        };
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let created = cx
                .background_spawn(
                    async move { store.lock().unwrap().create_hearing(case_id, &title) },
                )
                .await;
            this.update_in(cx, |this, window, cx| match created {
                Ok(hearing) => this.load_hearings(Some(hearing.id), window, cx),
                Err(_) => {
                    this.message = Some(CREATE_ERROR.into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn model_path(&self, model: &CatalogModel) -> Option<PathBuf> {
        self.models
            .as_ref()?
            .iter()
            .find(|status| status.model.key == model.key)
            .and_then(|status| status.path.clone())
    }

    /// The installed model files for a recording with the selected model, or `None` if a
    /// required model is missing.
    fn model_paths(&self, cx: &App) -> Option<ModelPaths> {
        let key = *self.model_select.read(cx).selected_value()?;
        let live = CATALOG
            .iter()
            .find(|model| model.key == key)
            .and_then(|model| self.model_path(model))?;
        Some(ModelPaths {
            live,
            vad: self.model_path(&SILERO_VAD)?,
        })
    }

    /// The required models that are not installed: the primary live model and the voice
    /// detector.
    fn missing_models(&self) -> Vec<&'static CatalogModel> {
        let Some(models) = &self.models else {
            return Vec::new();
        };
        [&NEMOTRON, &SILERO_VAD]
            .into_iter()
            .filter(|model| {
                models
                    .iter()
                    .any(|status| status.model.key == model.key && status.path.is_none())
            })
            .collect()
    }

    fn show_error(&mut self, error: AudioError, cx: &mut Context<Self>) {
        self.message = Some(error.message().into());
        cx.notify();
    }

    /// Starts the recording of the selected hearing, if it was not recorded before.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        let Some(hearing) = self.selected_hearing() else {
            return;
        };
        if hearing.started_at.is_some() {
            return;
        }
        let hearing_id = hearing.id;
        let Some(paths) = self.model_paths(cx) else {
            return self.show_error(AudioError::NoModel, cx);
        };
        if self.microphones.as_ref().is_some_and(Vec::is_empty) && self.saved_mic.is_none() {
            return self.show_error(AudioError::NoMicrophone, cx);
        }
        let device = self.selected_device(cx);
        self.message = None;
        cx.notify();
        self.recording.update(cx, |recording, cx| {
            recording.start(hearing_id, paths, device, cx)
        });
    }

    /// Shows the events of a recording or a refine pass in the transcript.
    fn apply(&mut self, hearing_id: i64, case_id: i64, event: Event, cx: &mut Context<Self>) {
        // A final answer of the accused waits for analysis. A refined text replaces the live
        // text, so the segment waits again (the analysis checks the speaker).
        match &event {
            Event::Final(segment) if segment.speaker == Speaker::Accused => {
                self.enqueue(case_id, hearing_id, segment.id, &segment.text)
            }
            Event::Refined { segment_id, text } => {
                self.enqueue(case_id, hearing_id, *segment_id, text)
            }
            _ => {}
        }
        let shown = self.selected == Some(hearing_id);
        match event {
            Event::Partial {
                segment_id,
                speaker,
                t_start_ms,
                committed,
                tentative,
            } if shown => {
                let follow = self.at_bottom();
                self.partial = Some(PartialRow {
                    segment_id,
                    speaker,
                    t_start_ms,
                    committed,
                    tentative,
                });
                self.follow(follow);
            }
            Event::Final(segment) | Event::Untranscribed(segment) if shown => {
                let follow = self.at_bottom();
                if self.partial.as_ref().map(|partial| partial.segment_id) == Some(segment.id) {
                    self.partial = None;
                }
                // The list can already have the segment, if it loaded after the segment ended.
                match self
                    .segments
                    .iter_mut()
                    .find(|shown| shown.id == segment.id)
                {
                    Some(shown) => *shown = segment,
                    None => self.segments.push(segment),
                }
                self.follow(follow);
            }
            Event::Discarded { segment_id } => {
                if self.partial.as_ref().map(|partial| partial.segment_id) == Some(segment_id) {
                    self.partial = None;
                }
            }
            Event::Refined { segment_id, text } => {
                if let Some(segment) = self
                    .segments
                    .iter_mut()
                    .find(|segment| segment.id == segment_id)
                {
                    segment.text = text;
                    segment.revision += 1;
                }
            }
            Event::Error(error) | Event::Interrupted(error) => {
                self.message = Some(error.message().into())
            }
            Event::Level(_)
            | Event::Health(_)
            | Event::SpeakerChanged { .. }
            | Event::Partial { .. }
            | Event::Final(_)
            | Event::Untranscribed(_) => return,
        }
        cx.notify();
    }

    /// True when the transcript shows its end, so that new text can scroll into view.
    fn at_bottom(&self) -> bool {
        let max = self.transcript_scroll.max_offset().y;
        max <= px(1.) || self.transcript_scroll.offset().y <= -max + px(4.)
    }

    /// Shows new text: scrolls to it if the user was at the end, else shows the button.
    fn follow(&mut self, at_bottom: bool) {
        if at_bottom {
            self.transcript_scroll.scroll_to_bottom();
        } else {
            self.new_text = true;
        }
    }

    fn show_note(&mut self, note: &'static str, cx: &mut Context<Self>) {
        self.note = Some(note);
        cx.notify();
        let timer = cx.background_executor().timer(Duration::from_secs(4));
        self.note_task = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| {
                this.note = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn follow_live(&mut self, cx: &mut Context<Self>) {
        self.new_text = false;
        self.transcript_scroll.scroll_to_bottom();
        cx.notify();
    }

    fn toggle_speaker(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(hearing_id), Some(case_id)) = (self.selected, self.case_id) else {
            return;
        };
        let Some(segment) = self.segments.iter_mut().find(|segment| segment.id == id) else {
            return;
        };
        let speaker = match segment.speaker {
            Speaker::Accused => Speaker::Other,
            Speaker::Other => Speaker::Accused,
        };
        let previous = std::mem::replace(&mut segment.speaker, speaker);
        cx.notify();
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let saved = cx
                .background_spawn(async move { store.lock().unwrap().set_speaker(id, speaker) })
                .await;
            this.update(cx, |this, cx| {
                if saved.is_err() {
                    if let Some(segment) = this.segments.iter_mut().find(|segment| segment.id == id)
                    {
                        segment.speaker = previous;
                    }
                    this.message = Some(SPEAKER_ERROR.into());
                    cx.notify();
                    return;
                }
                this.show_note("Hablante corregido.", cx);
                this.apply(
                    hearing_id,
                    case_id,
                    Event::SpeakerChanged { segment_id: id },
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }

    /// Downloads model files, one after the other. Damaged files move aside first. Downloads
    /// do not run during a recording.
    fn download_models(
        &mut self,
        models: Vec<&'static CatalogModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.recording.read(cx).busy() {
            self.message = Some(DOWNLOAD_BLOCKED.into());
            cx.notify();
            return;
        }
        if models.is_empty() || self.download.is_some() {
            return;
        }
        let damaged: Vec<PathBuf> = self
            .models
            .iter()
            .flatten()
            .filter(|status| models.iter().any(|model| model.key == status.model.key))
            .filter_map(|status| status.invalid.clone())
            .collect();
        self.download = Some(0.0);
        self.message = None;
        cx.notify();
        let dirs = self.env.model_dirs.clone();
        let total_mb: u32 = models.iter().map(|model| model.size_mb).sum();
        let (sender, receiver) = async_channel::unbounded();
        let job = cx.background_spawn(async move {
            for path in damaged {
                models::quarantine(&path).map_err(|error| error.to_string())?;
            }
            let mut done_mb = 0.0;
            let mut last = 0;
            for model in models {
                models::download(model, &dirs, |done, total| {
                    let part = if total == 0 {
                        0.0
                    } else {
                        done as f32 / total as f32
                    };
                    let percent = (done_mb + part * model.size_mb as f32) / total_mb as f32 * 100.;
                    // One update for each whole percent is enough for the progress bar.
                    if percent as u32 != last {
                        last = percent as u32;
                        sender.try_send(percent).ok();
                    }
                })?;
                done_mb += model.size_mb as f32;
            }
            Ok::<_, String>(())
        });
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(percent) = receiver.recv().await {
                this.update(cx, |this, cx| {
                    this.download = Some(percent.min(100.));
                    cx.notify();
                })
                .ok();
            }
            let result = job.await;
            this.update_in(cx, |this, window, cx| {
                this.download = None;
                if result.is_err() {
                    this.message = Some(DOWNLOAD_ERROR.into());
                }
                this.load_models(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn refine(&mut self, cx: &mut Context<Self>) {
        let (Some(hearing_id), Some(case_id), Some(model)) =
            (self.selected, self.case_id, self.model_path(&COHERE))
        else {
            return;
        };
        self.message = None;
        self.recording.update(cx, |recording, cx| {
            recording.refine(hearing_id, case_id, model, cx)
        });
    }

    fn set_filter(&mut self, filter: Filter, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_select.update(cx, |select, cx| {
            select.set_selected_value(&filter, window, cx)
        });
    }

    fn filter(&self, cx: &App) -> Filter {
        self.filter_select
            .read(cx)
            .selected_value()
            .copied()
            .unwrap_or(Filter::All)
    }

    /// Reads the suggestions of the hearing and their counts from the store.
    fn load_suggestions(&self, hearing_id: i64, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let store = store.lock().unwrap();
                    Ok::<_, rusqlite::Error>((
                        store.suggestions(hearing_id)?,
                        store.suggestion_counts(hearing_id)?,
                    ))
                })
                .await;
            this.update(cx, |this, cx| {
                if this.selected != Some(hearing_id) {
                    return;
                }
                match loaded {
                    Ok((suggestions, counts)) => {
                        this.suggestions = suggestions;
                        this.counts = counts;
                    }
                    Err(_) => this.message = Some(LIST_ERROR.into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_suggestion_status(&mut self, id: i64, status: SuggestionStatus, cx: &mut Context<Self>) {
        let Some(hearing_id) = self.selected else {
            return;
        };
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_spawn(
                    async move { store.lock().unwrap().set_suggestion_status(id, status) },
                )
                .await;
            this.update(cx, |this, cx| {
                if saved.is_err() {
                    this.message = Some(SUGGESTION_ERROR.into());
                }
                this.load_suggestions(hearing_id, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Looks for the files of the analysis models. Starts the analysis if a recording runs.
    fn load_analysis_models(&self, cx: &mut Context<Self>) {
        let dirs = self.env.model_dirs.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    ANALYSIS_MODELS
                        .iter()
                        .map(|&model| (model, models::find_files(&dirs, model)))
                        .collect()
                })
                .await;
            this.update(cx, |this, cx| {
                this.analysis_models = Some(found);
                if this.recording.read(cx).live().is_some() {
                    this.start_review(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The files of the NLI model and of the language model, if both are installed.
    fn analysis_files(&self) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
        let models = self.analysis_models.as_ref()?;
        let files = |key: &str| {
            models
                .iter()
                .find(|(model, _)| model.key == key)
                .and_then(|(_, files)| files.clone())
        };
        Some((files(models::NLI.key)?, files(models::LLM.key)?))
    }

    fn missing_analysis_models(&self) -> Vec<&'static AnalysisModel> {
        self.analysis_models
            .iter()
            .flatten()
            .filter(|(_, files)| files.is_none())
            .map(|(model, _)| *model)
            .collect()
    }

    /// True when the switch is on and the analysis models are installed.
    fn analysis_on(&self) -> bool {
        self.live_analysis && self.analysis_files().is_some()
    }

    /// Starts the analysis, if it is on and does not run. The models load in the background;
    /// the queue keeps the segments until they are ready.
    fn start_review(&mut self, cx: &mut Context<Self>) {
        if !self.live_analysis || self.review.is_some() || self.analysis_models.is_none() {
            return;
        }
        let Some((nli, llm)) = self.analysis_files() else {
            return;
        };
        let queue = JobQueue::new(REVIEW_QUEUE);
        self.review = Some(Review {
            queue: queue.clone(),
            task: None,
            cancel: Arc::default(),
        });
        let backend = self.env.review.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    // The files can be gone since the last look.
                    let present = nli.iter().chain(&llm).all(|path| path.is_file());
                    present.then(|| backend.load(&nli, &llm))
                })
                .await;
            this.update(cx, |this, cx| {
                // The switch can turn off while the models load.
                let Some(review) = this
                    .review
                    .as_mut()
                    .filter(|review| review.queue.same(&queue))
                else {
                    return;
                };
                match loaded {
                    None => {
                        this.review = None;
                        this.apply_review(ReviewEvent::ModelsMissing, cx);
                    }
                    Some(Ok(engines)) => {
                        let (events, receiver) = async_channel::unbounded();
                        let analyzer = Analyzer::new(this.store.clone(), engines)
                            .with_cancel(review.cancel.clone());
                        review.task =
                            Some(cx.background_spawn(analyzer::run(analyzer, queue, events)));
                        this.listen_review(receiver, cx);
                    }
                    Some(Err(error)) => {
                        this.review = None;
                        this.message = Some(error.message().into());
                        // Damaged files: show the models as not installed, so that the panel
                        // offers the downloads and the switch shows off.
                        for (_, files) in this.analysis_models.iter_mut().flatten() {
                            *files = None;
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Stops the analysis. The segments in the queue are not analyzed.
    fn stop_review(&mut self) {
        if let Some(review) = self.review.take() {
            review.queue.close();
            review.cancel.store(true, Ordering::Relaxed);
        }
        self.review_pending = 0;
    }

    fn listen_review(
        &mut self,
        events: async_channel::Receiver<ReviewEvent>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |this, cx| this.apply_review(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn enqueue(&mut self, case_id: i64, hearing_id: i64, segment_id: i64, text: &str) {
        if let Some(review) = &self.review {
            review.queue.push(Job {
                case_id,
                hearing_id,
                segment_id,
                text_hash: text_hash(text),
            });
        }
    }

    fn apply_review(&mut self, event: ReviewEvent, cx: &mut Context<Self>) {
        match event {
            ReviewEvent::SuggestionAdded(suggestion) => {
                if self.selected == Some(suggestion.hearing_id) {
                    self.load_suggestions(suggestion.hearing_id, cx);
                }
            }
            ReviewEvent::SuggestionsRemoved { hearing_id } => {
                if self.selected == Some(hearing_id) {
                    self.load_suggestions(hearing_id, cx);
                }
            }
            ReviewEvent::Progress { pending, dropped } => {
                self.review_pending = pending;
                self.review_dropped = dropped;
            }
            // The files are gone: look again, so that the panel offers the downloads.
            ReviewEvent::ModelsMissing => self.load_analysis_models(cx),
            ReviewEvent::Error(message) => self.message = Some(message.into()),
        }
        cx.notify();
    }

    fn toggle_live_analysis(&mut self, on: bool, cx: &mut Context<Self>) {
        self.live_analysis = on;
        if !on {
            self.stop_review();
        } else if self.recording.read(cx).live().is_some() {
            self.start_review(cx);
        }
        cx.notify();
    }

    /// Downloads the files of one analysis model.
    fn download_analysis_model(
        &mut self,
        model: &'static AnalysisModel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.analysis_download.is_some() {
            return;
        }
        self.analysis_download = Some((model.key, 0.0));
        cx.notify();
        let dirs = self.env.model_dirs.clone();
        let (sender, receiver) = async_channel::unbounded();
        let job = cx.background_spawn(async move {
            let mut last = 0;
            models::download_files(model, &dirs, |part| {
                // One update for each whole percent is enough for the progress bar.
                let percent = (part * 100.0) as u32;
                if percent != last {
                    last = percent;
                    sender.try_send(percent as f32).ok();
                }
            })
        });
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(percent) = receiver.recv().await {
                this.update(cx, |this, cx| {
                    this.analysis_download = Some((model.key, percent.min(100.)));
                    cx.notify();
                })
                .ok();
            }
            let result = job.await;
            this.update(cx, |this, cx| {
                this.analysis_download = None;
                if result.is_err() {
                    this.message = Some(DOWNLOAD_ERROR.into());
                }
                this.load_analysis_models(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hearing) = self.selected_hearing().cloned() else {
            return;
        };
        let view = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let view = view.clone();
            let id = hearing.id;
            alert
                .title("¿Eliminar esta audiencia?")
                .description(SharedString::from(format!(
                    "Nicolaw eliminará \"{}\", su transcripción y su audio. No puede deshacer \
                     esta acción.",
                    hearing.title
                )))
                .confirm()
                .ok_text("Eliminar")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("Cancelar")
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| this.delete(id, window, cx)).ok();
                    true
                })
        });
    }

    /// True while the hearing records, starts, saves, or refines: it cannot be deleted.
    fn in_use(&self, id: i64, cx: &App) -> bool {
        let recording = self.recording.read(cx);
        recording.live_hearing() == Some(id)
            || recording.starting().is_some()
            || recording.is_saving(id)
            || recording.refining() == Some(id)
    }

    fn delete(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_use(id, cx) {
            return;
        }
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let deleted = cx
                .background_spawn(async move { store.lock().unwrap().delete_hearing(id) })
                .await;
            this.update_in(cx, |this, window, cx| {
                if deleted.is_err() {
                    this.message = Some(DELETE_ERROR.into());
                }
                this.load_hearings(None, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn open_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        window.open_sheet_at(Placement::Left, cx, move |sheet, _, cx| {
            let Some(this) = view.upgrade() else {
                return sheet;
            };
            sheet
                .title("Audiencias")
                .size(px(300.))
                .child(this.read(cx).render_list_body(view.clone(), true, cx))
        });
    }

    /// The new hearing button and the hearing rows. In the drawer, a selection closes it.
    fn render_list_body(&self, view: WeakEntity<Self>, drawer: bool, cx: &App) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let live_id = self.recording.read(cx).live_hearing();
        let rows = self.hearings.iter().map(|hearing| {
            let id = hearing.id;
            let when = hearing
                .started_at
                .clone()
                .unwrap_or_else(|| "Sin iniciar".to_owned());
            let tag = if live_id == Some(id) {
                Some(Tag::danger().small().child("En curso"))
            } else if hearing.ended_at.is_some() {
                Some(Tag::secondary().small().child("Finalizada"))
            } else {
                None
            };
            let view = view.clone();
            ListItem::new(("hearing", id as u64))
                .rounded(radius)
                .py_2()
                .selected(self.selected == Some(id))
                .accessibility_label(hearing.title.clone())
                .on_click(move |_, window, cx| {
                    view.update(cx, |this, cx| this.select(id, window, cx)).ok();
                    if drawer {
                        window.close_sheet(cx);
                    }
                })
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(div().text_sm().truncate().child(hearing.title.clone()))
                                .child(div().text_xs().text_color(muted).child(when)),
                        )
                        .children(tag),
                )
                .into_any_element()
        });
        let empty = (self.loaded && self.hearings.is_empty()).then(|| {
            div()
                .p_2()
                .text_sm()
                .text_color(muted)
                .child("No hay audiencias. Seleccione Nueva audiencia.")
        });
        let new = view.clone();
        v_flex()
            .size_full()
            .min_h_0()
            .child(
                div().flex_none().px_2().pt_2().child(
                    Button::new("new-hearing")
                        .outline()
                        .w_full()
                        .icon(IconName::Plus)
                        .label("Nueva audiencia")
                        .disabled(self.case_id.is_none())
                        .on_click(move |_, window, cx| {
                            if drawer {
                                window.close_sheet(cx);
                            }
                            new.update(cx, |this, cx| this.open_new_hearing_dialog(window, cx))
                                .ok();
                        }),
                ),
            )
            .child(
                v_flex()
                    .id("hearing-list")
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .gap_1()
                    .children(rows)
                    .children(empty)
                    .overflow_y_scrollbar(),
            )
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        v_flex()
            .id("hearing-pane")
            .flex_none()
            .w(px(224.))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(self.render_list_body(view, false, cx))
            .test_support()
    }

    /// The 40-pixel title row: the drawer button in a narrow window, the hearing title, and
    /// the secondary actions.
    fn render_title_row(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let hearing = self.selected_hearing();
        let recording = self.recording.read(cx);
        let refine = hearing.filter(|hearing| {
            hearing.ended_at.is_some()
                && hearing.audio_path.is_some()
                && self.model_path(&COHERE).is_some()
        });
        let refining = recording.refining();
        let can_refine = refining.is_none() && !recording.busy();
        h_flex()
            .id("hearing-title-row")
            .flex_none()
            .h(px(40.))
            .px_4()
            .gap_2()
            .items_center()
            .when(narrow, |row| {
                row.child(
                    Button::new("hearings-drawer")
                        .outline()
                        .small()
                        .flex_none()
                        .icon(IconName::PanelLeft)
                        .label("Audiencias")
                        .on_click(cx.listener(|this, _, window, cx| this.open_drawer(window, cx))),
                )
            })
            .child(
                div().flex_1().min_w_0().font_semibold().truncate().child(
                    hearing
                        .map(|hearing| hearing.title.clone())
                        .unwrap_or_default(),
                ),
            )
            .children(refine.map(|hearing| {
                let id = hearing.id;
                Button::new("refine")
                    .ghost()
                    .small()
                    .flex_none()
                    .label(if refining == Some(id) {
                        "Refinando…"
                    } else {
                        "Refinar transcripción"
                    })
                    .loading(refining == Some(id))
                    .disabled(!can_refine)
                    .tooltip(
                        "Transcribe de nuevo con Cohere Transcribe. El texto en vivo se conserva.",
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.refine(cx)))
            }))
            .child(
                Button::new("models-toggle")
                    .ghost()
                    .small()
                    .flex_none()
                    .label("Modelos")
                    .icon(if self.show_models {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_models = !this.show_models;
                        cx.notify();
                    })),
            )
            .children(hearing.map(|hearing| {
                Button::new("delete-hearing")
                    .ghost()
                    .danger()
                    .small()
                    .flex_none()
                    .label("Eliminar")
                    .disabled(self.in_use(hearing.id, cx))
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)))
            }))
            .test_support()
    }

    /// The model panel: the state of each model, with Descargar and Reparar.
    fn render_models(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let recording = self.recording.read(cx);
        let blocked = recording.busy() || self.download.is_some();
        let rows = self.models.iter().flatten().map(|status| {
            let model = status.model;
            let (state, tag, action) = match (&status.path, &status.invalid) {
                (Some(path), _) => {
                    let installed = match models::quant(model, path) {
                        Some(quant) => format!("Instalado · {quant}"),
                        None => "Instalado".to_owned(),
                    };
                    let tag = if recording.in_use(path) {
                        Tag::success().small().child("En uso")
                    } else {
                        Tag::secondary().small().child("No cargado")
                    };
                    (installed, Some(tag), None)
                }
                (None, Some(_)) => (
                    "El archivo del modelo está dañado. Seleccione Reparar.".to_owned(),
                    Some(Tag::danger().small().child("Dañado")),
                    Some("Reparar"),
                ),
                (None, None) => (
                    format!("No instalado · {} MB", model.size_mb),
                    None,
                    Some("Descargar"),
                ),
            };
            h_flex()
                .id(SharedString::from(format!("model-{}", model.key)))
                .gap_2()
                .items_center()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().child(model.name))
                        .child(div().text_xs().text_color(muted).truncate().child(state)),
                )
                .children(tag)
                .children(action.map(|label| {
                    Button::new(SharedString::from(format!("model-action-{}", model.key)))
                        .outline()
                        .small()
                        .label(label)
                        .disabled(blocked)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.download_models(vec![model], window, cx)
                        }))
                }))
                .test_support()
        });
        v_flex()
            .id("models-panel")
            .flex_none()
            .mx_4()
            .p_3()
            .gap_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .children(rows)
            .children(self.download.map(|percent| {
                Progress::new("download-progress")
                    .value(percent)
                    .accessibility_label("Descarga")
            }))
            .test_support()
    }

    /// The setup row before a recording: the model and the microphone, and Iniciar.
    fn render_setup(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let recording = self.recording.read(cx);
        let starting = recording.starting().is_some();
        let busy = recording.busy();
        let refining = recording.refining().is_some();
        let has_models = self.model_select.read(cx).selected_value().is_some();
        let missing = self.missing_models();
        let download = (!missing.is_empty()).then(|| {
            let names: Vec<&str> = missing.iter().map(|model| model.name).collect();
            let size: u32 = missing.iter().map(|model| model.size_mb).sum();
            h_flex()
                .flex_none()
                .gap_2()
                .items_center()
                .child(
                    Button::new("download")
                        .outline()
                        .icon(IconName::ArrowDown)
                        .label("Descargar")
                        .loading(self.download.is_some())
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            let missing = this.missing_models();
                            this.download_models(missing, window, cx)
                        })),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("{} · {size} MB", names.join(" y "))),
                )
                .children(self.download.map(|percent| {
                    div().w(px(160.)).child(
                        Progress::new("download-progress")
                            .value(percent)
                            .accessibility_label("Descarga"),
                    )
                }))
        });
        v_flex()
            .id("hearing-setup")
            .flex_none()
            .px_4()
            .gap_2()
            .child(
                h_flex()
                    .flex_none()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(
                        // The select fills its wrapper: the wrapper sets the size.
                        div()
                            .id("model-select-box")
                            .flex_none()
                            .w(px(240.))
                            .h(px(36.))
                            .child(
                                Select::new(&self.model_select)
                                    .id("model-select")
                                    .w_full()
                                    .placeholder("Sin modelo instalado")
                                    .accessibility_label("Modelo de voz")
                                    .disabled(busy || !has_models),
                            )
                            .test_support(),
                    )
                    .child(
                        div()
                            .id("mic-select-box")
                            .flex_none()
                            .w(px(240.))
                            .h(px(36.))
                            .child(
                                Select::new(&self.mic_select)
                                    .id("mic-select")
                                    .w_full()
                                    .placeholder("Micrófono no disponible")
                                    .accessibility_label("Micrófono")
                                    .disabled(busy),
                            )
                            .test_support(),
                    )
                    .child(
                        Button::new("start")
                            .primary()
                            .flex_none()
                            .icon(IconName::Mic)
                            .label("Iniciar")
                            .tooltip(if refining {
                                "Espere a que termine el refinamiento."
                            } else {
                                "Iniciar la grabación · ⌘⇧R"
                            })
                            .loading(starting)
                            .disabled(busy || refining)
                            .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
                    ),
            )
            .children(download)
            .test_support()
    }

    fn render_detail(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let pane = v_flex()
            .id("hearing-main")
            .flex_1()
            .min_w_0()
            .h_full()
            .pt_1()
            .gap_2()
            .child(self.render_title_row(narrow, cx))
            .when(self.show_models, |pane| pane.child(self.render_models(cx)));
        let Some(hearing) = self.selected_hearing() else {
            return pane
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(muted)
                        .child("Seleccione una audiencia."),
                )
                .test_support();
        };
        let recording = self.recording.read(cx);
        // The health, the pause, and the delay of the live recording of this hearing.
        let live = recording
            .live()
            .filter(|live| live.hearing_id == hearing.id)
            .map(|live| (live.health, live.paused, live.delay_s()));
        let saving = recording.is_saving(hearing.id);
        let state = if live.is_some() || recording.starting() == Some(hearing.id) {
            None
        } else if saving {
            Some(SAVING.to_owned())
        } else if let Some(ended) = &hearing.ended_at {
            Some(format!("Grabación finalizada: {ended}."))
        } else if hearing.started_at.is_some() {
            Some("La grabación se detuvo antes de Finalizar.".to_owned())
        } else {
            None
        };
        let setup = (hearing.started_at.is_none() && live.is_none()).then(|| self.render_setup(cx));
        let delay = live
            .filter(|(health, paused, delay)| {
                *health == Health::Ok && !paused && *delay >= DELAY_NOTICE_S
            })
            .map(|(_, _, delay)| delay)
            .map(|delay| {
                div()
                    .id("transcript-delay")
                    .flex_none()
                    .px_4()
                    .text_xs()
                    .text_color(muted)
                    .child(format!(
                        "Transcripción retrasada: {delay} s. El audio sigue guardándose."
                    ))
                    .test_support()
            });

        pane.children(setup)
            .children(state.map(|state| {
                div()
                    .flex_none()
                    .px_4()
                    .text_sm()
                    .text_color(muted)
                    .child(state)
            }))
            .children(self.message.clone().map(|message| {
                div()
                    .id("hearing-message")
                    .flex_none()
                    .px_4()
                    .child(Alert::error("hearing-alert", message).on_close(cx.listener(
                        |this, _, _, cx| {
                            this.message = None;
                            cx.notify();
                        },
                    )))
                    .test_support()
            }))
            .children(delay)
            .children(self.note.map(|note| {
                div()
                    .id("hearing-note")
                    .flex_none()
                    .px_4()
                    .text_xs()
                    .text_color(muted)
                    .child(note)
                    .test_support()
            }))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .px_4()
                    .pb_4()
                    .child(self.render_transcript(hearing, cx)),
            )
            .test_support()
    }

    fn render_transcript(&self, hearing: &Hearing, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let frame = div()
            .id("transcript-frame")
            .relative()
            .size_full()
            .overflow_hidden()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .test_support();
        if self.segments.is_empty() && self.partial.is_none() {
            let text = if hearing.started_at.is_none() {
                "Seleccione Iniciar para grabar la audiencia."
            } else if self.recording.read(cx).live_hearing() == Some(hearing.id) {
                "El texto aparece aquí cuando alguien habla."
            } else {
                "Esta audiencia no tiene transcripción."
            };
            return frame
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_sm()
                .text_color(muted)
                .child(text);
        }

        let rows = self.segments.iter().map(|segment| {
            let id = segment.id;
            let text = if segment.failed {
                div()
                    .text_sm()
                    .italic()
                    .text_color(muted)
                    .child("Fragmento sin transcribir")
            } else {
                div().text_sm().child(segment.text.clone())
            };
            let toggle = (!segment.failed).then(|| {
                Button::new(("toggle-speaker", id as u64))
                    .ghost()
                    .xsmall()
                    .icon(IconName::RefreshCw)
                    .tooltip("Cambiar hablante")
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.toggle_speaker(id, window, cx)),
                    )
            });
            render_row(
                ("segment", id as u64),
                segment.t_start_ms,
                segment.speaker,
                None,
                text,
                toggle,
                muted,
            )
        });
        let partial = self.partial.as_ref().map(|partial| {
            let text = format!("{}{}", partial.committed, partial.tentative);
            let tentative = partial.committed.len()..text.len();
            render_row(
                "partial",
                partial.t_start_ms,
                partial.speaker,
                Some("Texto provisional"),
                div()
                    .text_sm()
                    .child(StyledText::new(text).with_highlights([(
                        tentative,
                        HighlightStyle {
                            color: Some(muted),
                            ..Default::default()
                        },
                    )])),
                None,
                muted,
            )
        });
        let follow = self.new_text.then(|| {
            div()
                .absolute()
                .bottom_3()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    Button::new("follow-live")
                        .primary()
                        .small()
                        .icon(IconName::ArrowDown)
                        .label("Volver al texto en vivo")
                        .on_click(cx.listener(|this, _, _, cx| this.follow_live(cx))),
                )
        });

        frame
            .child(
                v_flex()
                    .id("transcript")
                    .size_full()
                    .track_scroll(&self.transcript_scroll)
                    .overflow_y_scroll()
                    .p_3()
                    .gap_3()
                    .children(rows)
                    .children(partial)
                    .test_support(),
            )
            .vertical_scrollbar(&self.transcript_scroll)
            .children(follow)
    }
}

impl HearingsView {
    /// The suggestions panel: counters, the live analysis switch, the analysis models, the
    /// filter, and the cards.
    fn render_suggestions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let installed = self.analysis_files().is_some();
        let filter = self.filter(cx);
        let cards: Vec<AnyElement> = self
            .suggestions
            .iter()
            .filter(|suggestion| filter.matches(suggestion.status))
            .map(|suggestion| self.render_card(suggestion, cx))
            .collect();
        let empty = cards.is_empty().then(|| {
            let text = if self.suggestions.is_empty() {
                "No hay sugerencias. Nicolaw analiza las declaraciones del acusado cuando terminan."
            } else {
                "No hay sugerencias con este filtro."
            };
            div()
                .id("suggestions-empty")
                .text_sm()
                .text_color(muted)
                .child(text)
                .test_support()
        });
        let missing = self.missing_analysis_models();
        let models = (!missing.is_empty()).then(|| {
            v_flex()
                .id("analysis-models")
                .gap_2()
                .child(div().text_sm().child(
                    "Faltan los modelos de análisis. Nicolaw graba y transcribe la audiencia sin \
                     ellos.",
                ))
                .children(missing.into_iter().map(|model| {
                    let download = self
                        .analysis_download
                        .filter(|(key, _)| *key == model.key)
                        .map(|(_, percent)| percent);
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .justify_between()
                                .child(div().text_xs().text_color(muted).child(format!(
                                    "{} · {} MB",
                                    model.name,
                                    model.size_mb()
                                )))
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "download-{}",
                                        model.key
                                    )))
                                    .outline()
                                    .small()
                                    .icon(IconName::ArrowDown)
                                    .label("Descargar")
                                    .loading(download.is_some())
                                    .disabled(self.analysis_download.is_some())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.download_analysis_model(model, window, cx)
                                    })),
                                ),
                        )
                        .children(download.map(|percent| {
                            Progress::new(SharedString::from(format!(
                                "download-{}-progress",
                                model.key
                            )))
                            .value(percent)
                            .accessibility_label("Descarga")
                        }))
                }))
        });
        let loading = self
            .review
            .as_ref()
            .is_some_and(|review| review.task.is_none());
        let status = if loading {
            Some("Nicolaw carga los modelos de análisis.".to_owned())
        } else if self.review_pending > 0 {
            Some(format!(
                "Declaraciones en análisis: {}",
                self.review_pending
            ))
        } else {
            None
        };
        let dropped = (self.review_dropped > 0).then(|| {
            format!(
                "Nicolaw no analizó {} declaraciones. Había demasiadas en espera.",
                self.review_dropped
            )
        });

        v_flex()
            .id("suggestions-panel")
            .flex_none()
            .w(px(340.))
            .h_full()
            .p_3()
            .gap_3()
            .border_l_1()
            .border_color(cx.theme().border)
            .test_support()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(div().font_semibold().child("Sugerencias"))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Tag::danger()
                                    .small()
                                    .child(format!("Conflictos: {}", self.counts.conflict)),
                            )
                            .child(
                                Tag::warning()
                                    .small()
                                    .child(format!("Revisar: {}", self.counts.check)),
                            ),
                    ),
            )
            .child(
                Switch::new("live-analysis")
                    .label("Analizar en vivo")
                    .checked(self.analysis_on())
                    .disabled(!installed)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.toggle_live_analysis(*checked, cx)
                    })),
            )
            .children(models)
            .children(
                status
                    .into_iter()
                    .chain(dropped)
                    .map(|text| div().text_xs().text_color(muted).child(text)),
            )
            .child(
                // Without this wrapper, the select grows in the column.
                div().flex_none().child(
                    Select::new(&self.filter_select)
                        .id("suggestion-filter")
                        .w_full()
                        .accessibility_label("Filtro de sugerencias"),
                ),
            )
            .child(
                v_flex()
                    .id("suggestion-list")
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .children(cards)
                    .children(empty)
                    .overflow_y_scrollbar(),
            )
    }

    /// One suggestion: level, kind, the claim with the conflicting words in bold, the
    /// evidence passage with its file and page, the explanation, and the decision buttons.
    fn render_card(&self, suggestion: &Suggestion, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let id = suggestion.id;
        let level = match suggestion.level {
            SuggestionLevel::Conflict => Tag::danger().small().child("Posible conflicto"),
            SuggestionLevel::Check => Tag::warning().small().child("Revisar"),
        };
        let status = match suggestion.status {
            SuggestionStatus::Open => None,
            SuggestionStatus::Confirmed => Some(Tag::success().small().child("Confirmada")),
            SuggestionStatus::Dismissed => Some(Tag::secondary().small().child("Descartada")),
        };
        let quote = suggestion.claim_quote.as_deref().and_then(|quote| {
            let start = suggestion.claim.find(quote)?;
            Some(start..start + quote.len())
        });
        let claim = StyledText::new(suggestion.claim.clone()).with_highlights(quote.map(|range| {
            (
                range,
                HighlightStyle {
                    font_weight: Some(FontWeight::BOLD),
                    underline: Some(UnderlineStyle {
                        thickness: px(1.),
                        color: None,
                        wavy: false,
                    }),
                    ..Default::default()
                },
            )
        }));
        let source = OpenEvidence {
            evidence_id: suggestion.evidence_id,
            page: suggestion.page,
        };
        let label = |text: &'static str| div().text_xs().text_color(muted).child(text);

        v_flex()
            .id(("suggestion", id as u64))
            .gap_2()
            .p_3()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(level)
                    .children(
                        suggestion
                            .kind
                            .map(|kind| div().text_xs().text_color(muted).child(kind_label(kind))),
                    )
                    .children(status),
            )
            .child(label("Declaración"))
            .child(div().text_sm().child(claim))
            .child(label("Prueba"))
            .child(
                div()
                    .pl_2()
                    .border_l_2()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .child(format!("«{}»", suggestion.evidence_quote)),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(muted)
                    .child(
                        Button::new(("source", id as u64))
                            .link()
                            .xsmall()
                            .label(suggestion.evidence_name.clone())
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(source))),
                    )
                    .child(format!("· Página {}", suggestion.page)),
            )
            .children(
                suggestion
                    .explanation
                    .clone()
                    .map(|explanation| div().text_sm().child(explanation)),
            )
            .when(suggestion.status == SuggestionStatus::Open, |card| {
                card.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(("confirm", id as u64))
                                .primary()
                                .small()
                                .label("Confirmar")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_suggestion_status(id, SuggestionStatus::Confirmed, cx)
                                })),
                        )
                        .child(
                            Button::new(("dismiss", id as u64))
                                .ghost()
                                .small()
                                .label("Descartar")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_suggestion_status(id, SuggestionStatus::Dismissed, cx)
                                })),
                        ),
                )
            })
            .test_support()
            .into_any_element()
    }
}

pub fn kind_label(kind: SuggestionKind) -> &'static str {
    match kind {
        SuggestionKind::Date => "Fecha",
        SuggestionKind::Amount => "Cantidad",
        SuggestionKind::Identity => "Identidad",
        SuggestionKind::Location => "Lugar",
        SuggestionKind::Sequence => "Secuencia",
        SuggestionKind::Other => "Otro",
    }
}

/// One transcript item: a small row with the time, the speaker, and the correction action,
/// then the statement at full width.
fn render_row(
    id: impl Into<ElementId>,
    t_start_ms: i64,
    speaker: Speaker,
    note: Option<&'static str>,
    text: Div,
    toggle: Option<Button>,
    muted: Hsla,
) -> AnyElement {
    let tag = match speaker {
        Speaker::Accused => Tag::warning(),
        Speaker::Other => Tag::secondary(),
    };
    v_flex()
        .id(id)
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .text_xs()
                .text_color(muted)
                .child(format_time(t_start_ms))
                .child(tag.small().child(speaker_label(speaker)))
                .children(toggle)
                .children(note),
        )
        .child(text.w_full())
        .test_support()
        .into_any_element()
}

/// A position in the hearing audio, as `MM:SS`, or `H:MM:SS` from one hour.
pub fn format_time(ms: i64) -> String {
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

impl Render for HearingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.new_text && self.at_bottom() {
            self.new_text = false;
        }
        let narrow = window.viewport_size().width < px(NARROW_WIDTH);
        h_flex()
            .id("hearings")
            .size_full()
            .items_start()
            .when(!narrow, |screen| screen.child(self.render_list(cx)))
            .child(self.render_detail(narrow, cx))
            .when(self.selected_hearing().is_some(), |screen| {
                screen.child(self.render_suggestions(cx))
            })
            .test_support()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use gpui_kit::component::v_flex;
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _,
        Pixels, Point, Render, ScrollDelta, Styled as _, TestAppContext, Window, WindowBounds,
        WindowOptions, div, point, px, size,
    };

    use super::{
        Filter, HearingsEnv, HearingsView, Recording, SAVE_ERROR, StartRecording, SwitchSpeaker,
        TogglePause, format_time, register_actions, render_live_strip, speaker_label, status_text,
    };
    use crate::audio::fake::FakeBackend;
    use crate::audio::pipeline::Event;
    use crate::audio::{AudioError, Microphone, ModelPaths};
    use crate::extract::{Page, PageSource};
    use crate::models::{
        ANALYSIS_MODELS, COHERE, ModelDirs, NEMOTRON, SILERO_VAD, write_fake_model,
    };
    use crate::review::fake::{FakeLlm, FakeNli, FakeReviewBackend};
    use crate::review::{ReviewError, ReviewEvent};
    use crate::store::{
        NewSuggestion, Segment, Speaker, Store, SuggestionCounts, SuggestionLevel, SuggestionStatus,
    };

    const WAIT: Duration = Duration::from_secs(1);

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nicolaw-hearings-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Model folders with fake files for the live model and the voice detector, or with no
    /// models.
    fn model_dirs(name: &str, installed: bool) -> ModelDirs {
        let root = temp_dir(&format!("models-{name}"));
        let dirs = ModelDirs {
            hf_cache: root.join("hub"),
            handy: root.join("handy"),
        };
        fs::create_dir_all(&dirs.handy).unwrap();
        if installed {
            write_fake_model(&dirs.handy.join(NEMOTRON.file), &NEMOTRON);
            write_fake_model(&dirs.handy.join(SILERO_VAD.file), &SILERO_VAD);
        }
        dirs
    }

    /// The live strip above the hearings screen, as in the app window.
    struct Screen {
        recording: Entity<Recording>,
        view: Entity<HearingsView>,
    }

    impl Render for Screen {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            v_flex()
                .size_full()
                .children(render_live_strip(&self.recording, None, window, cx))
                .child(div().flex_1().min_h_0().child(self.view.clone()))
        }
    }

    struct Opened {
        window: AnyWindowHandle,
        view: Entity<HearingsView>,
        recording: Entity<Recording>,
        store: Arc<Mutex<Store>>,
    }

    fn open(
        name: &str,
        store: Store,
        case: i64,
        backend: &FakeBackend,
        dirs: &ModelDirs,
        cx: &mut TestAppContext,
    ) -> Opened {
        open_sized(
            name,
            store,
            case,
            backend,
            dirs,
            size(px(1100.), px(720.)),
            cx,
        )
    }

    /// Opens the screen, and returns the window and the hearings view.
    fn open_pair(
        name: &str,
        store: Store,
        case: i64,
        backend: &FakeBackend,
        dirs: &ModelDirs,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<HearingsView>) {
        let opened = open(name, store, case, backend, dirs, cx);
        (opened.window, opened.view)
    }

    fn open_with(
        name: &str,
        store: Store,
        case: i64,
        backend: &FakeBackend,
        review: &FakeReviewBackend,
        dirs: &ModelDirs,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<HearingsView>) {
        let opened = open_full(
            name,
            store,
            case,
            backend,
            review,
            dirs,
            size(px(1100.), px(720.)),
            cx,
        );
        (opened.window, opened.view)
    }

    fn open_sized(
        name: &str,
        store: Store,
        case: i64,
        backend: &FakeBackend,
        dirs: &ModelDirs,
        window_size: gpui_kit::Size<Pixels>,
        cx: &mut TestAppContext,
    ) -> Opened {
        let review = FakeReviewBackend::new(FakeLlm::new("[]", "{}"), nli());
        open_full(name, store, case, backend, &review, dirs, window_size, cx)
    }

    #[allow(clippy::too_many_arguments)]
    fn open_full(
        name: &str,
        store: Store,
        case: i64,
        backend: &FakeBackend,
        review: &FakeReviewBackend,
        dirs: &ModelDirs,
        window_size: gpui_kit::Size<Pixels>,
        cx: &mut TestAppContext,
    ) -> Opened {
        let env = HearingsEnv {
            backend: Arc::new(backend.clone()),
            review: Arc::new(review.clone()),
            model_dirs: dirs.clone(),
            data_dir: temp_dir(&format!("data-{name}")),
        };
        let store = Arc::new(Mutex::new(store));
        cx.update(|cx| {
            gpui_kit::init(cx);
            super::bind_keys(cx);
            let recording = Recording::install(store.clone(), &env, cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: window_size,
                })),
                ..Default::default()
            };
            let (window, screen) = gpui_kit::open_window(options, cx, |window, cx| {
                let view = cx.new(|cx| {
                    let mut view =
                        HearingsView::new(store.clone(), env, recording.clone(), window, cx);
                    view.set_case(Some(case), window, cx);
                    view
                });
                cx.new(|_| Screen {
                    recording: recording.clone(),
                    view,
                })
            })
            .unwrap();
            let view = screen.read(cx).view.clone();
            register_actions(&recording, view.downgrade(), cx);
            Opened {
                window,
                view,
                recording,
                store: store.clone(),
            }
        })
    }

    fn store_with_case() -> (Store, i64) {
        let store = Store::open_in_memory().unwrap();
        let case = store.create_case("Caso").unwrap().id;
        (store, case)
    }

    fn click(cx: &mut TestAppContext, window: AnyWindowHandle, id: impl Into<gpui_kit::ElementId>) {
        let id = id.into();
        cx.update_window(window, |_, window, cx| window.click(id, cx))
            .unwrap();
    }

    fn shown(
        cx: &mut TestAppContext,
        window: AnyWindowHandle,
        id: impl Into<gpui_kit::ElementId>,
    ) -> bool {
        let id = id.into();
        cx.update_window(window, |_, window, _| window.try_find(id).is_some())
            .unwrap()
    }

    fn bounds(
        cx: &mut TestAppContext,
        window: AnyWindowHandle,
        id: impl Into<gpui_kit::ElementId>,
    ) -> Bounds<Pixels> {
        let id = id.into();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.find(id).bounds()
        })
        .unwrap()
    }

    /// Waits until the models and the microphones are known.
    async fn wait_until_ready(cx: &mut TestAppContext, opened: &Opened) {
        let view = opened.view.clone();
        cx.wait_for(opened.window, WAIT, move |_, cx| {
            let view = view.read(cx);
            view.microphones.is_some() && view.models.is_some()
        })
        .await;
    }

    /// Selects the hearing and starts its recording.
    async fn start(cx: &mut TestAppContext, opened: &Opened, backend: &FakeBackend, id: i64) {
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, opened.window, ("hearing", id as u64));
        wait_until_ready(cx, opened).await;
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find("start").is_some()
        })
        .await;
        click(cx, opened.window, "start");
        cx.wait_for(opened.window, WAIT, |_, _| {
            backend.input.lock().unwrap().is_some()
        })
        .await;
    }

    fn hearing_with_case(name: &str) -> (Store, i64, i64) {
        let (store, case) = store_with_case();
        let id = store.create_hearing(case, name).unwrap().id;
        (store, case, id)
    }

    #[test]
    fn times_show_minutes_and_seconds_and_hours_when_needed() {
        assert_eq!(format_time(0), "00:00");
        assert_eq!(format_time(65_400), "01:05");
        assert_eq!(format_time(3_725_000), "1:02:05");
    }

    #[test]
    fn speakers_have_spanish_labels() {
        assert_eq!(speaker_label(Speaker::Accused), "Acusado");
        assert_eq!(speaker_label(Speaker::Other), "Otra persona");
    }

    #[gpui_kit::test]
    async fn a_hearing_is_created_recorded_with_speakers_and_finished(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let backend = FakeBackend::new("Yo no estaba allí.");
        let dirs = model_dirs("record", true);
        let opened = open("record", store, case, &backend, &dirs, cx);
        let (window, view) = (opened.window, opened.view.clone());
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("new-hearing").is_some()
        })
        .await;

        // New hearing, with today's date in the default title.
        click(cx, window, "new-hearing");
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("hearing-title").is_some()
        })
        .await;
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        click(cx, window, "create-hearing");
        cx.wait_for(window, WAIT, |window, cx| {
            view.read(cx).selected.is_some() && window.try_find("start").is_some()
        })
        .await;
        let hearing = cx.update(|cx| view.read(cx).hearings[0].clone());
        assert!(hearing.title.starts_with("Audiencia "));
        assert_eq!(hearing.title.len(), "Audiencia 15/10/2026".len());

        // Start: the live model and the voice detector load, and the system microphone
        // records.
        wait_until_ready(cx, &opened).await;
        click(cx, window, "start");
        cx.wait_for(window, WAIT, |_, _| backend.input.lock().unwrap().is_some())
            .await;
        assert_eq!(
            *backend.loaded.lock().unwrap(),
            Some(ModelPaths {
                live: dirs.handy.join(NEMOTRON.file),
                vad: dirs.handy.join(SILERO_VAD.file),
            })
        );
        assert_eq!(*backend.opened.lock().unwrap(), Some(None));
        cx.update(|cx| {
            let recording = opened.recording.read(cx);
            let live = recording.live().unwrap();
            assert_eq!(live.device, "Micrófono de prueba");
            assert_eq!(live.case_name, "Caso");
        });
        assert!(shown(cx, window, "live-strip"));

        // Speech shows a partial row, then silence makes it a final row.
        backend.send(0.5, 10);
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("partial").is_some()
        })
        .await;
        backend.send(0.0, 19);
        cx.wait_for(window, WAIT, |window, cx| {
            window.try_find("partial").is_none() && view.read(cx).segments.len() == 1
        })
        .await;
        let segment = cx.update(|cx| view.read(cx).segments[0].clone());
        assert_eq!(segment.text, "Yo no estaba allí.");
        assert_eq!(segment.speaker, Speaker::Other);
        assert!(shown(cx, window, ("segment", segment.id as u64)));

        // The toggle of a row corrects its speaker.
        click(cx, window, ("toggle-speaker", segment.id as u64));
        cx.wait_for(window, WAIT, |_, cx| {
            view.read(cx).segments[0].speaker == Speaker::Accused
        })
        .await;
        cx.run_until_parked();
        assert_eq!(
            opened.store.lock().unwrap().segments(hearing.id).unwrap()[0].speaker,
            Speaker::Accused
        );

        // The speaker button labels the next speech.
        click(cx, window, "speaker");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| opened.recording.read(cx).speaker()),
            Speaker::Accused
        );
        backend.send(0.5, 5);
        backend.send(0.0, 19);
        cx.wait_for(window, WAIT, |_, cx| view.read(cx).segments.len() == 2)
            .await;
        assert_eq!(
            cx.update(|cx| view.read(cx).segments[1].speaker),
            Speaker::Accused
        );

        // Finish: the microphone stops, and the hearing has an end and an audio file.
        click(cx, window, "finish");
        cx.wait_for(window, WAIT, |_, cx| {
            let view = view.read(cx);
            view.recording.read(cx).live().is_none() && view.hearings[0].ended_at.is_some()
        })
        .await;
        assert!(backend.input.lock().unwrap().is_none());
        assert!(!shown(cx, window, "live-strip"));
        let audio = cx.update(|cx| view.read(cx).hearings[0].audio_path.clone().unwrap());
        assert_eq!(
            hound::WavReader::open(audio).unwrap().duration(),
            (10 + 19 + 5 + 19) * 512
        );
    }

    #[gpui_kit::test]
    async fn pause_ends_the_segment_and_resume_records_again(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("Continúe.");
        let dirs = model_dirs("pause", true);
        let opened = open("pause", store, case, &backend, &dirs, cx);
        let (window, view) = (opened.window, opened.view.clone());
        start(cx, &opened, &backend, id).await;

        backend.send(0.5, 5);
        click(cx, window, "pause");
        cx.wait_for(window, WAIT, |window, cx| {
            view.read(cx).segments.len() == 1 && window.try_find("resume").is_some()
        })
        .await;
        assert!(backend.input.lock().unwrap().is_none());
        cx.update(|cx| {
            let recording = opened.recording.read(cx);
            assert_eq!(status_text(recording.live().unwrap()), "Grabación en pausa");
        });
        click(cx, window, "resume");

        assert!(backend.input.lock().unwrap().is_some());
        assert!(shown(cx, window, "pause"));
    }

    #[gpui_kit::test]
    async fn zero_input_shows_no_signal_and_real_input_shows_signal(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("");
        let dirs = model_dirs("health", true);
        let opened = open("health", store, case, &backend, &dirs, cx);
        start(cx, &opened, &backend, id).await;
        let status = |cx: &mut TestAppContext| {
            cx.update(|cx| status_text(opened.recording.read(cx).live().unwrap()))
        };
        assert_eq!(status(cx), "Grabando · Esperando la señal del micrófono");

        // No frame for 3 s.
        cx.executor().advance_clock(Duration::from_secs(4));
        cx.run_until_parked();
        assert_eq!(
            status(cx),
            "El micrófono no entrega señal. Revise la entrada seleccionada."
        );

        // Frames with only zeros: the microphone gives no signal. The clock goes on with the
        // frames.
        for _ in 0..100 {
            backend.send(0.0, 1);
            cx.executor().advance_clock(Duration::from_millis(32));
        }
        cx.run_until_parked();
        assert_eq!(
            status(cx),
            "El micrófono no entrega señal. Revise la entrada seleccionada."
        );

        backend.send(0.0001, 320);
        cx.run_until_parked();
        assert_eq!(status(cx), "La señal es baja. Acérquese al micrófono.");

        backend.send(0.3, 1);
        cx.run_until_parked();
        assert_eq!(status(cx), "Grabando · Señal recibida");
    }

    #[gpui_kit::test]
    async fn a_microphone_failure_leaves_only_finalizar(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("Sí.");
        let dirs = model_dirs("interrupted", true);
        let opened = open("interrupted", store, case, &backend, &dirs, cx);
        let window = opened.window;
        start(cx, &opened, &backend, id).await;
        backend.send(0.5, 5);

        backend
            .input
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .try_send(crate::audio::pipeline::Input::Failed)
            .unwrap();
        cx.wait_for(window, WAIT, |_, cx| {
            opened
                .recording
                .read(cx)
                .live()
                .is_some_and(|live| live.interrupted)
        })
        .await;

        cx.update(|cx| {
            let recording = opened.recording.read(cx);
            assert_eq!(
                status_text(recording.live().unwrap()),
                "La grabación se interrumpió. Revise el audio guardado."
            );
        });
        assert!(backend.input.lock().unwrap().is_none());
        assert!(shown(cx, window, "finish"));
        assert!(!shown(cx, window, "pause"));
        assert!(!shown(cx, window, "speaker"));
        // The speech before the failure is a segment.
        cx.wait_for(window, WAIT, |_, cx| {
            opened.view.read(cx).segments.len() == 1
        })
        .await;
    }

    #[gpui_kit::test]
    async fn without_models_start_shows_a_message_and_the_download_button(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("");
        let dirs = model_dirs("none", false);
        let opened = open("none", store, case, &backend, &dirs, cx);
        let window = opened.window;
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;

        click(cx, window, ("hearing", id as u64));
        wait_until_ready(cx, &opened).await;
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("download").is_some()
        })
        .await;
        click(cx, window, "start");

        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("hearing-message").is_some()
        })
        .await;
        cx.update(|cx| {
            assert_eq!(
                opened.view.read(cx).message,
                Some(AudioError::NoModel.message().into())
            );
        });
        assert!(backend.input.lock().unwrap().is_none());
    }

    #[gpui_kit::test]
    async fn the_selected_microphone_is_the_device_that_opens(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let mut backend = FakeBackend::new("");
        backend.microphones.push(Microphone {
            id: "usb".into(),
            name: "Micrófono USB".into(),
            is_default: false,
        });
        let dirs = model_dirs("mic", true);
        let opened = open("mic", store, case, &backend, &dirs, cx);
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, opened.window, ("hearing", id as u64));
        wait_until_ready(cx, &opened).await;

        cx.update_window(opened.window, |_, window, cx| {
            opened.view.update(cx, |view, cx| {
                view.mic_select.update(cx, |select, cx| {
                    select.set_selected_value(&"usb".to_owned(), window, cx)
                })
            })
        })
        .unwrap();
        click(cx, opened.window, "start");
        cx.wait_for(opened.window, WAIT, |_, _| {
            backend.input.lock().unwrap().is_some()
        })
        .await;

        assert_eq!(*backend.opened.lock().unwrap(), Some(Some("usb".into())));
        cx.update(|cx| {
            assert_eq!(
                opened.recording.read(cx).live().unwrap().device,
                "Micrófono USB"
            );
        });
    }

    #[gpui_kit::test]
    async fn a_selected_microphone_that_is_gone_stops_the_start(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        store.set_setting("microphone", "unplugged").unwrap();
        let backend = FakeBackend::new("");
        let dirs = model_dirs("mic-gone", true);
        let opened = open("mic-gone", store, case, &backend, &dirs, cx);
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, opened.window, ("hearing", id as u64));
        wait_until_ready(cx, &opened).await;

        click(cx, opened.window, "start");
        cx.wait_for(opened.window, WAIT, |_, cx| {
            opened.view.read(cx).message.is_some()
        })
        .await;

        cx.update(|cx| {
            assert_eq!(
                opened.view.read(cx).message,
                Some(AudioError::MicrophoneMissing.message().into())
            );
            assert!(opened.recording.read(cx).live().is_none());
        });
        assert_eq!(*backend.opened.lock().unwrap(), None);
        assert!(
            opened.store.lock().unwrap().hearings(case).unwrap()[0]
                .started_at
                .is_none()
        );
    }

    #[gpui_kit::test]
    async fn a_second_hearing_cannot_start_while_one_records(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let first = store.create_hearing(case, "Primera").unwrap().id;
        let second = store.create_hearing(case, "Segunda").unwrap().id;
        let backend = FakeBackend::new("Sí.");
        let dirs = model_dirs("busy", true);
        let opened = open("busy", store, case, &backend, &dirs, cx);
        start(cx, &opened, &backend, first).await;

        click(cx, opened.window, ("hearing", second as u64));
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find("start").is_some()
        })
        .await;
        cx.update(|cx| opened.view.update(cx, |view, cx| view.start(cx)));

        cx.wait_for(opened.window, WAIT, |_, cx| {
            opened.view.read(cx).message.is_some()
        })
        .await;
        cx.update(|cx| {
            assert_eq!(
                opened.view.read(cx).message,
                Some(AudioError::Busy.message().into())
            );
            assert_eq!(opened.recording.read(cx).live_hearing(), Some(first));
        });
    }

    #[gpui_kit::test]
    async fn the_keys_start_pause_and_switch_the_speaker(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("");
        let dirs = model_dirs("keys", true);
        let opened = open("keys", store, case, &backend, &dirs, cx);
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, opened.window, ("hearing", id as u64));
        wait_until_ready(cx, &opened).await;
        let press = |cx: &mut TestAppContext, key: &str| {
            cx.update_window(opened.window, |_, window, cx| window.press(key, cx))
                .unwrap();
            cx.run_until_parked();
        };

        press(cx, "cmd-shift-r");
        cx.wait_for(opened.window, WAIT, |_, _| {
            backend.input.lock().unwrap().is_some()
        })
        .await;
        press(cx, "cmd-shift-a");
        press(cx, "cmd-shift-p");

        cx.update(|cx| {
            let recording = opened.recording.read(cx);
            assert_eq!(recording.speaker(), Speaker::Accused);
            assert!(recording.live().unwrap().paused);
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            for (action, key) in [
                (&StartRecording as &dyn gpui_kit::Action, "cmd-shift-r"),
                (&TogglePause, "cmd-shift-p"),
                (&SwitchSpeaker, "cmd-shift-a"),
            ] {
                let keys: Vec<String> = keymap
                    .bindings_for_action(action)
                    .flat_map(|binding| binding.keystrokes().iter().map(|key| key.unparse()))
                    .collect();
                assert_eq!(keys, [key]);
            }
        });
    }

    #[gpui_kit::test]
    async fn delete_removes_the_hearing(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("");
        let dirs = model_dirs("delete", true);
        let opened = open("delete", store, case, &backend, &dirs, cx);
        let window = opened.window;
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, window, ("hearing", id as u64));
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("delete-hearing").is_some()
        })
        .await;

        click(cx, window, "delete-hearing");
        cx.wait_for(window, WAIT, |window, _| window.try_find("ok").is_some())
            .await;
        click(cx, window, "ok");

        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_none()
        })
        .await;
        cx.update(|cx| {
            let view = opened.view.read(cx);
            assert!(view.hearings.is_empty());
            assert_eq!(view.selected, None);
        });
    }

    const ANSWER: &str = "Ese día yo estaba en Medellín.";
    const CLAIM: &str = "El acusado estaba en Medellín el 12 de marzo.";
    const HOTEL: &str =
        "Según el registro del hotel, el acusado estaba en Bogotá el 12 de marzo de 2024.";

    fn nli() -> FakeNli {
        FakeNli {
            word: "Bogotá".into(),
            contradiction: 0.95,
        }
    }

    /// Writes fake files of the analysis models into the Hugging Face cache of `dirs`.
    fn install_analysis_models(dirs: &ModelDirs) {
        for model in ANALYSIS_MODELS {
            for file in model.files {
                let path = dirs
                    .hf_cache
                    .join(format!("models--{}", file.repo.replace('/', "--")))
                    .join("snapshots")
                    .join(file.revision)
                    .join(file.file);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, b"x").unwrap();
            }
        }
    }

    /// Adds the hotel register to the case. Returns the id of its chunk.
    fn add_evidence(store: &Store, case: i64, name: &str) -> i64 {
        let path = temp_dir(&format!("evidence-{name}")).join("registro.txt");
        fs::write(&path, "x").unwrap();
        let id = store.import(case, &path, |_, _| {}).unwrap().id;
        let page = Page {
            number: 3,
            text: HOTEL.into(),
            source: PageSource::Text,
        };
        store.save_analysis(id, &[page]).unwrap();
        store.search(case, "hotel", 1).unwrap()[0].chunk_id
    }

    /// A hearing with one answer of the accused. Returns the hearing and the segment ids.
    fn add_hearing(store: &Store, case: i64) -> (i64, i64) {
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let segment = store
            .insert_segment(hearing, 1_000, Speaker::Accused)
            .unwrap();
        store.finish_segment(segment, ANSWER, 3_000).unwrap();
        (hearing, segment)
    }

    fn new_suggestion(
        hearing: i64,
        segment: i64,
        chunk: i64,
        level: SuggestionLevel,
    ) -> NewSuggestion {
        NewSuggestion {
            hearing_id: hearing,
            segment_id: segment,
            chunk_id: chunk,
            claim: CLAIM.into(),
            claim_quote: Some("en Medellín".into()),
            evidence_quote: "estaba en Bogotá el 12 de marzo de 2024".into(),
            kind: None,
            level,
            nli_contradiction: 0.95,
            explanation: Some("El registro ubica al acusado en Bogotá.".into()),
        }
    }

    async fn select_hearing(cx: &mut TestAppContext, window: AnyWindowHandle, hearing: i64) {
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hearing", hearing as u64)).is_some()
        })
        .await;
        click(cx, window, ("hearing", hearing as u64));
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("suggestions-panel").is_some()
        })
        .await;
    }

    #[gpui_kit::test]
    async fn a_suggestion_event_shows_a_card_and_updates_the_counter(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let chunk = add_evidence(&store, case, "event");
        let (hearing, segment) = add_hearing(&store, case);
        let dirs = model_dirs("event", true);
        let (window, view) = open_pair("event", store, case, &FakeBackend::new(""), &dirs, cx);
        select_hearing(cx, window, hearing).await;
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("suggestions-empty").is_some()
        })
        .await;

        let suggestion = cx.update(|cx| {
            view.read(cx)
                .store
                .lock()
                .unwrap()
                .insert_suggestion(&new_suggestion(
                    hearing,
                    segment,
                    chunk,
                    SuggestionLevel::Conflict,
                ))
                .unwrap()
        });
        let id = suggestion.id as u64;
        view.update(cx, |view, cx| {
            view.apply_review(ReviewEvent::SuggestionAdded(suggestion), cx)
        });

        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("suggestion", id)).is_some()
        })
        .await;
        cx.update(|cx| {
            assert_eq!(
                view.read(cx).counts,
                SuggestionCounts {
                    conflict: 1,
                    check: 0
                }
            );
        });
        assert!(shown(cx, window, ("confirm", id)));
        assert!(shown(cx, window, ("source", id)));
    }

    #[gpui_kit::test]
    async fn confirm_changes_the_status_and_the_counter(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let chunk = add_evidence(&store, case, "confirm");
        let (hearing, segment) = add_hearing(&store, case);
        let id = store
            .insert_suggestion(&new_suggestion(
                hearing,
                segment,
                chunk,
                SuggestionLevel::Check,
            ))
            .unwrap()
            .id;
        let dirs = model_dirs("confirm", true);
        let (window, view) = open_pair("confirm", store, case, &FakeBackend::new(""), &dirs, cx);
        select_hearing(cx, window, hearing).await;
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("confirm", id as u64)).is_some()
        })
        .await;
        cx.update(|cx| assert_eq!(view.read(cx).counts.check, 1));

        click(cx, window, ("confirm", id as u64));

        cx.wait_for(window, WAIT, |_, cx| view.read(cx).counts.check == 0)
            .await;
        cx.update(|cx| {
            let view = view.read(cx);
            assert_eq!(view.suggestions[0].status, SuggestionStatus::Confirmed);
            assert_eq!(
                view.store.lock().unwrap().suggestions(hearing).unwrap()[0].status,
                SuggestionStatus::Confirmed
            );
        });
        // The card stays (the filter shows all suggestions), without the buttons.
        assert!(shown(cx, window, ("suggestion", id as u64)));
        assert!(!shown(cx, window, ("confirm", id as u64)));
        assert!(!shown(cx, window, ("dismiss", id as u64)));
    }

    #[gpui_kit::test]
    async fn the_filter_shows_the_suggestions_with_the_selected_status(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let chunk = add_evidence(&store, case, "filter");
        let (hearing, segment) = add_hearing(&store, case);
        let add = |level| {
            store
                .insert_suggestion(&new_suggestion(hearing, segment, chunk, level))
                .unwrap()
                .id as u64
        };
        let open_id = add(SuggestionLevel::Conflict);
        let dismissed = add(SuggestionLevel::Check);
        store
            .set_suggestion_status(dismissed as i64, SuggestionStatus::Dismissed)
            .unwrap();
        let dirs = model_dirs("filter", true);
        let (window, view) = open_pair("filter", store, case, &FakeBackend::new(""), &dirs, cx);
        select_hearing(cx, window, hearing).await;
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("suggestion", open_id)).is_some()
                && window.try_find(("suggestion", dismissed)).is_some()
        })
        .await;

        for (filter, open_shown, dismissed_shown) in [
            (Filter::Open, true, false),
            (Filter::Dismissed, false, true),
            (Filter::Confirmed, false, false),
            (Filter::All, true, true),
        ] {
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.filter_select.update(cx, |select, cx| {
                        select.set_selected_value(&filter, window, cx)
                    })
                })
            })
            .unwrap();
            cx.run_until_parked();

            assert_eq!(
                shown(cx, window, ("suggestion", open_id)),
                open_shown,
                "{filter:?}"
            );
            assert_eq!(
                shown(cx, window, ("suggestion", dismissed)),
                dismissed_shown,
                "{filter:?}"
            );
        }
    }

    #[gpui_kit::test]
    async fn without_analysis_models_the_panel_offers_a_download_for_each(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let (hearing, _) = add_hearing(&store, case);
        let dirs = model_dirs("no-analysis", true);
        let (window, view) =
            open_pair("no-analysis", store, case, &FakeBackend::new(""), &dirs, cx);
        select_hearing(cx, window, hearing).await;

        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("download-nli").is_some() && window.try_find("download-llm").is_some()
        })
        .await;
        cx.update(|cx| assert!(!view.read(cx).analysis_on()));
    }

    #[gpui_kit::test]
    async fn an_accused_answer_in_a_live_hearing_becomes_a_suggestion(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        add_evidence(&store, case, "live");
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let backend = FakeBackend::new(ANSWER);
        let explanation = serde_json::json!({
            "contradiction": true,
            "kind": "location",
            "explanation_es": "El registro ubica al acusado en Bogotá.",
            "quote_claim": "en Medellín",
            "quote_evidence": "estaba en Bogotá",
        });
        let llm = FakeLlm::new(
            &serde_json::json!([CLAIM]).to_string(),
            &explanation.to_string(),
        );
        let review = FakeReviewBackend::new(llm.clone(), nli());
        let dirs = model_dirs("live-analysis", true);
        install_analysis_models(&dirs);
        let (window, view) = open_with("live", store, case, &backend, &review, &dirs, cx);
        select_hearing(cx, window, hearing).await;
        cx.wait_for(window, WAIT, |_, cx| view.read(cx).ready())
            .await;
        cx.wait_for(window, WAIT, |_, cx| view.read(cx).analysis_on())
            .await;

        let recording = cx.update(|cx| view.read(cx).recording.clone());
        recording.update(cx, |recording, cx| recording.switch_speaker(cx));
        click(cx, window, "start");
        cx.wait_for(window, WAIT, |_, _| backend.input.lock().unwrap().is_some())
            .await;
        backend.send(0.5, 5);
        backend.send(0.0, 19);

        cx.wait_for(window, WAIT, |_, cx| view.read(cx).suggestions.len() == 1)
            .await;
        let suggestion = cx.update(|cx| view.read(cx).suggestions[0].clone());
        assert_eq!(suggestion.level, SuggestionLevel::Conflict);
        assert_eq!(suggestion.claim, CLAIM);
        assert_eq!(suggestion.page, 3);
        assert_eq!(llm.calls()[0], format!("claims||{ANSWER}"));
        let loaded = review.loaded.lock().unwrap().clone().unwrap();
        assert_eq!((loaded.0.len(), loaded.1.len()), (3, 2));
    }

    /// Opens a view with installed analysis models, selects a new hearing, and starts the
    /// recording. Returns the window, the view, and the hearing.
    async fn record_with_analysis(
        name: &str,
        review: &FakeReviewBackend,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<HearingsView>, i64) {
        let (store, case) = store_with_case();
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let backend = FakeBackend::new(ANSWER);
        let dirs = model_dirs(name, true);
        install_analysis_models(&dirs);
        let (window, view) = open_with(name, store, case, &backend, review, &dirs, cx);
        select_hearing(cx, window, hearing).await;
        cx.wait_for(window, WAIT, |_, cx| view.read(cx).ready())
            .await;
        cx.wait_for(window, WAIT, |_, cx| view.read(cx).analysis_on())
            .await;
        click(cx, window, "start");
        cx.wait_for(window, WAIT, |_, _| backend.input.lock().unwrap().is_some())
            .await;
        (window, view, hearing)
    }

    #[gpui_kit::test]
    async fn analysis_models_that_do_not_load_show_the_downloads_again(cx: &mut TestAppContext) {
        let mut review = FakeReviewBackend::new(FakeLlm::new("[]", "{}"), nli());
        review.fail = true;

        let (window, view, _) = record_with_analysis("analysis-broken", &review, cx).await;

        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("download-nli").is_some() && window.try_find("download-llm").is_some()
        })
        .await;
        cx.update(|cx| {
            let view = view.read(cx);
            assert!(!view.analysis_on());
            assert_eq!(view.message, Some(ReviewError::ModelLoad.message().into()));
        });
    }

    #[gpui_kit::test]
    async fn the_switch_off_cancels_the_running_analysis(cx: &mut TestAppContext) {
        let review = FakeReviewBackend::new(FakeLlm::new("[]", "{}"), nli());
        let (window, view, _) = record_with_analysis("analysis-cancel", &review, cx).await;
        cx.wait_for(window, WAIT, |_, cx| {
            view.read(cx)
                .review
                .as_ref()
                .is_some_and(|review| review.task.is_some())
        })
        .await;
        let cancel = cx.update(|cx| view.read(cx).review.as_ref().unwrap().cancel.clone());

        click(cx, window, "live-analysis");
        cx.run_until_parked();

        assert!(cancel.load(Ordering::Relaxed));
        cx.update(|cx| assert!(view.read(cx).review.is_none()));
    }

    #[gpui_kit::test]
    async fn a_hearing_cannot_be_deleted_while_it_records(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("");
        let dirs = model_dirs("delete-live", true);
        let opened = open("delete-live", store, case, &backend, &dirs, cx);
        start(cx, &opened, &backend, id).await;

        cx.update_window(opened.window, |_, window, cx| {
            opened
                .view
                .update(cx, |view, cx| view.delete(id, window, cx));
        })
        .unwrap();
        cx.run_until_parked();

        assert_eq!(
            opened.store.lock().unwrap().hearings(case).unwrap().len(),
            1
        );
    }

    #[gpui_kit::test]
    async fn an_empty_or_blank_hearing_title_shows_a_field_error(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let backend = FakeBackend::new("");
        let dirs = model_dirs("title", true);
        let opened = open("title", store, case, &backend, &dirs, cx);
        let window = opened.window;
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("new-hearing").is_some()
        })
        .await;
        click(cx, window, "new-hearing");
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("hearing-title").is_some()
        })
        .await;
        cx.run_until_parked();

        cx.update_window(window, |_, window, cx| {
            window.click("hearing-title", cx);
            window.press("cmd-a", cx);
            window.press("backspace", cx);
            window.input("   ", cx);
        })
        .unwrap();
        cx.run_until_parked();
        click(cx, window, "create-hearing");
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("hearing-title-error").is_some()
        })
        .await;

        assert!(shown(cx, window, "hearing-title"));
        assert!(
            opened
                .store
                .lock()
                .unwrap()
                .hearings(case)
                .unwrap()
                .is_empty()
        );
    }

    #[gpui_kit::test]
    async fn downloads_do_not_run_while_a_hearing_records(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("");
        let dirs = model_dirs("download-live", true);
        let opened = open("download-live", store, case, &backend, &dirs, cx);
        start(cx, &opened, &backend, id).await;

        cx.update_window(opened.window, |_, window, cx| {
            opened.view.update(cx, |view, cx| {
                view.download_models(vec![&COHERE], window, cx)
            });
        })
        .unwrap();

        cx.update(|cx| {
            let view = opened.view.read(cx);
            assert_eq!(
                view.message.as_deref(),
                Some("Finalice la audiencia para descargar modelos.")
            );
            assert!(view.download.is_none());
        });
    }

    #[gpui_kit::test]
    async fn a_failed_segment_shows_as_a_marker(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("");
        let dirs = model_dirs("marker", true);
        let opened = open("marker", store, case, &backend, &dirs, cx);
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, opened.window, ("hearing", id as u64));
        cx.run_until_parked();
        let segment = Segment {
            id: 5,
            t_start_ms: 0,
            t_end_ms: 2_000,
            text: String::new(),
            speaker: Speaker::Accused,
            is_final: true,
            revision: 0,
            failed: true,
        };

        opened.view.update(cx, |view, cx| {
            view.apply(id, case, Event::Untranscribed(segment.clone()), cx)
        });

        assert!(shown(cx, opened.window, ("segment", 5u64)));
        assert!(!shown(cx, opened.window, ("toggle-speaker", 5u64)));
    }

    #[gpui_kit::test]
    async fn a_final_segment_that_is_already_shown_is_not_shown_twice(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("");
        let dirs = model_dirs("final-twice", true);
        let opened = open("final-twice", store, case, &backend, &dirs, cx);
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, opened.window, ("hearing", id as u64));
        cx.run_until_parked();
        let segment = Segment {
            id: 7,
            t_start_ms: 0,
            t_end_ms: 320,
            text: "Sí.".into(),
            speaker: Speaker::Other,
            is_final: true,
            revision: 0,
            failed: false,
        };

        opened.view.update(cx, |view, cx| {
            view.segments = vec![segment.clone()];
            view.apply(id, case, Event::Final(segment.clone()), cx);
        });

        cx.update(|cx| assert_eq!(opened.view.read(cx).segments, [segment]));
    }

    fn segment(id: i64) -> Segment {
        Segment {
            id,
            t_start_ms: id * 1_000,
            t_end_ms: id * 1_000 + 900,
            text: format!("Declaración número {id}."),
            speaker: Speaker::Other,
            is_final: true,
            revision: 0,
            failed: false,
        }
    }

    #[gpui_kit::test]
    async fn new_text_does_not_move_a_reader_away_from_earlier_text(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("");
        let dirs = model_dirs("follow", true);
        let opened = open("follow", store, case, &backend, &dirs, cx);
        let window = opened.window;
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, window, ("hearing", id as u64));
        cx.run_until_parked();
        opened.view.update(cx, |view, cx| {
            view.segments = (1..=1_000).map(segment).collect();
            view.transcript_scroll.scroll_to_bottom();
            cx.notify();
        });
        let started = Instant::now();
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        let render = started.elapsed();

        // At the end, new text scrolls into view.
        opened.view.update(cx, |view, cx| {
            view.apply(id, case, Event::Final(segment(1_001)), cx)
        });
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        assert!(!shown(cx, window, "follow-live"));

        // The user reads earlier text: new text does not move it.
        cx.update_window(window, |_, window, cx| {
            window.scroll(
                "transcript",
                ScrollDelta::Pixels(point(px(0.), px(2_000.))),
                cx,
            )
        })
        .unwrap();
        let offset = cx.update(|cx| opened.view.read(cx).transcript_scroll.offset());
        opened.view.update(cx, |view, cx| {
            view.apply(id, case, Event::Final(segment(1_002)), cx)
        });
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();

        assert_eq!(
            cx.update(|cx| opened.view.read(cx).transcript_scroll.offset()),
            offset
        );
        assert!(shown(cx, window, "follow-live"));
        click(cx, window, "follow-live");
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        assert!(!shown(cx, window, "follow-live"));
        assert!(
            render < Duration::from_secs(5),
            "1,000 rows took {render:?}"
        );
    }

    /// No two controls overlap, and all of them are inside the window.
    fn assert_apart(cx: &mut TestAppContext, window: AnyWindowHandle, ids: &[&'static str]) {
        let viewport = cx
            .update_window(window, |_, window, _| window.viewport_size())
            .unwrap();
        let boxes: Vec<(&str, Bounds<Pixels>)> = ids
            .iter()
            .map(|id| (*id, bounds(cx, window, *id)))
            .collect();
        for (id, bounds) in &boxes {
            assert!(
                bounds.left() >= px(0.)
                    && bounds.top() >= px(0.)
                    && bounds.right() <= viewport.width
                    && bounds.bottom() <= viewport.height,
                "{id} is outside the window: {bounds:?}"
            );
        }
        for (i, (a, first)) in boxes.iter().enumerate() {
            for (b, second) in &boxes[i + 1..] {
                assert!(
                    !first.intersects(second),
                    "{a} {first:?} overlaps {b} {second:?}"
                );
            }
        }
    }

    async fn check_layout(name: &str, width: f32, height: f32, cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let setup = store
            .create_hearing(case, "Audiencia por iniciar")
            .unwrap()
            .id;
        let id = store
            .create_hearing(
                case,
                "Audiencia de juicio oral contra el acusado con un título muy largo",
            )
            .unwrap()
            .id;
        let backend = FakeBackend::new("");
        let dirs = model_dirs(name, true);
        let opened = open_sized(
            name,
            store,
            case,
            &backend,
            &dirs,
            size(px(width), px(height)),
            cx,
        );
        let narrow = width < 1000.;

        // Before the start: the setup row.
        cx.wait_for(opened.window, WAIT, |_, cx| opened.view.read(cx).loaded)
            .await;
        cx.update_window(opened.window, |_, window, cx| {
            opened
                .view
                .update(cx, |view, cx| view.select(setup, window, cx))
        })
        .unwrap();
        wait_until_ready(cx, &opened).await;
        let mut setup_ids = vec!["model-select-box", "mic-select-box", "start"];
        if narrow {
            setup_ids.push("hearings-drawer");
        } else {
            setup_ids.push("hearing-pane");
        }
        assert_apart(cx, opened.window, &setup_ids);
        assert_eq!(
            bounds(cx, opened.window, "model-select-box").size,
            size(px(240.), px(36.))
        );

        // During the recording, with a long error.
        cx.update_window(opened.window, |_, window, cx| {
            opened
                .view
                .update(cx, |view, cx| view.select(id, window, cx))
        })
        .unwrap();
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find("start").is_some()
        })
        .await;
        click(cx, opened.window, "start");
        cx.wait_for(opened.window, WAIT, |_, _| {
            backend.input.lock().unwrap().is_some()
        })
        .await;
        opened.view.update(cx, |view, cx| {
            view.message = Some(SAVE_ERROR.repeat(12).into());
            view.segments = (1..=40).map(segment).collect();
            cx.notify();
        });
        let mut live_ids = vec![
            "live-title",
            "live-health",
            "live-device",
            "speaker",
            "pause",
            "finish",
        ];
        if narrow {
            live_ids.push("hearings-drawer");
        }
        assert_apart(cx, opened.window, &live_ids);
        let strip = bounds(cx, opened.window, "live-strip");
        assert_eq!(strip.size.height, px(if narrow { 80. } else { 48. }));
        assert!(bounds(cx, opened.window, "speaker").size.width >= px(176.));
        let transcript = bounds(cx, opened.window, "transcript-frame");
        // The suggestions panel has its own narrow layout: this check is for the transcript
        // without it.
        let panel = shown(cx, opened.window, "suggestions-panel");
        let reading = if narrow { 600. } else { 664. };
        assert!(
            (narrow && panel) || transcript.size.width >= px(reading),
            "the transcript is {transcript:?}"
        );
        assert!(transcript.size.height > px(0.));
        let message = bounds(cx, opened.window, "hearing-message");
        assert!(message.bottom() <= transcript.top());
    }

    #[gpui_kit::test]
    async fn controls_do_not_overlap_at_1280_by_820(cx: &mut TestAppContext) {
        check_layout("layout-wide", 1280., 820., cx).await;
    }

    #[gpui_kit::test]
    async fn controls_do_not_overlap_at_900_by_600(cx: &mut TestAppContext) {
        check_layout("layout-narrow", 900., 600., cx).await;
    }

    #[gpui_kit::test]
    async fn the_hearing_list_is_a_drawer_in_a_narrow_window(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("");
        let dirs = model_dirs("drawer", true);
        let opened = open_sized(
            "drawer",
            store,
            case,
            &backend,
            &dirs,
            size(px(900.), px(600.)),
            cx,
        );
        cx.wait_for(opened.window, WAIT, |_, cx| opened.view.read(cx).loaded)
            .await;
        assert!(!shown(cx, opened.window, "hearing-pane"));

        click(cx, opened.window, "hearings-drawer");
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, opened.window, ("hearing", id as u64));
        cx.run_until_parked();

        assert_eq!(cx.update(|cx| opened.view.read(cx).selected), Some(id));
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_none()
        })
        .await;
    }

    #[gpui_kit::test]
    async fn quit_after_finish_waits_for_the_end_of_the_hearing(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("Fin.");
        let dirs = model_dirs("quit-finish", true);
        let opened = open("quit-finish", store, case, &backend, &dirs, cx);
        start(cx, &opened, &backend, id).await;
        backend.send(0.5, 5);

        click(cx, opened.window, "finish");
        let result = opened
            .recording
            .update(cx, |recording, cx| recording.stop_all(cx));

        assert_eq!(result, Ok(()));
        let store = opened.store.lock().unwrap();
        assert!(store.hearings(case).unwrap()[0].ended_at.is_some());
        assert_eq!(store.segments(id).unwrap()[0].text, "Fin.");
    }

    #[gpui_kit::test]
    async fn closing_the_window_and_quitting_saves_the_live_recording(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("Última frase.");
        let dirs = model_dirs("quit-close", true);
        let opened = open("quit-close", store, case, &backend, &dirs, cx);
        start(cx, &opened, &backend, id).await;
        backend.send(0.5, 5);

        // The window closes first, as on the close button; the view goes away with it.
        cx.update_window(opened.window, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        cx.quit();

        assert!(backend.input.lock().unwrap().is_none());
        let store = opened.store.lock().unwrap();
        let hearing = &store.hearings(case).unwrap()[0];
        assert!(hearing.ended_at.is_some());
        assert_eq!(store.segments(id).unwrap()[0].text, "Última frase.");
        let audio = hearing.audio_path.clone().unwrap();
        assert_eq!(hound::WavReader::open(audio).unwrap().duration(), 5 * 512);
    }

    #[gpui_kit::test]
    async fn quit_while_the_models_load_records_nothing(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("");
        let dirs = model_dirs("quit-start", true);
        let opened = open("quit-start", store, case, &backend, &dirs, cx);
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, opened.window, ("hearing", id as u64));
        wait_until_ready(cx, &opened).await;

        // The start work is pending when the quit comes.
        cx.update(|cx| opened.view.update(cx, |view, cx| view.start(cx)));
        assert!(cx.update(|cx| opened.recording.read(cx).starting().is_some()));
        let result = opened
            .recording
            .update(cx, |recording, cx| recording.stop_all(cx));

        assert_eq!(result, Ok(()));
        assert!(backend.input.lock().unwrap().is_none());
        assert!(cx.update(|cx| opened.recording.read(cx).live().is_none()));
        let store = opened.store.lock().unwrap();
        assert!(store.hearings(case).unwrap()[0].started_at.is_none());
    }

    #[gpui_kit::test]
    async fn a_failed_end_of_the_hearing_is_reported(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("Sí.");
        let dirs = model_dirs("end-error", true);
        let opened = open("end-error", store, case, &backend, &dirs, cx);
        start(cx, &opened, &backend, id).await;
        backend.send(0.5, 5);
        opened
            .store
            .lock()
            .unwrap()
            .execute_test_sql(
                "CREATE TEMP TRIGGER no_end BEFORE UPDATE OF ended_at ON hearings
                 BEGIN SELECT RAISE(ABORT, 'test'); END;",
            )
            .unwrap();

        click(cx, opened.window, "finish");
        cx.wait_for(opened.window, WAIT, |_, cx| {
            opened.view.read(cx).message.is_some()
        })
        .await;
        cx.update(|cx| {
            assert_eq!(opened.view.read(cx).message.as_deref(), Some(SAVE_ERROR));
        });

        // The audio file is complete.
        let audio = opened.store.lock().unwrap().hearings(case).unwrap()[0]
            .audio_path
            .clone()
            .unwrap();
        assert_eq!(hound::WavReader::open(audio).unwrap().duration(), 5 * 512);
    }

    #[gpui_kit::test]
    async fn quit_reports_a_failed_end_of_a_live_hearing(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let backend = FakeBackend::new("Sí.");
        let dirs = model_dirs("quit-end-error", true);
        let opened = open("quit-end-error", store, case, &backend, &dirs, cx);
        start(cx, &opened, &backend, id).await;
        opened
            .store
            .lock()
            .unwrap()
            .execute_test_sql(
                "CREATE TEMP TRIGGER no_end BEFORE UPDATE OF ended_at ON hearings
                 BEGIN SELECT RAISE(ABORT, 'test'); END;",
            )
            .unwrap();

        let result = opened
            .recording
            .update(cx, |recording, cx| recording.stop_all(cx));
        let again = opened
            .recording
            .update(cx, |recording, cx| recording.stop_all(cx));

        assert_eq!(result, Err(AudioError::Storage));
        assert_eq!(again, Ok(()));
        assert!(backend.input.lock().unwrap().is_none());
    }

    #[gpui_kit::test]
    async fn refine_after_the_hearing_keeps_the_live_text_and_unloads(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let mut backend = FakeBackend::new("yo no estaba alli");
        backend.refined = "Yo no estaba allí.".into();
        let dirs = model_dirs("refine", true);
        write_fake_model(&dirs.handy.join(COHERE.file), &COHERE);
        let opened = open("refine", store, case, &backend, &dirs, cx);
        start(cx, &opened, &backend, id).await;
        assert_eq!(*backend.refiner.lock().unwrap(), None);
        backend.send(0.5, 5);
        backend.send(0.0, 19);
        click(cx, opened.window, "finish");
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find("refine").is_some()
        })
        .await;

        click(cx, opened.window, "refine");
        cx.wait_for(opened.window, WAIT, |_, cx| {
            opened.recording.read(cx).refining().is_none()
                && opened
                    .view
                    .read(cx)
                    .segments
                    .first()
                    .is_some_and(|segment| segment.text == "Yo no estaba allí.")
        })
        .await;

        assert_eq!(
            *backend.refiner.lock().unwrap(),
            Some(dirs.handy.join(COHERE.file))
        );
        assert!(!backend.refiner_loaded.load(Ordering::SeqCst));
        let store = opened.store.lock().unwrap();
        let segment = &store.segments(id).unwrap()[0];
        assert_eq!(segment.revision, 1);
        assert_eq!(
            store.live_text(segment.id).unwrap().as_deref(),
            Some("yo no estaba alli")
        );
    }

    #[gpui_kit::test]
    async fn quit_during_a_refine_stops_it_and_keeps_the_end(cx: &mut TestAppContext) {
        let (store, case, id) = hearing_with_case("Audiencia");
        let mut backend = FakeBackend::new("uno");
        backend.refined = "Uno.".into();
        let dirs = model_dirs("quit-refine", true);
        write_fake_model(&dirs.handy.join(COHERE.file), &COHERE);
        let opened = open("quit-refine", store, case, &backend, &dirs, cx);
        start(cx, &opened, &backend, id).await;
        for _ in 0..3 {
            backend.send(0.5, 5);
            backend.send(0.0, 19);
        }
        click(cx, opened.window, "finish");
        cx.wait_for(opened.window, WAIT, |window, _| {
            window.try_find("refine").is_some()
        })
        .await;

        // The refine pass is pending when the quit comes.
        cx.update(|cx| opened.view.update(cx, |view, cx| view.refine(cx)));
        let result = opened
            .recording
            .update(cx, |recording, cx| recording.stop_all(cx));

        assert_eq!(result, Ok(()));
        assert!(!backend.refiner_loaded.load(Ordering::SeqCst));
        let store = opened.store.lock().unwrap();
        assert!(store.hearings(case).unwrap()[0].ended_at.is_some());
        // The pass stopped before the first segment: the live text stays.
        assert!(
            store
                .segments(id)
                .unwrap()
                .iter()
                .all(|segment| segment.text == "uno")
        );
    }
}
