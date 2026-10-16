use std::cell::Cell;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::attachment::{
    Attachment, AttachmentContent, AttachmentDescription, AttachmentMedia, AttachmentStatus,
    AttachmentTitle,
};
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::description_list::DescriptionList;
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::notification::NotificationType;
use gpui_kit::component::progress::{Progress, ProgressCircle};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::tab::{self as tabs, TabBar};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::analysis;
use crate::extract::{Page, PageSource};
use crate::preview::{self, Preview};
use crate::store::{AnalysisStatus, Case, Evidence, ImportError, SearchHit, Store, file_name};
use crate::transfer::{Queue, State};

/// The settings key of the last selected case.
const LAST_CASE: &str = "last_case";
/// The search shows at most this number of hits.
const SEARCH_LIMIT: usize = 50;

const LIST_ERROR: &str = "Nicolaw no puede leer las pruebas.";
const DELETE_ERROR: &str = "Nicolaw no puede eliminar el archivo. Intente de nuevo.";
const CASE_ERROR: &str = "Nicolaw no puede crear el caso. Intente de nuevo.";
const SEARCH_ERROR: &str = "Nicolaw no puede buscar. Intente de nuevo.";
const NO_CASE: &str = "Cree un caso antes de agregar archivos.";

/// The preview of the selected file, after it is loaded from the store.
enum Loaded {
    /// Percent of the content that is read.
    Loading(u8),
    Image(Arc<Image>),
    Text(SharedString),
    Unavailable,
}

/// The tab of the detail pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Preview,
    Text,
}

/// One case in the case select.
#[derive(Clone)]
struct CaseItem {
    id: i64,
    name: SharedString,
}

impl SelectItem for CaseItem {
    type Value = i64;

    fn title(&self) -> SharedString {
        self.name.clone()
    }

    fn value(&self) -> &i64 {
        &self.id
    }
}

pub struct FilesView {
    store: Arc<Mutex<Store>>,
    cases: Vec<Case>,
    case_id: Option<i64>,
    case_select: Entity<SelectState<Vec<CaseItem>>>,
    search: Entity<InputState>,
    /// The trimmed search text. While it is not empty, the list shows the search hits.
    query: String,
    hits: Vec<SearchHit>,
    search_task: Option<Task<()>>,
    files: Vec<Evidence>,
    selected: Option<i64>,
    tab: Tab,
    preview: Loaded,
    preview_task: Option<Task<()>>,
    /// The extracted pages of the selected file. `None` while they load.
    pages: Option<Vec<Page>>,
    pages_task: Option<Task<()>>,
    /// The page to show in the text tab, after the user selects a search hit.
    focus_page: Option<u32>,
    text_scroll: ScrollHandle,
    /// False until the first list result arrives.
    list_loaded: bool,
    queue: Queue,
    /// The case of each file in the queue, in queue order.
    queue_cases: VecDeque<i64>,
    /// True while a worker saves the files in the queue.
    saving: bool,
    /// Files saved in this session. Their rows show a highlight one time.
    highlighted: HashSet<i64>,
    /// True while Open prepares the file.
    opening: bool,
    /// Evidence that waits for text analysis, in order.
    analysis: VecDeque<i64>,
    /// True while a worker analyzes the evidence in `analysis`.
    analyzing: bool,
    _subscriptions: Vec<Subscription>,
}

impl FilesView {
    pub fn new(store: Store, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = Arc::new(Mutex::new(store));
        let case_select = cx.new(|cx| SelectState::new(Vec::<CaseItem>::new(), None, window, cx));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Buscar en las pruebas…"));
        let subscriptions = vec![
            cx.subscribe_in(
                &case_select,
                window,
                |this, _, event: &SelectEvent<Vec<CaseItem>>, window, cx| {
                    if let SelectEvent::Confirm(Some(id)) = event {
                        this.open_case(*id, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &search,
                window,
                |this, state, event: &InputEvent, window, cx| {
                    if let InputEvent::Change = event {
                        let query = state.read(cx).value().to_string();
                        this.run_search(query, window, cx);
                    }
                },
            ),
        ];

        let start_store = store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let start = cx
                .background_spawn(async move {
                    let store = start_store.lock().unwrap();
                    let cases = store.cases()?;
                    let last = store
                        .setting(LAST_CASE)?
                        .and_then(|value| value.parse::<i64>().ok())
                        .filter(|id| cases.iter().any(|case| case.id == *id));
                    let case_id = last.or(cases.first().map(|case| case.id));
                    Ok::<_, rusqlite::Error>((cases, case_id, store.unfinished_analysis()?))
                })
                .await;
            this.update_in(cx, |this, window, cx| match start {
                Ok((cases, case_id, unfinished)) => {
                    this.set_cases(cases, case_id, window, cx);
                    match case_id {
                        Some(id) => this.open_case(id, window, cx),
                        None => this.list_loaded = true,
                    }
                    this.enqueue_analysis(unfinished, window, cx);
                    cx.notify();
                }
                Err(_) => {
                    this.list_loaded = true;
                    window.push_notification((NotificationType::Error, LIST_ERROR), cx);
                }
            })
            .ok();
        })
        .detach();

        Self {
            store,
            cases: Vec::new(),
            case_id: None,
            case_select,
            search,
            query: String::new(),
            hits: Vec::new(),
            search_task: None,
            files: Vec::new(),
            selected: None,
            tab: Tab::Preview,
            preview: Loaded::Unavailable,
            preview_task: None,
            pages: None,
            pages_task: None,
            focus_page: None,
            text_scroll: ScrollHandle::new(),
            list_loaded: false,
            queue: Queue::default(),
            queue_cases: VecDeque::new(),
            saving: false,
            highlighted: HashSet::new(),
            opening: false,
            analysis: VecDeque::new(),
            analyzing: false,
            _subscriptions: subscriptions,
        }
    }

    /// Replaces the items of the case select, and shows `selected` in it.
    fn set_cases(
        &mut self,
        cases: Vec<Case>,
        selected: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = cases
            .iter()
            .map(|case| CaseItem {
                id: case.id,
                name: case.name.clone().into(),
            })
            .collect();
        self.cases = cases;
        self.case_select.update(cx, |select, cx| {
            select.set_items(items, window, cx);
            if let Some(id) = selected {
                select.set_selected_value(&id, window, cx);
            }
        });
        cx.notify();
    }

    /// Shows the evidence of the case, and saves the case as the last selected case.
    fn open_case(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.case_id == Some(id) {
            return;
        }
        self.case_id = Some(id);
        self.files.clear();
        self.clear_selection();
        self.query.clear();
        self.hits.clear();
        self.search_task = None;
        self.search
            .update(cx, |search, cx| search.set_value("", window, cx));
        self.list_loaded = false;
        cx.notify();

        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let files = cx
                .background_spawn(async move {
                    let store = store.lock().unwrap();
                    // The list is more important than the setting: ignore a setting error.
                    store.set_setting(LAST_CASE, &id.to_string()).ok();
                    store.list(id).map_err(|_| LIST_ERROR)
                })
                .await;
            this.update_in(cx, |this, window, cx| this.set_files(id, files, window, cx))
                .ok();
        })
        .detach();
    }

    fn open_new_case_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Nombre del caso"));
        let view = cx.entity().downgrade();
        // Returns false if the name is empty, so that the dialog stays open.
        let submit = Rc::new({
            let input = input.clone();
            move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    return false;
                }
                view.update(cx, |this, cx| this.create_case(name, window, cx))
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
                .title("Nuevo caso")
                .w(px(420.))
                .child(
                    div()
                        .id("case-name")
                        .child(Input::new(&input))
                        .test_support(),
                )
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|button| button.label("Cancelar")))
                        .child(
                            Button::new("create-case")
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

    fn create_case(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let created = cx
                .background_spawn(async move {
                    let store = store.lock().unwrap();
                    let case = store.create_case(&name)?;
                    Ok::<_, rusqlite::Error>((case.id, store.cases()?))
                })
                .await;
            this.update_in(cx, |this, window, cx| match created {
                Ok((id, cases)) => {
                    this.set_cases(cases, Some(id), window, cx);
                    this.open_case(id, window, cx);
                }
                Err(_) => window.push_notification((NotificationType::Error, CASE_ERROR), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Sets the evidence list of a case. A list for a case that is not open is ignored.
    fn set_files(
        &mut self,
        case_id: i64,
        files: Result<Vec<Evidence>, &'static str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.case_id != Some(case_id) {
            return;
        }
        match files {
            Ok(files) => self.files = files,
            Err(message) => window.push_notification((NotificationType::Error, message), cx),
        }
        self.list_loaded = true;
        if self.selected_entry().is_none() {
            self.clear_selection();
        }
        cx.notify();
    }

    /// Replaces one entry of the list, if the list shows it.
    fn update_entry(&mut self, entry: Evidence, cx: &mut Context<Self>) {
        if let Some(file) = self.files.iter_mut().find(|file| file.id == entry.id) {
            *file = entry;
            cx.notify();
        }
    }

    fn clear_selection(&mut self) {
        self.selected = None;
        self.preview_task = None;
        self.pages = None;
        self.pages_task = None;
        self.focus_page = None;
    }

    fn selected_entry(&self) -> Option<&Evidence> {
        let id = self.selected?;
        self.files.iter().find(|file| file.id == id)
    }

    fn prompt_for_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Agregar".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                this.update_in(cx, |this, window, cx| this.add_files(paths, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn add_files(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(case_id) = self.case_id else {
            window.push_notification((NotificationType::Warning, NO_CASE), cx);
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            // The UI thread does not touch the file system.
            let files = cx
                .background_spawn(async move {
                    paths
                        .into_iter()
                        .map(|path| {
                            let size = fs::metadata(&path).map_or(0, |metadata| metadata.len());
                            (file_name(&path), size, path)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                for (name, size, path) in files {
                    this.queue.add(path, name, size);
                    this.queue_cases.push_back(case_id);
                }
                cx.notify();
                if !this.saving {
                    this.saving = true;
                    this.save_queue(window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Saves the files in the queue, one at a time, until the queue is empty.
    fn save_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let next = this.update(cx, |this, cx| {
                    let next = this.queue.start_next();
                    this.saving = next.is_some();
                    cx.notify();
                    next.zip(this.queue_cases.pop_front())
                });
                let Ok(Some(((_, path), case_id))) = next else {
                    break;
                };

                let (sender, receiver) = async_channel::bounded(1);
                let store = store.clone();
                // The store is locked for one file, so that a preview can load between files.
                let job = cx.background_spawn(async move {
                    let store = store.lock().unwrap();
                    let entry = store.import(case_id, &path, |done, _| {
                        sender.try_send(done).ok();
                    });
                    (entry, store.list(case_id).map_err(|_| LIST_ERROR))
                });
                while let Ok(done) = receiver.recv().await {
                    this.update(cx, |this, cx| {
                        if this.queue.set_progress(done) {
                            cx.notify();
                        }
                    })
                    .ok();
                }
                let (entry, files) = job.await;
                this.update_in(cx, |this, window, cx| {
                    this.finish_transfer(case_id, entry, files, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    fn finish_transfer(
        &mut self,
        case_id: i64,
        entry: Result<Evidence, ImportError>,
        files: Result<Vec<Evidence>, &'static str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut saved = None;
        match entry {
            Ok(entry) => {
                self.queue.complete();
                self.highlighted.insert(entry.id);
                saved = Some(entry.id);
            }
            Err(error) => {
                let active = self.queue.transfers().iter();
                let name = active
                    .filter(|transfer| transfer.state == State::Saving)
                    .map(|transfer| transfer.name.clone())
                    .next()
                    .unwrap_or_default();
                self.queue.fail();
                window
                    .push_notification((NotificationType::Error, import_message(error, &name)), cx);
            }
        }
        if self.queue.is_empty() && self.queue.succeeded() > 0 {
            let message = match self.queue.succeeded() {
                1 => "Se agregó 1 archivo.".to_owned(),
                n => format!("Se agregaron {n} archivos."),
            };
            window.push_notification((NotificationType::Success, message), cx);
        }
        self.set_files(case_id, files, window, cx);
        // The analysis starts after the list shows the new file as pending.
        self.enqueue_analysis(saved, window, cx);
    }

    fn enqueue_analysis(
        &mut self,
        ids: impl IntoIterator<Item = i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.analysis.extend(ids);
        if !self.analyzing && !self.analysis.is_empty() {
            self.analyzing = true;
            self.analyze_queue(window, cx);
        }
    }

    /// Analyzes the evidence in the analysis queue, one file at a time, on a background thread.
    fn analyze_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let next = this.update(cx, |this, _| {
                    let next = this.analysis.pop_front();
                    this.analyzing = next.is_some();
                    next
                });
                let Ok(Some(id)) = next else {
                    break;
                };

                let job_store = store.clone();
                let started = cx
                    .background_spawn(async move {
                        let store = job_store.lock().unwrap();
                        store.set_status(id, AnalysisStatus::Analyzing, None)?;
                        store.evidence(id)
                    })
                    .await;
                // An error here means that the user deleted the evidence.
                let Ok(entry) = started else {
                    continue;
                };
                this.update(cx, |this, cx| this.update_entry(entry, cx))
                    .ok();

                let job_store = store.clone();
                let finished = cx
                    .background_spawn(async move {
                        analysis::run(&job_store, id)?;
                        job_store.lock().unwrap().evidence(id)
                    })
                    .await;
                this.update_in(cx, |this, window, cx| {
                    // On an error the status stays `analyzing`, and the next start tries again.
                    if let Ok(entry) = finished {
                        this.finish_analysis(entry, window, cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn finish_analysis(&mut self, entry: Evidence, window: &mut Window, cx: &mut Context<Self>) {
        let id = entry.id;
        self.update_entry(entry, cx);
        if self.selected == Some(id) {
            self.load_pages(id, window, cx);
        }
        if !self.query.is_empty() {
            self.run_search(self.query.clone(), window, cx);
        }
    }

    fn run_search(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        self.query = query.trim().to_owned();
        cx.notify();
        let Some(case_id) = self.case_id.filter(|_| !self.query.is_empty()) else {
            self.hits.clear();
            self.search_task = None;
            return;
        };
        let store = self.store.clone();
        let query = self.query.clone();
        // Replacing the task cancels the previous search.
        self.search_task = Some(cx.spawn_in(window, async move |this, cx| {
            let hits = cx
                .background_spawn(async move {
                    store.lock().unwrap().search(case_id, &query, SEARCH_LIMIT)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                match hits {
                    Ok(hits) => this.hits = hits,
                    Err(_) => window.push_notification((NotificationType::Error, SEARCH_ERROR), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Selects the evidence of the hit and shows its text at the page of the hit.
    fn open_hit(&mut self, hit: &SearchHit, window: &mut Window, cx: &mut Context<Self>) {
        self.select(hit.evidence_id, window, cx);
        self.tab = Tab::Text;
        self.focus_page = Some(hit.page);
        self.scroll_to_focus_page();
        cx.notify();
    }

    fn scroll_to_focus_page(&mut self) {
        let (Some(page), Some(pages)) = (self.focus_page, &self.pages) else {
            return;
        };
        if let Some(index) = pages.iter().position(|p| p.number == page) {
            self.text_scroll.scroll_to_item(index);
            self.focus_page = None;
        }
    }

    fn select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected == Some(id) {
            return;
        }
        self.clear_selection();
        self.selected = Some(id);
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.preview = Loaded::Loading(0);
        self.load_pages(id, window, cx);
        cx.notify();

        let name = entry.name.clone();
        let store = self.store.clone();
        // Replacing the task cancels the load for the previous selection.
        self.preview_task = Some(cx.spawn_in(window, async move |this, cx| {
            let (sender, receiver) = async_channel::bounded(1);
            let job = cx.background_spawn(async move {
                let content = store
                    .lock()
                    .unwrap()
                    .content(entry.id, |done, total| {
                        sender.try_send((done * 100 / total.max(1)) as u8).ok();
                    })
                    .ok()?;
                Some(match preview::classify(&entry.mime, &content) {
                    Preview::Image(format) => {
                        Loaded::Image(Arc::new(Image::from_bytes(format, content)))
                    }
                    Preview::Text(text) => Loaded::Text(text.into()),
                    Preview::None => Loaded::Unavailable,
                })
            });
            while let Ok(percent) = receiver.recv().await {
                this.update(cx, |this, cx| {
                    if let Loaded::Loading(current) = &mut this.preview
                        && *current != percent
                    {
                        *current = percent;
                        cx.notify();
                    }
                })
                .ok();
            }
            let loaded = job.await;

            this.update_in(cx, |this, window, cx| {
                this.preview = loaded.unwrap_or_else(|| {
                    let message = format!("Nicolaw no puede leer \"{name}\".");
                    window.push_notification((NotificationType::Error, message), cx);
                    Loaded::Unavailable
                });
                cx.notify();
            })
            .ok();
        }));
    }

    fn load_pages(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        self.pages_task = Some(cx.spawn_in(window, async move |this, cx| {
            let pages = cx
                .background_spawn(async move { store.lock().unwrap().pages(id) })
                .await;
            this.update(cx, |this, cx| {
                if this.selected == Some(id) {
                    this.pages = Some(pages.unwrap_or_default());
                    this.scroll_to_focus_page();
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Writes the selected file to a temporary folder and opens it with the system application.
    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        if self.opening {
            return;
        }
        self.opening = true;
        cx.notify();
        let name = entry.name.clone();
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let path = cx
                .background_spawn(async move {
                    let content = store.lock().unwrap().content(entry.id, |_, _| {}).ok()?;
                    let dir = std::env::temp_dir()
                        .join("nicolaw")
                        .join(entry.id.to_string());
                    fs::create_dir_all(&dir).ok()?;
                    let path = dir.join(&entry.name);
                    fs::write(&path, content).ok()?;
                    Some(path)
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                this.opening = false;
                cx.notify();
                match path {
                    Some(path) => cx.open_with_system(&path),
                    None => {
                        let message = format!("Nicolaw no puede abrir \"{name}\".");
                        window.push_notification((NotificationType::Error, message), cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let view = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let view = view.clone();
            let id = entry.id;
            alert
                .title("¿Eliminar este archivo?")
                .description(SharedString::from(format!(
                    "Nicolaw eliminará \"{}\" y su texto. No puede deshacer esta acción.",
                    entry.name
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
        let Some(case_id) = self.case_id else {
            return;
        };
        self.analysis.retain(|queued| *queued != id);
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let files = cx
                .background_spawn(async move {
                    let store = store.lock().unwrap();
                    store.delete(id).map_err(|_| DELETE_ERROR)?;
                    store.list(case_id).map_err(|_| LIST_ERROR)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.set_files(case_id, files, window, cx);
                if !this.query.is_empty() {
                    this.run_search(this.query.clone(), window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = (!self.files.is_empty()).then(|| self.files.len().to_string());
        let summary = self
            .queue
            .position()
            .map(|(n, total)| format!("Guardando {n} de {total}"));
        let muted = cx.theme().muted_foreground;
        h_flex()
            .relative()
            .flex_none()
            .justify_between()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Select::new(&self.case_select)
                            .id("case-select")
                            .w(px(260.))
                            .placeholder("Seleccione un caso")
                            .accessibility_label("Caso")
                            .disabled(self.cases.is_empty()),
                    )
                    .child(
                        Button::new("new-case")
                            .outline()
                            .icon(IconName::Plus)
                            .label("Nuevo caso")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_new_case_dialog(window, cx)
                            })),
                    )
                    .children(count.map(|count| div().text_sm().text_color(muted).child(count)))
                    .children(
                        summary
                            .map(|summary| div().ml_2().text_sm().text_color(muted).child(summary)),
                    ),
            )
            .child(
                Button::new("add-files")
                    .primary()
                    .icon(IconName::Plus)
                    .label("Agregar archivos")
                    .disabled(self.case_id.is_none())
                    .on_click(cx.listener(|this, _, window, cx| this.prompt_for_files(window, cx))),
            )
            // Absolute, so that the header height does not change when the bar shows.
            .when(!self.queue.is_empty(), |header| {
                header.child(
                    div().absolute().left_0().right_0().bottom_0().child(
                        Progress::new("save-progress")
                            .xsmall()
                            .rounded_none()
                            .value(self.queue.overall_percent() as f32),
                    ),
                )
            })
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let list = v_flex().id("file-list").flex_1().min_h_0().p_2().gap_1();
        let list = if !self.list_loaded {
            list.child(self.render_skeleton(cx))
        } else if self.query.is_empty() {
            list.children(self.render_transfers())
                .children(self.render_rows(cx))
        } else if self.hits.is_empty() {
            list.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("No hay resultados para \"{}\".", self.query)),
            )
        } else {
            list.children(self.render_hits(cx))
        };

        div()
            .id("list-pane")
            .flex_none()
            .w(px(320.))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .size_full()
                    .child(
                        div()
                            .id("search")
                            .flex_none()
                            .px_2()
                            .pt_2()
                            .child(
                                Input::new(&self.search)
                                    .prefix(Icon::new(IconName::Search).small())
                                    .cleanable(true)
                                    .disabled(!self.list_loaded),
                            )
                            .test_support(),
                    )
                    .child(list.overflow_y_scrollbar()),
            )
            .test_support()
    }

    fn render_transfers(&self) -> Vec<AnyElement> {
        self.queue
            .transfers()
            .iter()
            .map(|transfer| {
                let (status, description) = match transfer.state {
                    State::Waiting => (AttachmentStatus::Pending, "En espera"),
                    // The component adds " · <n>%" to the description.
                    State::Saving => (AttachmentStatus::Uploading, "Guardando"),
                };
                Attachment::new()
                    .id(("transfer", transfer.id))
                    .w_full()
                    .status(status)
                    .progress(transfer.percent() as f32)
                    .media(AttachmentMedia::new().child(Icon::new(IconName::File)))
                    .content(
                        AttachmentContent::new()
                            .title(AttachmentTitle::new(transfer.name.clone()))
                            .description(AttachmentDescription::new(description)),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn render_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let highlight = cx.theme().primary;
        self.files
            .iter()
            .map(|file| {
                let id = file.id;
                let row = ListItem::new(("file", id as u64))
                    .rounded(radius)
                    .py_2()
                    .selected(self.selected == Some(id))
                    .accessibility_label(file.name.clone())
                    .on_click(cx.listener(move |this, _, window, cx| this.select(id, window, cx)))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(div().text_sm().truncate().child(file.name.clone()))
                                    .child(div().text_xs().text_color(muted).child(format!(
                                        "{} · {}",
                                        preview::type_label(&file.name),
                                        preview::format_size(file.size)
                                    ))),
                            )
                            .child(
                                div()
                                    .id(("status", id as u64))
                                    .flex_none()
                                    .child(status_tag(file.status))
                                    .test_support(),
                            ),
                    );
                if !self.highlighted.contains(&id) {
                    return row.into_any_element();
                }
                // One-shot: the id keeps the finished state, so later renders do not replay it.
                row.with_animation(
                    ("highlight", id as u64),
                    Animation::new(Duration::from_millis(1500)),
                    move |row, delta| row.bg(highlight.opacity(0.2 * (1. - delta))),
                )
                .into_any_element()
            })
            .collect()
    }

    fn render_hits(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        self.hits
            .iter()
            .map(|hit| {
                let open = hit.clone();
                ListItem::new(("hit", hit.chunk_id as u64))
                    .rounded(radius)
                    .py_2()
                    .selected(self.selected == Some(hit.evidence_id))
                    .accessibility_label(format!("{}, página {}", hit.evidence_name, hit.page))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_hit(&open, window, cx)),
                    )
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_sm()
                                            .min_w_0()
                                            .truncate()
                                            .child(hit.evidence_name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(format!("Página {}", hit.page)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .line_clamp(3)
                                    .child(hit.snippet.clone()),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// Placeholder rows until the first list arrives. Each row uses the list item and the
    /// text line heights of a file row, so that the rows do not move when the list arrives.
    fn render_skeleton(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let radius = cx.theme().radius;
        let bar = move |width: f32| {
            Skeleton::new()
                .absolute()
                .left_0()
                .top_1()
                .bottom_1()
                .w(px(width))
                .rounded(radius)
        };
        v_flex()
            .id("list-skeleton")
            .gap_1()
            .children((0..6).map(|i| {
                ListItem::new(("skeleton", i as u64))
                    .py_2()
                    .disabled(true)
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .relative()
                                    .text_sm()
                                    .child("\u{a0}")
                                    .child(bar(if i % 2 == 0 { 180. } else { 140. })),
                            )
                            .child(div().relative().text_xs().child("\u{a0}").child(bar(80.))),
                    )
            }))
            .test_support()
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = v_flex().flex_1().min_w_0().h_full().p_4().gap_4();
        if !self.list_loaded {
            return pane;
        }
        let Some(file) = self.selected_entry() else {
            return pane
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Seleccione un archivo para revisarlo.");
        };
        let sha256 = file.sha256.get(..12).unwrap_or(&file.sha256);
        let pages = match file.pages {
            0 => "—".to_owned(),
            n => n.to_string(),
        };

        pane.child(
            div()
                .text_lg()
                .font_semibold()
                .truncate()
                .child(file.name.clone()),
        )
        .child(
            DescriptionList::horizontal()
                .columns(1)
                .bordered(false)
                .label_width(px(90.))
                .item("Tipo", file.mime.clone(), 1)
                .item("Tamaño", preview::format_size(file.size), 1)
                .item("SHA-256", format!("{sha256}…"), 1)
                .item("Agregado", file.added_at.clone(), 1)
                .item("Estado", status_label(file.status), 1)
                .item("Páginas", pages, 1),
        )
        .child(
            TabBar::new("detail-tabs")
                .underline()
                .selected_index(match self.tab {
                    Tab::Preview => 0,
                    Tab::Text => 1,
                })
                .on_click(cx.listener(|this, index: &usize, _, cx| {
                    this.tab = if *index == 0 { Tab::Preview } else { Tab::Text };
                    cx.notify();
                }))
                .child(tabs::Tab::new().label("Vista previa"))
                .child(tabs::Tab::new().label("Texto")),
        )
        .map(|pane| match self.tab {
            Tab::Preview => pane.child(self.render_preview(cx)),
            Tab::Text => pane.child(self.render_text(file, cx)),
        })
        .child(
            h_flex()
                .justify_between()
                .child(
                    Button::new("open")
                        .outline()
                        .icon(IconName::ExternalLink)
                        .label("Abrir")
                        .loading(self.opening)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_selected(window, cx)),
                        ),
                )
                .child(
                    Button::new("delete")
                        .outline()
                        .danger()
                        .label("Eliminar")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)),
                        ),
                ),
        )
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = div()
            .id("preview")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().muted);

        let frame = match &self.preview {
            Loaded::Loading(percent) => frame
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    ProgressCircle::new("preview-progress")
                        .value(*percent as f32)
                        .size_10(),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("Cargando · {percent}%")),
                ),
            // The flex column sets the image height. With `size_full`, the height follows the width.
            Loaded::Image(image) => frame.flex().flex_col().p_4().child(
                img(image.clone())
                    .id("image-preview")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .object_fit(ObjectFit::Contain)
                    .test_support(),
            ),
            Loaded::Text(text) => frame.child(
                div()
                    .id("text-preview")
                    .size_full()
                    .p_3()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_sm()
                    .child(text.clone())
                    .overflow_y_scrollbar(),
            ),
            Loaded::Unavailable => frame
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(
                    "No hay vista previa para este tipo de archivo. Seleccione Abrir para ver el archivo.",
                ),
        };
        frame.test_support()
    }

    /// The extracted text of the file, page by page.
    fn render_text(&self, file: &Evidence, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let frame = div()
            .id("text-frame")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border);
        let message = |frame: Stateful<Div>, text: &str, note: Option<String>| {
            frame
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .p_4()
                .text_sm()
                .text_color(muted)
                .child(text.to_owned())
                .children(note.map(|note| div().text_xs().child(note)))
        };

        let frame = match (file.status, &self.pages) {
            (AnalysisStatus::Pending | AnalysisStatus::Analyzing, _) => message(
                frame,
                "El análisis no terminó. El texto aparece aquí cuando termine.",
                None,
            ),
            (AnalysisStatus::NotAnalyzed, _) => message(
                frame,
                "Este archivo no tiene texto para mostrar.",
                file.note.clone(),
            ),
            (AnalysisStatus::Ready, None) => frame,
            (AnalysisStatus::Ready, Some(pages))
                if pages.iter().all(|page| page.text.trim().is_empty()) =>
            {
                message(frame, "Nicolaw no encontró texto en este archivo.", None)
            }
            (AnalysisStatus::Ready, Some(pages)) => frame
                .child(
                    v_flex()
                        .id("text-pages")
                        .size_full()
                        .track_scroll(&self.text_scroll)
                        .overflow_y_scroll()
                        .p_3()
                        .gap_4()
                        .children(pages.iter().map(|page| render_page(page, muted))),
                )
                .vertical_scrollbar(&self.text_scroll),
        };
        frame.test_support()
    }
}

fn render_page(page: &Page, muted: Hsla) -> impl IntoElement {
    let text = page.text.trim();
    v_flex()
        .gap_2()
        .child(
            h_flex()
                .id(("page", page.number as u64))
                .gap_2()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .child(format!("Página {}", page.number)),
                )
                .when(page.source == PageSource::Ocr, |header| {
                    header.child(Tag::info().small().child("OCR"))
                })
                .test_support(),
        )
        .child(if text.is_empty() {
            div()
                .text_sm()
                .text_color(muted)
                .child("Esta página no tiene texto.")
        } else {
            div().text_sm().child(text.to_owned())
        })
}

fn status_label(status: AnalysisStatus) -> &'static str {
    match status {
        AnalysisStatus::Pending => "Pendiente",
        AnalysisStatus::Analyzing => "Analizando",
        AnalysisStatus::Ready => "Listo",
        AnalysisStatus::NotAnalyzed => "Sin analizar",
    }
}

fn status_tag(status: AnalysisStatus) -> Tag {
    let tag = match status {
        AnalysisStatus::Pending => Tag::secondary(),
        AnalysisStatus::Analyzing => Tag::info(),
        AnalysisStatus::Ready => Tag::success(),
        AnalysisStatus::NotAnalyzed => Tag::warning(),
    };
    tag.small().child(status_label(status))
}

impl Render for FilesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = |id: &'static str, icon: IconName, title: &'static str, text: &'static str| {
            div()
                .id(id)
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Empty::new().header(
                        EmptyHeader::new()
                            .media(
                                EmptyMedia::new()
                                    .with_variant(EmptyMediaVariant::Icon)
                                    .child(Icon::new(icon)),
                            )
                            .title(EmptyTitle::new().child(title))
                            .description(EmptyDescription::new().child(text)),
                    ),
                )
                .test_support()
                .into_any_element()
        };
        let body = if self.list_loaded && self.case_id.is_none() {
            empty(
                "no-cases",
                IconName::FolderOpen,
                "No hay casos",
                "Cree un caso para agregar pruebas. Seleccione Nuevo caso.",
            )
        } else if self.list_loaded
            && self.files.is_empty()
            && self.queue.is_empty()
            && self.query.is_empty()
        {
            empty(
                "empty-state",
                IconName::Inbox,
                "No hay pruebas",
                "Agregue archivos a este caso. También puede soltar archivos en esta ventana.",
            )
        } else {
            h_flex()
                .id("body")
                .flex_1()
                .min_h_0()
                .items_start()
                .child(self.render_list(cx))
                .child(self.render_detail(cx))
                .test_support()
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.add_files(paths.paths().to_vec(), window, cx)
            }))
            .child(self.render_header(cx))
            .child(body)
    }
}

fn import_message(error: ImportError, name: &str) -> String {
    match error {
        ImportError::TooLarge => {
            format!("\"{name}\" pesa más de 100 MB. Nicolaw no agregó el archivo.")
        }
        ImportError::Read => format!("Nicolaw no puede leer \"{name}\"."),
        ImportError::Changed => {
            format!("\"{name}\" cambió durante la carga. Nicolaw no agregó el archivo.")
        }
        ImportError::Storage => "Nicolaw no puede guardar el archivo. Intente de nuevo.".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Duration;

    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
        WindowOptions, px, size,
    };

    use super::{FilesView, Loaded, Tab};
    use crate::extract::{Page, PageSource};
    use crate::store::{AnalysisStatus, Store};

    const WAIT: Duration = Duration::from_secs(1);

    fn open(store: Store, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<FilesView>) {
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
                cx.new(|cx| FilesView::new(store, window, cx))
            })
            .unwrap()
        })
    }

    /// A store with one case, which is the last selected case. Returns the store and the case id.
    fn store_with_case() -> (Store, i64) {
        let store = Store::open_in_memory().unwrap();
        let case = store.create_case("Caso de prueba").unwrap().id;
        store.set_setting("last_case", &case.to_string()).unwrap();
        (store, case)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nicolaw-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[gpui_kit::test]
    async fn a_new_case_is_created_selected_and_remembered(cx: &mut TestAppContext) {
        let (window, view) = open(Store::open_in_memory().unwrap(), cx);
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("no-cases").is_some()
        })
        .await;

        cx.update_window(window, |_, window, cx| window.click("new-case", cx))
            .unwrap();
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("case-name").is_some()
        })
        .await;
        // The dialog slides in: let it finish, so that clicks hit the final bounds.
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.click("case-name", cx);
            window.input("Caso Pérez", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.click("create-case", cx))
            .unwrap();
        cx.wait_for(window, WAIT, |_, cx| view.read(cx).case_id.is_some())
            .await;

        cx.update(|cx| {
            let view = view.read(cx);
            let store = view.store.lock().unwrap();
            let cases = store.cases().unwrap();
            assert_eq!(cases.len(), 1);
            assert_eq!(cases[0].name, "Caso Pérez");
            assert_eq!(view.case_id, Some(cases[0].id));
            assert_eq!(
                store.setting("last_case").unwrap(),
                Some(cases[0].id.to_string())
            );
        });
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("case-name").is_none() && window.try_find("empty-state").is_some()
        })
        .await;
    }

    #[gpui_kit::test]
    async fn the_last_selected_case_opens_at_start(cx: &mut TestAppContext) {
        let store = Store::open_in_memory().unwrap();
        let first = store.create_case("Primero").unwrap().id;
        store.create_case("Segundo").unwrap();
        store.set_setting("last_case", &first.to_string()).unwrap();
        let (window, view) = open(store, cx);

        cx.wait_for(window, WAIT, |_, cx| view.read(cx).list_loaded)
            .await;

        cx.update(|cx| assert_eq!(view.read(cx).case_id, Some(first)));
    }

    #[gpui_kit::test]
    async fn an_imported_file_goes_from_pending_to_analyzing_to_ready(cx: &mut TestAppContext) {
        let path = temp_dir("status").join("acta.txt");
        fs::write(&path, "La declaración del acusado.").unwrap();
        let (store, _) = store_with_case();
        let (window, view) = open(store, cx);
        cx.run_until_parked();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let record = seen.clone();
        let _observer = cx.update(|cx| {
            cx.observe(&view, move |view, cx| {
                if let Some(file) = view.read(cx).files.first() {
                    let mut seen = record.borrow_mut();
                    if seen.last() != Some(&file.status) {
                        seen.push(file.status);
                    }
                }
            })
        });

        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| view.add_files(vec![path], window, cx));
        })
        .unwrap();
        cx.wait_for(window, WAIT, |_, cx| {
            view.read(cx)
                .files
                .first()
                .is_some_and(|file| file.status == AnalysisStatus::Ready)
        })
        .await;

        assert_eq!(
            *seen.borrow(),
            [
                AnalysisStatus::Pending,
                AnalysisStatus::Analyzing,
                AnalysisStatus::Ready
            ]
        );
        let id = cx.update(|cx| view.read(cx).files[0].id);
        cx.update_window(window, |_, window, _| {
            assert!(window.find(("status", id as u64)).visible());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    async fn search_shows_hits_and_a_hit_opens_the_text_at_its_page(cx: &mut TestAppContext) {
        let path = temp_dir("search").join("acta.txt");
        fs::write(&path, "x").unwrap();
        let (store, case) = store_with_case();
        let id = store.import(case, &path, |_, _| {}).unwrap().id;
        let page = |number: u32, text: &str| Page {
            number,
            text: text.into(),
            source: PageSource::Text,
        };
        store
            .save_analysis(
                id,
                &[
                    page(1, "Primera página."),
                    page(2, "La declaración del acusado."),
                ],
            )
            .unwrap();
        let chunk = store.search(case, "declaracion", 1).unwrap()[0].chunk_id;
        let (window, view) = open(store, cx);
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("file", id as u64)).is_some()
        })
        .await;

        cx.update_window(window, |_, window, cx| {
            window.click("search", cx);
            window.input("declaracion", cx);
        })
        .unwrap();
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("hit", chunk as u64)).is_some()
        })
        .await;
        cx.update_window(window, |_, window, cx| {
            assert!(window.try_find(("file", id as u64)).is_none());
            window.click(("hit", chunk as u64), cx);
        })
        .unwrap();
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find(("page", 2u64)).is_some()
        })
        .await;

        cx.update(|cx| {
            let view = view.read(cx);
            assert_eq!(view.selected, Some(id));
            assert_eq!(view.tab, Tab::Text);
        });
        cx.update_window(window, |_, window, _| {
            assert!(window.find(("page", 2u64)).visible());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    async fn select_shows_the_file_and_delete_removes_it(cx: &mut TestAppContext) {
        let path = temp_dir("view").join("hello.txt");
        fs::write(&path, "hello").unwrap();
        let (store, case) = store_with_case();
        let id = store.import(case, &path, |_, _| {}).unwrap().id;
        let (window, view) = open(store, cx);
        let row = ("file", id as u64);

        cx.wait_for(window, WAIT, |window, _| window.try_find(row).is_some())
            .await;
        cx.update_window(window, |_, window, cx| window.click(row, cx))
            .unwrap();
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("open").is_some_and(|open| open.visible())
        })
        .await;
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(matches!(&view.read(cx).preview, Loaded::Text(text) if text == "hello"));
        });

        cx.update_window(window, |_, window, cx| window.click("delete", cx))
            .unwrap();
        cx.wait_for(window, WAIT, |window, _| window.try_find("ok").is_some())
            .await;
        cx.update_window(window, |_, window, cx| window.click("ok", cx))
            .unwrap();
        cx.wait_for(window, WAIT, |window, _| window.try_find(row).is_none())
            .await;
        cx.update(|cx| assert!(view.read(cx).files.is_empty()));
    }

    /// A 4 x 4 PNG. Its aspect ratio is 1, so a width-fitted image is taller than the frame.
    const SQUARE_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 4, 0, 0, 0, 4, 8, 2,
        0, 0, 0, 38, 147, 9, 41, 0, 0, 0, 16, 73, 68, 65, 84, 120, 156, 99, 112, 104, 56, 0, 71,
        12, 196, 113, 0, 104, 82, 24, 1, 110, 45, 53, 125, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
        130,
    ];

    #[gpui_kit::test]
    async fn panes_and_previews_fit_the_window(cx: &mut TestAppContext) {
        let dir = temp_dir("layout");
        fs::write(dir.join("notes.txt"), "hello").unwrap();
        fs::write(dir.join("square.png"), SQUARE_PNG).unwrap();
        let (store, case) = store_with_case();
        let text = store
            .import(case, &dir.join("notes.txt"), |_, _| {})
            .unwrap()
            .id;
        let image = store
            .import(case, &dir.join("square.png"), |_, _| {})
            .unwrap()
            .id;
        let (window, _) = open(store, cx);
        let (text, image) = (("file", text as u64), ("file", image as u64));

        cx.wait_for(window, WAIT, |window, _| window.try_find(text).is_some())
            .await;
        cx.update_window(window, |_, window, cx| {
            let body = window.find("body").bounds();
            let list = window.find("list-pane").bounds();
            assert_eq!(list.size.height, body.size.height);
            assert_eq!(body.bottom(), window.viewport_size().height);

            window.click(text, cx);
        })
        .unwrap();
        cx.wait_for(window, WAIT, |window, _| window.try_find("open").is_some())
            .await;
        cx.update_window(window, |_, window, _| {
            let viewport = Bounds::new(Point::default(), window.viewport_size());
            for id in ["open", "delete"] {
                let button = window.find(id);
                assert!(button.visible(), "{id} is not visible");
                let bounds = button.bounds();
                assert!(bounds.left() >= viewport.left() && bounds.top() >= viewport.top());
                assert!(bounds.right() <= viewport.right() && bounds.bottom() <= viewport.bottom());
            }
        })
        .unwrap();

        cx.update_window(window, |_, window, cx| window.click(image, cx))
            .unwrap();
        cx.wait_for(window, WAIT, |window, _| {
            window
                .try_find("image-preview")
                .is_some_and(|image| image.bounds().size.height > px(0.))
        })
        .await;
        cx.update_window(window, |_, window, _| {
            let frame = window.find("preview").bounds();
            let image = window.find("image-preview").bounds();
            assert!(image.top() >= frame.top() && image.bottom() <= frame.bottom());
            assert!(image.left() >= frame.left() && image.right() <= frame.right());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    async fn skeleton_shows_until_the_list_arrives(cx: &mut TestAppContext) {
        let (store, _) = store_with_case();
        let (window, view) = open(store, cx);

        // The list task has not run yet: the executor runs it only when the test waits.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("list-skeleton").visible());
            assert!(window.try_find("empty-state").is_none());
        })
        .unwrap();
        cx.update(|cx| assert!(!view.read(cx).list_loaded));

        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("empty-state").is_some()
        })
        .await;
        cx.update_window(window, |_, window, _| {
            assert!(window.try_find("list-skeleton").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    async fn add_files_saves_each_file_through_the_queue(cx: &mut TestAppContext) {
        let dir = temp_dir("queue");
        let paths: Vec<_> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|name| {
                let path = dir.join(name);
                fs::write(&path, name.as_bytes()).unwrap();
                path
            })
            .collect();
        let (store, case) = store_with_case();
        let (window, view) = open(store, cx);
        cx.run_until_parked();

        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| view.add_files(paths, window, cx));
        })
        .unwrap();
        cx.wait_for(window, WAIT, |_, cx| {
            let view = view.read(cx);
            view.queue.is_empty() && view.files.len() == 3
        })
        .await;

        cx.update(|cx| {
            let view = view.read(cx);
            let ids: Vec<_> = view.files.iter().map(|file| file.id).collect();
            assert_eq!(view.store.lock().unwrap().list(case).unwrap().len(), 3);
            for id in &ids {
                assert!(view.highlighted.contains(id));
            }
        });
        cx.update_window(window, |_, window, _| {
            for id in [1u64, 2, 3] {
                assert!(window.find(("file", id)).visible());
            }
        })
        .unwrap();
    }
}
