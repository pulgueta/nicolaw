use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::attachment::{
    Attachment, AttachmentContent, AttachmentDescription, AttachmentMedia, AttachmentStatus,
    AttachmentTitle,
};
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::description_list::DescriptionList;
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::notification::NotificationType;
use gpui_kit::component::progress::{Progress, ProgressCircle};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::preview::{self, Preview};
use crate::store::{FileEntry, ImportError, Store, file_name};
use crate::transfer::{Queue, State};

const LIST_ERROR: &str = "Nicolaw cannot read the files.";
const DELETE_ERROR: &str = "Nicolaw cannot delete the file. Try again.";

/// The preview of the selected file, after it is loaded from the store.
enum Loaded {
    /// Percent of the content that is read.
    Loading(u8),
    Image(Arc<Image>),
    Text(SharedString),
    Unavailable,
}

pub struct FilesView {
    store: Arc<Mutex<Store>>,
    files: Vec<FileEntry>,
    selected: Option<i64>,
    preview: Loaded,
    preview_task: Option<Task<()>>,
    /// False until the first list result arrives.
    list_loaded: bool,
    queue: Queue,
    /// True while a worker saves the files in the queue.
    saving: bool,
    /// Files saved in this session. Their rows show a highlight one time.
    highlighted: HashSet<i64>,
    /// True while Open prepares the file.
    opening: bool,
}

impl FilesView {
    pub fn new(store: Store, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = Arc::new(Mutex::new(store));
        let list_store = store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let files = cx
                .background_spawn(async move {
                    list_store.lock().unwrap().list().map_err(|_| LIST_ERROR)
                })
                .await;
            this.update_in(cx, |this, window, cx| this.set_files(files, window, cx))
                .ok();
        })
        .detach();

        Self {
            store,
            files: Vec::new(),
            selected: None,
            preview: Loaded::Unavailable,
            preview_task: None,
            list_loaded: false,
            queue: Queue::default(),
            saving: false,
            highlighted: HashSet::new(),
            opening: false,
        }
    }

    fn set_files(
        &mut self,
        files: Result<Vec<FileEntry>, &'static str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match files {
            Ok(files) => self.files = files,
            Err(message) => window.push_notification((NotificationType::Error, message), cx),
        }
        self.list_loaded = true;
        if self.selected_entry().is_none() {
            self.selected = None;
            self.preview_task = None;
        }
        cx.notify();
    }

    fn selected_entry(&self) -> Option<&FileEntry> {
        let id = self.selected?;
        self.files.iter().find(|file| file.id == id)
    }

    fn prompt_for_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Add".into()),
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
                    next
                });
                let Ok(Some((_, path))) = next else {
                    break;
                };

                let (sender, receiver) = async_channel::bounded(1);
                let store = store.clone();
                // The store is locked for one file, so that a preview can load between files.
                let job = cx.background_spawn(async move {
                    let store = store.lock().unwrap();
                    let entry = store.import(&path, |done, _| {
                        sender.try_send(done).ok();
                    });
                    (entry, store.list().map_err(|_| LIST_ERROR))
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
                    this.finish_transfer(entry, files, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    fn finish_transfer(
        &mut self,
        entry: Result<FileEntry, ImportError>,
        files: Result<Vec<FileEntry>, &'static str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match entry {
            Ok(entry) => {
                self.queue.complete();
                self.highlighted.insert(entry.id);
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
                1 => "1 file added.".to_owned(),
                n => format!("{n} files added."),
            };
            window.push_notification((NotificationType::Success, message), cx);
        }
        self.set_files(files, window, cx);
    }

    fn select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected == Some(id) {
            return;
        }
        self.selected = Some(id);
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.preview = Loaded::Loading(0);
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
                    let message = format!("Nicolaw cannot read \"{name}\".");
                    window.push_notification((NotificationType::Error, message), cx);
                    Loaded::Unavailable
                });
                cx.notify();
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
                        let message = format!("Nicolaw cannot open \"{name}\".");
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
                .title("Delete this file?")
                .description(SharedString::from(format!(
                    "\"{}\" will be removed from Nicolaw. You cannot undo this action.",
                    entry.name
                )))
                .confirm()
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("Cancel")
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| this.delete(id, window, cx)).ok();
                    true
                })
        });
    }

    fn delete(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let files = cx
                .background_spawn(async move {
                    let store = store.lock().unwrap();
                    store.delete(id).map_err(|_| DELETE_ERROR)?;
                    store.list().map_err(|_| LIST_ERROR)
                })
                .await;
            this.update_in(cx, |this, window, cx| this.set_files(files, window, cx))
                .ok();
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = (!self.files.is_empty()).then(|| self.files.len().to_string());
        let summary = self
            .queue
            .position()
            .map(|(n, total)| format!("Saving {n} of {total}"));
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
                    .items_baseline()
                    .child(div().text_lg().font_semibold().child("Files"))
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
                    .label("Add files")
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
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let highlight = cx.theme().primary;
        let transfers = self.queue.transfers().iter().map(|transfer| {
            let (status, description) = match transfer.state {
                State::Waiting => (AttachmentStatus::Pending, "Waiting"),
                // The component adds " · <n>%" to the description.
                State::Saving => (AttachmentStatus::Uploading, "Saving"),
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
        });
        let rows = self.files.iter().map(|file| {
            let id = file.id;
            let row = ListItem::new(("file", id as u64))
                .rounded(radius)
                .py_2()
                .selected(self.selected == Some(id))
                .accessibility_label(file.name.clone())
                .on_click(cx.listener(move |this, _, window, cx| this.select(id, window, cx)))
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(div().text_sm().truncate().child(file.name.clone()))
                        .child(div().text_xs().text_color(muted).child(format!(
                            "{} · {}",
                            preview::type_label(&file.name),
                            preview::format_size(file.size)
                        ))),
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
        });
        let rows: Vec<_> = rows.collect();

        div()
            .id("list-pane")
            .flex_none()
            .w(px(320.))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .id("file-list")
                    .size_full()
                    .p_2()
                    .gap_1()
                    .map(|list| {
                        if self.list_loaded {
                            list.children(transfers).children(rows)
                        } else {
                            list.child(self.render_skeleton(cx))
                        }
                    })
                    .overflow_y_scrollbar(),
            )
            .test_support()
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
                .child("Select a file to review it.");
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
                .label_width(px(80.))
                .item("Type", file.mime.clone(), 1)
                .item("Size", preview::format_size(file.size), 1)
                .item("Added", file.added_at.clone(), 1),
        )
        .child(self.render_preview(cx))
        .child(
            h_flex()
                .justify_between()
                .child(
                    Button::new("open")
                        .outline()
                        .icon(IconName::ExternalLink)
                        .label("Open")
                        .loading(self.opening)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_selected(window, cx)),
                        ),
                )
                .child(
                    Button::new("delete")
                        .outline()
                        .danger()
                        .label("Delete")
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
                        .child(format!("Loading · {percent}%")),
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
                .child("No preview is available for this file type. Select Open to see the file."),
        };
        frame.test_support()
    }
}

impl Render for FilesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.list_loaded && self.files.is_empty() && self.queue.is_empty() {
            div()
                .id("empty-state")
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
                                    .child(Icon::new(IconName::Inbox)),
                            )
                            .title(EmptyTitle::new().child("No files"))
                            .description(EmptyDescription::new().child(
                                "Add files to review them. You can also drop files in this window.",
                            )),
                    ),
                )
                .test_support()
                .into_any_element()
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
            format!("\"{name}\" is larger than 100 MB. The file was not added.")
        }
        ImportError::Read => format!("Nicolaw cannot read \"{name}\"."),
        ImportError::Storage => "Nicolaw cannot save the file. Try again.".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
        WindowOptions, px, size,
    };

    use super::{FilesView, Loaded};
    use crate::store::Store;

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

    #[gpui_kit::test]
    async fn select_shows_the_file_and_delete_removes_it(cx: &mut TestAppContext) {
        let path = std::env::temp_dir().join(format!("nicolaw-view-{}.txt", std::process::id()));
        fs::write(&path, "hello").unwrap();
        let store = Store::open_in_memory().unwrap();
        let id = store.import(&path, |_, _| {}).unwrap().id;
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
        let dir = std::env::temp_dir().join(format!("nicolaw-layout-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "hello").unwrap();
        fs::write(dir.join("square.png"), SQUARE_PNG).unwrap();
        let store = Store::open_in_memory().unwrap();
        let text = store.import(&dir.join("notes.txt"), |_, _| {}).unwrap().id;
        let image = store.import(&dir.join("square.png"), |_, _| {}).unwrap().id;
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
        let (window, view) = open(Store::open_in_memory().unwrap(), cx);

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
        let dir = std::env::temp_dir().join(format!("nicolaw-queue-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let paths: Vec<_> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|name| {
                let path = dir.join(name);
                fs::write(&path, name.as_bytes()).unwrap();
                path
            })
            .collect();
        let (window, view) = open(Store::open_in_memory().unwrap(), cx);
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
            assert_eq!(view.store.lock().unwrap().list().unwrap().len(), 3);
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
