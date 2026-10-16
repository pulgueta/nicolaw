//! The hearings screen: the hearings of a case, live recording and transcription, and the
//! transcript with the speaker of each segment.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectItem, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::audio::pipeline::{Event, Input as AudioInput, Processor, refine};
use crate::audio::{AudioError, Backend, Capture, Microphone, ModelPaths};
use crate::models::{self, COHERE, CatalogModel, ModelDirs, ModelStatus, NEMOTRON, SILERO_VAD};
use crate::store::{Hearing, Segment, Speaker, Store};

const LIST_ERROR: &str = "Nicolaw no puede leer las audiencias.";
const CREATE_ERROR: &str = "Nicolaw no puede crear la audiencia. Intente de nuevo.";
const DELETE_ERROR: &str = "Nicolaw no puede eliminar la audiencia. Intente de nuevo.";
const SPEAKER_ERROR: &str = "Nicolaw no puede cambiar el hablante. Intente de nuevo.";
/// At quit, Nicolaw waits this long for the last segment and the audio file of a recording.
const QUIT_TIMEOUT: Duration = Duration::from_secs(10);

const DOWNLOAD_ERROR: &str =
    "La descarga falló. Revise la conexión a internet y seleccione Descargar.";

/// What the hearings screen needs from the app.
#[derive(Clone)]
pub struct HearingsEnv {
    pub backend: Arc<dyn Backend>,
    pub model_dirs: ModelDirs,
    /// `<data_dir>/nicolaw`. The audio of each hearing goes to `hearings/<id>.wav` in it.
    pub data_dir: PathBuf,
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

/// The text of the open segment.
struct PartialRow {
    segment_id: i64,
    speaker: Speaker,
    t_start_ms: i64,
    committed: String,
    tentative: String,
}

/// The hearing that records now.
struct Live {
    hearing_id: i64,
    capture: Box<dyn Capture>,
    input: async_channel::Sender<AudioInput>,
    paused: bool,
    /// The processor task. It ends after the input closes.
    task: Task<()>,
    /// The refine task. It ends after the processor task.
    refine_task: Option<Task<()>>,
}

pub struct HearingsView {
    store: Arc<Mutex<Store>>,
    env: HearingsEnv,
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
    /// The download progress in percent, while a download runs.
    download: Option<f32>,
    /// True while the models load and the microphone opens.
    starting: bool,
    live: Option<Live>,
    /// The recordings that Finalizar ended: each task ends after the last segment, the end of
    /// the hearing, and the refine pass. The quit hook waits for them.
    finishing: Vec<Task<()>>,
    /// The mic level, from 0 to 1.
    level: f32,
    /// True while the switch "the accused speaks" is on. The processor reads it at the start of
    /// each segment.
    accused: Arc<AtomicBool>,
    /// The message in the alert above the transcript.
    message: Option<SharedString>,
    transcript_scroll: ScrollHandle,
    _quit: Subscription,
}

impl HearingsView {
    pub fn new(
        store: Arc<Mutex<Store>>,
        env: HearingsEnv,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let model_select = cx.new(|cx| SelectState::new(Vec::<ModelItem>::new(), None, window, cx));
        let mic_select = cx.new(|cx| SelectState::new(Vec::<MicItem>::new(), None, window, cx));
        let quit = cx.on_app_quit(|this, cx| {
            this.stop_at_quit(cx);
            async {}
        });

        let view = Self {
            store,
            env,
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
            download: None,
            starting: false,
            live: None,
            finishing: Vec::new(),
            level: 0.0,
            accused: Arc::new(AtomicBool::new(false)),
            message: None,
            transcript_scroll: ScrollHandle::new(),
            _quit: quit,
        };
        view.load_models(window, cx);
        view.load_microphones(window, cx);
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

    fn clear_selection(&mut self) {
        self.selected = None;
        self.segments.clear();
        self.partial = None;
        self.message = None;
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

    fn load_microphones(&self, window: &mut Window, cx: &mut Context<Self>) {
        let backend = self.env.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let microphones = cx
                .background_spawn(async move { backend.microphones().unwrap_or_default() })
                .await;
            this.update_in(cx, |this, window, cx| {
                let items = microphones
                    .iter()
                    .map(|microphone| MicItem {
                        id: microphone.id.clone(),
                        name: microphone.name.clone().into(),
                    })
                    .collect();
                let default = microphones
                    .iter()
                    .find(|microphone| microphone.is_default)
                    .or(microphones.first())
                    .map(|microphone| microphone.id.clone());
                this.mic_select.update(cx, |select, cx| {
                    select.set_items(items, window, cx);
                    if let Some(id) = default {
                        select.set_selected_value(&id, window, cx);
                    }
                });
                this.microphones = Some(microphones);
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        cx.notify();
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
        // Returns false if the title is empty, so that the dialog stays open.
        let submit = Rc::new({
            let input = input.clone();
            move |window: &mut Window, cx: &mut App| {
                let title = input.read(cx).value().trim().to_owned();
                if title.is_empty() {
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
            dialog
                .title("Nueva audiencia")
                .w(px(420.))
                .child(
                    div()
                        .id("hearing-title")
                        .child(Input::new(&input))
                        .test_support(),
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

    /// The installed model files for a recording with the selected model, or `None` if a
    /// required model is missing. The refine pass uses Cohere Transcribe when it is installed
    /// and it is not the live model.
    fn model_paths(&self, cx: &App) -> Option<ModelPaths> {
        let models = self.models.as_ref()?;
        let path = |model: &CatalogModel| {
            models
                .iter()
                .find(|status| status.model.key == model.key)
                .and_then(|status| status.path.clone())
        };
        let key = *self.model_select.read(cx).selected_value()?;
        let live = models
            .iter()
            .find(|status| status.model.key == key)
            .and_then(|status| status.path.clone())?;
        Some(ModelPaths {
            live,
            vad: path(&SILERO_VAD)?,
            refine: (key != COHERE.key).then(|| path(&COHERE)).flatten(),
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

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hearing_id) = self.selected else {
            return;
        };
        if self.live.is_some() || self.starting {
            return self.show_error(AudioError::Busy, cx);
        }
        let Some(paths) = self.model_paths(cx) else {
            return self.show_error(AudioError::NoModel, cx);
        };
        if self.microphones.as_ref().is_some_and(Vec::is_empty) {
            return self.show_error(AudioError::NoMicrophone, cx);
        }
        let device = self.mic_select.read(cx).selected_value().cloned();
        self.message = None;
        self.starting = true;
        cx.notify();

        let backend = self.env.backend.clone();
        let store = self.store.clone();
        let accused = self.accused.clone();
        let wav = self
            .env
            .data_dir
            .join("hearings")
            .join(format!("{hearing_id}.wav"));
        let (input, input_receiver) = async_channel::unbounded();
        cx.spawn_in(window, async move |this, cx| {
            let opened = cx
                .background_spawn({
                    let input = input.clone();
                    async move {
                        let engines = backend.load(&paths)?;
                        let mut capture = backend.open_microphone(device.as_deref(), input)?;
                        let (refine_sender, refine_receiver) = async_channel::unbounded();
                        let refiner = engines.refiner.map(|refiner| (refiner, refine_receiver));
                        let processor = Processor::new(
                            hearing_id,
                            &wav,
                            store.clone(),
                            accused,
                            engines.vad,
                            engines.transcriber,
                            refiner.is_some().then_some(refine_sender),
                        )?;
                        capture.start()?;
                        store
                            .lock()
                            .unwrap()
                            .start_hearing(hearing_id, &wav)
                            .map_err(|_| AudioError::Storage)?;
                        Ok::<_, AudioError>((capture, processor, refiner))
                    }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.starting = false;
                match opened {
                    Ok((capture, processor, refiner)) => {
                        let (events, event_receiver) = async_channel::unbounded();
                        let task = cx.background_spawn({
                            let events = events.clone();
                            async move {
                                let mut processor = processor;
                                while let Ok(input) = input_receiver.recv().await {
                                    for event in processor.push(input) {
                                        events.send(event).await.ok();
                                    }
                                }
                                for event in processor.finish() {
                                    events.send(event).await.ok();
                                }
                            }
                        });
                        let refine_task = refiner.map(|(mut refiner, jobs)| {
                            let store = this.store.clone();
                            let events = events.clone();
                            cx.background_spawn(async move {
                                while let Ok(job) = jobs.recv().await {
                                    if let Some(event) = refine(refiner.as_mut(), &store, job) {
                                        events.send(event).await.ok();
                                    }
                                }
                            })
                        });
                        drop(events);
                        this.listen(hearing_id, event_receiver, window, cx);
                        this.live = Some(Live {
                            hearing_id,
                            capture,
                            input,
                            paused: false,
                            task,
                            refine_task,
                        });
                        this.load_hearings(None, window, cx);
                    }
                    Err(error) => this.show_error(error, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Shows the events of the recording until the processor and the refine pass stop.
    fn listen(
        &mut self,
        hearing_id: i64,
        events: async_channel::Receiver<Event>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |this, cx| this.apply(hearing_id, event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply(&mut self, hearing_id: i64, event: Event, cx: &mut Context<Self>) {
        let shown = self.selected == Some(hearing_id);
        match event {
            Event::Level(level) => self.level = level,
            Event::Partial {
                segment_id,
                speaker,
                t_start_ms,
                committed,
                tentative,
            } if shown => {
                self.partial = Some(PartialRow {
                    segment_id,
                    speaker,
                    t_start_ms,
                    committed,
                    tentative,
                });
                self.transcript_scroll.scroll_to_bottom();
            }
            Event::Final(segment) if shown => {
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
                self.transcript_scroll.scroll_to_bottom();
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
                }
            }
            Event::Error(error) => self.message = Some(error.message().into()),
            Event::Partial { .. } | Event::Final(_) => return,
        }
        cx.notify();
    }

    fn pause(&mut self, cx: &mut Context<Self>) {
        let Some(live) = self.live.as_mut().filter(|live| !live.paused) else {
            return;
        };
        live.capture.stop();
        live.input.try_send(AudioInput::Pause).ok();
        live.paused = true;
        self.level = 0.0;
        cx.notify();
    }

    fn resume(&mut self, cx: &mut Context<Self>) {
        let Some(live) = self.live.as_mut().filter(|live| live.paused) else {
            return;
        };
        match live.capture.start() {
            Ok(()) => live.paused = false,
            Err(error) => self.message = Some(error.message().into()),
        }
        cx.notify();
    }

    /// Stops the microphone, waits for the last segment, and records the end of the hearing.
    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(live) = self.live.take() else {
            return;
        };
        let Live {
            hearing_id,
            mut capture,
            input,
            task,
            refine_task,
            ..
        } = live;
        capture.stop();
        // The processor input closes when the capture and the sender are gone.
        drop(capture);
        drop(input);
        self.level = 0.0;
        cx.notify();
        let store = self.store.clone();
        let (ended_sender, ended) = async_channel::bounded(1);
        self.finishing.push(cx.background_spawn(async move {
            task.await;
            let result = store.lock().unwrap().end_hearing(hearing_id);
            ended_sender.try_send(result).ok();
            // The refine pass can continue after the end. Its text goes to the store.
            if let Some(refine_task) = refine_task {
                refine_task.await;
            }
        }));
        cx.spawn_in(window, async move |this, cx| {
            let ended = ended.recv().await;
            this.update_in(cx, |this, window, cx| {
                if !matches!(ended, Ok(Ok(_))) {
                    this.show_error(AudioError::Storage, cx);
                }
                this.load_hearings(None, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// At quit, ends the recording like Finalizar, and waits for it. Then the audio file is
    /// complete, and the models are free before the process exits: a Metal model that is
    /// still loaded at exit makes ggml abort.
    fn stop_at_quit(&mut self, cx: &mut Context<Self>) {
        // A recording that Finalizar ended can still be finishing.
        let mut tasks = std::mem::take(&mut self.finishing);
        let mut hearing_id = None;
        if let Some(live) = self.live.take() {
            let Live {
                hearing_id: id,
                mut capture,
                input,
                task,
                refine_task,
                ..
            } = live;
            capture.stop();
            drop(capture);
            drop(input);
            hearing_id = Some(id);
            tasks.push(task);
            tasks.extend(refine_task);
        }
        let finished = cx
            .foreground_executor()
            .block_with_timeout(QUIT_TIMEOUT, async move {
                for task in tasks {
                    task.await;
                }
            })
            .is_ok();
        if let Some(hearing_id) = hearing_id.filter(|_| finished) {
            self.store.lock().unwrap().end_hearing(hearing_id).ok();
        }
    }

    fn toggle_speaker(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
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
            if saved.is_err() {
                this.update(cx, |this, cx| {
                    if let Some(segment) = this.segments.iter_mut().find(|segment| segment.id == id)
                    {
                        segment.speaker = previous;
                    }
                    this.message = Some(SPEAKER_ERROR.into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Downloads the missing required models, one after the other.
    fn download_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let missing = self.missing_models();
        if missing.is_empty() || self.download.is_some() {
            return;
        }
        self.download = Some(0.0);
        self.message = None;
        cx.notify();
        let dirs = self.env.model_dirs.clone();
        let total_mb: u32 = missing.iter().map(|model| model.size_mb).sum();
        let (sender, receiver) = async_channel::unbounded();
        let job = cx.background_spawn(async move {
            let mut done_mb = 0.0;
            let mut last = 0;
            for model in missing {
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

    fn delete(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.starting || self.live.as_ref().is_some_and(|live| live.hearing_id == id) {
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

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let live_id = self.live.as_ref().map(|live| live.hearing_id);
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
            ListItem::new(("hearing", id as u64))
                .rounded(radius)
                .py_2()
                .selected(self.selected == Some(id))
                .accessibility_label(hearing.title.clone())
                .on_click(cx.listener(move |this, _, window, cx| this.select(id, window, cx)))
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

        v_flex()
            .id("hearing-pane")
            .flex_none()
            .w(px(320.))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                div().flex_none().px_2().pt_2().child(
                    Button::new("new-hearing")
                        .outline()
                        .w_full()
                        .icon(IconName::Plus)
                        .label("Nueva audiencia")
                        .disabled(self.case_id.is_none())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_new_hearing_dialog(window, cx)
                        })),
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
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = v_flex().flex_1().min_w_0().h_full().p_4().gap_3();
        let Some(hearing) = self.selected_hearing() else {
            return pane
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Seleccione una audiencia.");
        };
        let is_live = self
            .live
            .as_ref()
            .is_some_and(|live| live.hearing_id == hearing.id);
        let can_record = is_live || hearing.started_at.is_none();

        pane.child(
            h_flex()
                .gap_2()
                .justify_between()
                .child(
                    div()
                        .text_lg()
                        .font_semibold()
                        .truncate()
                        .child(hearing.title.clone()),
                )
                .child(
                    Button::new("delete-hearing")
                        .outline()
                        .danger()
                        .label("Eliminar")
                        .disabled(is_live || self.starting)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)),
                        ),
                ),
        )
        .map(|pane| {
            if can_record {
                pane.child(self.render_controls(is_live, cx))
            } else {
                pane.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(match &hearing.ended_at {
                            Some(ended) => format!("Grabación finalizada: {ended}."),
                            None => "La grabación se detuvo antes de Finalizar.".to_owned(),
                        }),
                )
            }
        })
        .children(self.message.clone().map(|message| {
            div()
                .id("hearing-message")
                .child(Alert::error("hearing-alert", message).on_close(cx.listener(
                    |this, _, _, cx| {
                        this.message = None;
                        cx.notify();
                    },
                )))
                .test_support()
        }))
        .child(self.render_transcript(hearing, cx))
    }

    fn render_controls(&self, is_live: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let paused = self.live.as_ref().is_some_and(|live| live.paused);
        let busy = is_live || self.starting;
        let missing = self.missing_models();
        let download = (!missing.is_empty()).then(|| {
            let names: Vec<&str> = missing.iter().map(|model| model.name).collect();
            let size: u32 = missing.iter().map(|model| model.size_mb).sum();
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("download")
                        .outline()
                        .icon(IconName::ArrowDown)
                        .label("Descargar")
                        .loading(self.download.is_some())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.download_models(window, cx)),
                        ),
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
        let has_models = self.model_select.read(cx).selected_value().is_some();
        let buttons = if !is_live {
            vec![
                Button::new("start")
                    .primary()
                    .icon(IconName::Mic)
                    .label("Iniciar")
                    .loading(self.starting)
                    .on_click(cx.listener(|this, _, window, cx| this.start(window, cx)))
                    .into_any_element(),
            ]
        } else {
            let toggle = if paused {
                Button::new("resume")
                    .primary()
                    .icon(IconName::Play)
                    .label("Reanudar")
                    .on_click(cx.listener(|this, _, _, cx| this.resume(cx)))
            } else {
                Button::new("pause")
                    .outline()
                    .icon(IconName::Pause)
                    .label("Pausar")
                    .on_click(cx.listener(|this, _, _, cx| this.pause(cx)))
            };
            vec![
                toggle.into_any_element(),
                Button::new("finish")
                    .outline()
                    .danger()
                    .icon(IconName::Square)
                    .label("Finalizar")
                    .on_click(cx.listener(|this, _, window, cx| this.finish(window, cx)))
                    .into_any_element(),
            ]
        };

        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(
                        Select::new(&self.model_select)
                            .id("model-select")
                            .w(px(240.))
                            .placeholder("Sin modelo instalado")
                            .accessibility_label("Modelo de voz")
                            .disabled(busy || !has_models),
                    )
                    .child(
                        Select::new(&self.mic_select)
                            .id("mic-select")
                            .w(px(240.))
                            .placeholder("Sin micrófono")
                            .accessibility_label("Micrófono")
                            .disabled(busy || self.microphones.as_ref().is_none_or(Vec::is_empty)),
                    )
                    .children(buttons),
            )
            .children(download)
            .child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .child(
                        Switch::new("accused")
                            .label("Habla el acusado")
                            .checked(self.accused.load(Ordering::Relaxed))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.accused.store(*checked, Ordering::Relaxed);
                                cx.notify();
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_color(muted)
                            .child(Icon::new(IconName::Mic).small())
                            .child(
                                div().w(px(120.)).child(
                                    Progress::new("mic-level")
                                        .value(self.level * 100.)
                                        .accessibility_label("Nivel del micrófono"),
                                ),
                            ),
                    ),
            )
    }

    fn render_transcript(&self, hearing: &Hearing, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let frame = div()
            .id("transcript-frame")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border);
        if self.segments.is_empty() && self.partial.is_none() {
            let text = if hearing.started_at.is_none() {
                "Seleccione Iniciar para grabar y transcribir la audiencia."
            } else if self.live.is_some() {
                "Nicolaw escucha. El texto aparece aquí cuando alguien habla."
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
            render_row(
                ("segment", id as u64),
                segment.t_start_ms,
                segment.speaker,
                div().text_sm().child(segment.text.clone()),
                Some(
                    Button::new(("toggle-speaker", id as u64))
                        .ghost()
                        .xsmall()
                        .icon(IconName::RefreshCw)
                        .tooltip("Cambiar hablante")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.toggle_speaker(id, window, cx)
                        })),
                ),
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
                    .children(partial),
            )
            .vertical_scrollbar(&self.transcript_scroll)
    }
}

/// One transcript row: time, speaker, text, and the speaker toggle of a final segment.
fn render_row(
    id: impl Into<ElementId>,
    t_start_ms: i64,
    speaker: Speaker,
    text: Div,
    toggle: Option<Button>,
    muted: Hsla,
) -> AnyElement {
    let tag = match speaker {
        Speaker::Accused => Tag::warning(),
        Speaker::Other => Tag::secondary(),
    };
    h_flex()
        .id(id)
        .gap_3()
        .items_start()
        .child(
            div()
                .flex_none()
                .w(px(56.))
                .pt_0p5()
                .text_xs()
                .text_color(muted)
                .child(format_time(t_start_ms)),
        )
        .child(
            h_flex()
                .flex_none()
                .w(px(104.))
                .gap_1()
                .items_center()
                .child(tag.small().child(speaker_label(speaker)))
                .children(toggle),
        )
        .child(text.flex_1().min_w_0())
        .test_support()
        .into_any_element()
}

pub fn speaker_label(speaker: Speaker) -> &'static str {
    match speaker {
        Speaker::Accused => "Acusado",
        Speaker::Other => "Otro",
    }
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("hearings")
            .size_full()
            .items_start()
            .child(self.render_list(cx))
            .child(self.render_detail(cx))
            .test_support()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
        WindowOptions, px, size,
    };

    use super::{HearingsEnv, HearingsView, format_time, speaker_label};
    use crate::audio::fake::FakeBackend;
    use crate::audio::pipeline::Event;
    use crate::audio::{AudioError, ModelPaths};
    use crate::models::{ModelDirs, NEMOTRON, SILERO_VAD, write_fake_model};
    use crate::store::{Segment, Speaker, Store};

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

    fn open(
        name: &str,
        store: Store,
        case: i64,
        backend: &FakeBackend,
        dirs: &ModelDirs,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<HearingsView>) {
        let env = HearingsEnv {
            backend: Arc::new(backend.clone()),
            model_dirs: dirs.clone(),
            data_dir: temp_dir(&format!("data-{name}")),
        };
        cx.update(|cx| {
            gpui_kit::init(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1100.), px(720.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    let mut view = HearingsView::new(Arc::new(Mutex::new(store)), env, window, cx);
                    view.set_case(Some(case), window, cx);
                    view
                })
            })
            .unwrap()
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

    /// Waits until the models and the microphones are known.
    async fn wait_until_ready(
        cx: &mut TestAppContext,
        window: AnyWindowHandle,
        view: &Entity<HearingsView>,
    ) {
        cx.wait_for(window, WAIT, |_, cx| {
            let view = view.read(cx);
            view.microphones.is_some() && view.models.is_some()
        })
        .await;
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
        assert_eq!(speaker_label(Speaker::Other), "Otro");
    }

    #[gpui_kit::test]
    async fn a_hearing_is_created_recorded_with_speakers_and_finished(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let backend = FakeBackend::new("Yo no estaba allí.");
        let dirs = model_dirs("record", true);
        let (window, view) = open("record", store, case, &backend, &dirs, cx);
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

        // Start: the live model and the voice detector load, and the microphone records.
        wait_until_ready(cx, window, &view).await;
        click(cx, window, "start");
        cx.wait_for(window, WAIT, |_, _| backend.input.lock().unwrap().is_some())
            .await;
        assert_eq!(
            *backend.loaded.lock().unwrap(),
            Some(ModelPaths {
                live: dirs.handy.join(NEMOTRON.file),
                vad: dirs.handy.join(SILERO_VAD.file),
                refine: None,
            })
        );

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
        cx.update_window(window, |_, window, _| {
            assert!(window.find(("segment", segment.id as u64)).visible());
        })
        .unwrap();

        // The toggle of a row corrects its speaker.
        click(cx, window, ("toggle-speaker", segment.id as u64));
        cx.wait_for(window, WAIT, |_, cx| {
            view.read(cx).segments[0].speaker == Speaker::Accused
        })
        .await;
        cx.run_until_parked();
        cx.update(|cx| {
            let store = view.read(cx).store.lock().unwrap();
            assert_eq!(
                store.segments(hearing.id).unwrap()[0].speaker,
                Speaker::Accused
            );
        });

        // The switch labels the next segments.
        click(cx, window, "accused");
        cx.run_until_parked();
        assert!(cx.update(|cx| view.read(cx).accused.load(Ordering::Relaxed)));
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
            view.live.is_none() && view.hearings[0].ended_at.is_some()
        })
        .await;
        assert!(backend.input.lock().unwrap().is_none());
        let audio = cx.update(|cx| view.read(cx).hearings[0].audio_path.clone().unwrap());
        assert_eq!(
            hound::WavReader::open(audio).unwrap().duration(),
            (10 + 19 + 5 + 19) * 512
        );
    }

    #[gpui_kit::test]
    async fn pause_ends_the_segment_and_resume_records_again(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let id = store.create_hearing(case, "Audiencia").unwrap().id;
        let backend = FakeBackend::new("Continúe.");
        let dirs = model_dirs("pause", true);
        let (window, view) = open("pause", store, case, &backend, &dirs, cx);
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, window, ("hearing", id as u64));
        wait_until_ready(cx, window, &view).await;
        click(cx, window, "start");
        cx.wait_for(window, WAIT, |_, _| backend.input.lock().unwrap().is_some())
            .await;

        backend.send(0.5, 5);
        click(cx, window, "pause");
        cx.wait_for(window, WAIT, |window, cx| {
            view.read(cx).segments.len() == 1 && window.try_find("resume").is_some()
        })
        .await;
        assert!(backend.input.lock().unwrap().is_none());
        click(cx, window, "resume");

        assert!(backend.input.lock().unwrap().is_some());
        cx.update_window(window, |_, window, _| {
            assert!(window.try_find("pause").is_some());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    async fn without_models_start_shows_a_message_and_the_download_button(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let id = store.create_hearing(case, "Audiencia").unwrap().id;
        let backend = FakeBackend::new("");
        let dirs = model_dirs("none", false);
        let (window, view) = open("none", store, case, &backend, &dirs, cx);
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;

        click(cx, window, ("hearing", id as u64));
        wait_until_ready(cx, window, &view).await;
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
                view.read(cx).message,
                Some(AudioError::NoModel.message().into())
            );
        });
        assert!(backend.input.lock().unwrap().is_none());
    }

    #[gpui_kit::test]
    async fn a_second_hearing_cannot_start_while_one_records(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let first = store.create_hearing(case, "Primera").unwrap().id;
        let second = store.create_hearing(case, "Segunda").unwrap().id;
        let backend = FakeBackend::new("Sí.");
        let dirs = model_dirs("busy", true);
        let (window, view) = open("busy", store, case, &backend, &dirs, cx);
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hearing", first as u64)).is_some()
        })
        .await;
        click(cx, window, ("hearing", first as u64));
        wait_until_ready(cx, window, &view).await;
        click(cx, window, "start");
        cx.wait_for(window, WAIT, |_, cx| view.read(cx).live.is_some())
            .await;

        click(cx, window, ("hearing", second as u64));
        cx.wait_for(window, WAIT, |window, _| window.try_find("start").is_some())
            .await;
        click(cx, window, "start");

        cx.wait_for(window, WAIT, |_, cx| view.read(cx).message.is_some())
            .await;
        cx.update(|cx| {
            let view = view.read(cx);
            assert_eq!(view.message, Some(AudioError::Busy.message().into()));
            assert_eq!(view.live.as_ref().map(|live| live.hearing_id), Some(first));
        });
    }

    #[gpui_kit::test]
    async fn delete_removes_the_hearing(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let id = store.create_hearing(case, "Audiencia").unwrap().id;
        let backend = FakeBackend::new("");
        let dirs = model_dirs("delete", true);
        let (window, view) = open("delete", store, case, &backend, &dirs, cx);
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
            let view = view.read(cx);
            assert!(view.hearings.is_empty());
            assert_eq!(view.selected, None);
        });
    }

    #[gpui_kit::test]
    async fn a_hearing_cannot_be_deleted_while_a_recording_starts(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let id = store.create_hearing(case, "Audiencia").unwrap().id;
        let backend = FakeBackend::new("");
        let dirs = model_dirs("delete-starting", true);
        let (window, view) = open("delete-starting", store, case, &backend, &dirs, cx);
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, window, ("hearing", id as u64));
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("delete-hearing").is_some()
        })
        .await;

        view.update(cx, |view, cx| {
            view.starting = true;
            cx.notify();
        });
        cx.run_until_parked();
        click(cx, window, "delete-hearing");
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(window.try_find("ok").is_none());
            view.update(cx, |view, cx| view.delete(id, window, cx));
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            let view = view.read(cx);
            assert_eq!(view.hearings.len(), 1);
            assert_eq!(view.store.lock().unwrap().hearings(case).unwrap().len(), 1);
        });
    }

    #[gpui_kit::test]
    async fn quit_after_finish_waits_for_the_end_of_the_hearing(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let id = store.create_hearing(case, "Audiencia").unwrap().id;
        let backend = FakeBackend::new("Fin.");
        let dirs = model_dirs("quit-finish", true);
        let (window, view) = open("quit-finish", store, case, &backend, &dirs, cx);
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, window, ("hearing", id as u64));
        wait_until_ready(cx, window, &view).await;
        click(cx, window, "start");
        cx.wait_for(window, WAIT, |_, _| backend.input.lock().unwrap().is_some())
            .await;
        backend.send(0.5, 5);

        click(cx, window, "finish");
        view.update(cx, |view, cx| view.stop_at_quit(cx));

        cx.update(|cx| {
            let store = view.read(cx).store.lock().unwrap();
            assert!(store.hearings(case).unwrap()[0].ended_at.is_some());
            assert_eq!(store.segments(id).unwrap()[0].text, "Fin.");
        });
    }

    #[gpui_kit::test]
    async fn a_final_segment_that_is_already_shown_is_not_shown_twice(cx: &mut TestAppContext) {
        let (store, case) = store_with_case();
        let id = store.create_hearing(case, "Audiencia").unwrap().id;
        let backend = FakeBackend::new("");
        let dirs = model_dirs("final-twice", true);
        let (window, view) = open("final-twice", store, case, &backend, &dirs, cx);
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hearing", id as u64)).is_some()
        })
        .await;
        click(cx, window, ("hearing", id as u64));
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

        view.update(cx, |view, cx| {
            view.segments = vec![segment.clone()];
            view.apply(id, Event::Final(segment.clone()), cx);
        });

        cx.update(|cx| assert_eq!(view.read(cx).segments, [segment]));
    }
}
