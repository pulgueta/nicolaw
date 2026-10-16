# Feature: file-upload-review

Locator: `odd/tasks/file-upload-review.md`
Engram mirror: `odd/file-upload-review/tasks`

## Objective

A basic desktop window where the user adds files of all types and reviews them. All data stays in one local SQLite database.

## Problem

The repository is a bare scaffold (`src/main.rs` prints "Hello, world!"). There is no UI and no storage.

## Why

The user asked for a basic interface to upload and review files, built with GPUI Kit, with local SQLite storage.

## Assumptions

- "fails" in the request means "files".
- "Upload" means: copy a file from the local disk into the database. There is no network.
- "Review" means: see the list of files, select one file, read its details and preview.
- File content is stored as a BLOB in the database, not as a path.

## Scope

In scope:

- Add files with the native file dialog and with drag and drop.
- List of stored files (name, type, size, date).
- Detail pane for the selected file: details, preview, Open, Delete.
- Preview: images and UTF-8 text. Other types show a message.
- Open: write the content to a temporary file and open it with the system application.
- Delete with a confirmation dialog.
- Empty states and error notifications.

Out of scope:

- Search, sort, tags, review status, notes, rename.
- Network sync, accounts, encryption.
- Custom fonts, icons, or image assets.

## Constraints

- Framework: `gpui-kit = "0.7"` (docs: https://gpui-kit.com/llms.txt). Use components from `gpui_kit::component` and theme tokens. No hard-coded colors.
- Storage: `rusqlite` with the `bundled` feature.
- Database path: `<data_dir>/nicolaw/nicolaw.db` (`dirs::data_dir()`).
- Maximum file size: 100 MB for each file. Larger files are rejected with a message.
- File reads and database work run off the UI thread.
- UI copy: ASD-STE100 Simplified Technical English. Short sentences, active voice.
- Simplicity first. No speculative abstractions.

## Design

Window: title "Nicolaw", 1100 x 720, minimum 800 x 520.

```
+--------------------------------------------------------------+
| Files  (3)                                     [ Add files ] |
+----------------------+---------------------------------------+
| > report.pdf         | report.pdf                            |
|   PDF - 2.4 MB       | Type   application/pdf                |
|   photo.png          | Size   2.4 MB                         |
|   PNG - 310 KB       | Added  2026-09-29 15:48               |
|   notes.txt          |                                       |
|   TXT - 2 KB         | [ preview area ]                      |
|                      |                                       |
|                      | [ Open ]                  [ Delete ]  |
+----------------------+---------------------------------------+
```

- Left pane: fixed width (about 320 px), scrollable list, selected row is highlighted.
- Right pane: details (DescriptionList), preview, actions.
- The full window accepts dropped files.

Schema:

```sql
CREATE TABLE IF NOT EXISTS files (
    id       INTEGER PRIMARY KEY,
    name     TEXT    NOT NULL,
    mime     TEXT    NOT NULL,
    size     INTEGER NOT NULL,
    added_at INTEGER NOT NULL, -- Unix seconds
    content  BLOB    NOT NULL
);
```

The list query does not read `content`.

Modules:

- `src/main.rs`: application start and window.
- `src/store.rs`: SQLite store and its tests.
- `src/preview.rs`: preview classification (image, text, none), size format, and tests.
- `src/app.rs`: the main view.

UI copy:

| Location | Text |
| --- | --- |
| Header title | Files |
| Primary button | Add files |
| Empty list title | No files |
| Empty list description | Add files to review them. You can also drop files in this window. |
| No selection | Select a file to review it. |
| No preview | No preview is available for this file type. Select Open to see the file. |
| Open button | Open |
| Delete button | Delete |
| Delete dialog title | Delete this file? |
| Delete dialog description | "<name>" will be removed from Nicolaw. You cannot undo this action. |
| Delete dialog confirm | Delete |
| Delete dialog cancel | Cancel |
| Added notification | 1 file added. / <n> files added. |
| Too large error | "<name>" is larger than 100 MB. The file was not added. |
| Read error | Nicolaw cannot read "<name>". |
| Storage error | Nicolaw cannot save the file. Try again. |

## TDD

- Mode: enabled (strict).
- Source: session configuration ("Strict TDD Mode: enabled").
- Runner: `cargo test`.
- Order: observed RED, then GREEN, then REFACTOR.

## Review

- Receipt-driven development: off (decided by global). No native review runs.

## Delivery

- Strategy: `single-pr`.
- Forecast: about 600 to 800 authored lines. This is above the 400-line heuristic because a first UI needs the store, the view, and tests together.
- Commits: not created. The user rule is "do not commit unless explicitly asked". The work stays in the working tree.

## Tasks

- [x] T1 Storage: `store.rs` with open and migrate, add, list, read content, delete, and the 100 MB limit. Tests use an in-memory database.
- [x] T2 Preview logic: `preview.rs` with kind classification and size format. Unit tests.
- [ ] T3 UI shell: window, header, file list, empty state, add with file dialog and drag and drop. (Code done. Observed: window, header, empty state, list rows, list scroll, list layout in offscreen images. Pending manual check: file dialog, drag and drop from the desktop.)
- [ ] T4 Detail pane: details, preview, Open, Delete with confirmation, notifications. (Code done. Observed: select, text preview and scroll, image preview, no-preview message, long name, Delete dialog, detail layout at 1100 x 720 and 800 x 520 in offscreen images. Pending manual check: Open, notifications.)
- [x] T5 Verification: all checks pass, and the application starts.
- [x] T6 Store progress: `import` and `content` move the data in blocks and report the progress (bytes done, bytes total). `import` is atomic. Tests first.
- [x] T7 Transfer queue: `transfer.rs` with the queue states and the progress calculation. Pure logic. Tests first.
- [ ] T8 Save progress UI: transfer rows in the list, header summary and bar, highlight of a new row. (Code done. Observed: transfer rows, header summary and bar, highlight and its end, list refresh after each file, 3 files through the queue. Pending: error and success notifications in a visible window.)
- [ ] T9 Load progress UI: list skeleton at start, preview progress, loading state of the Open button. (Code done. Observed: skeleton before the list and empty state after a list with 0 files (headless test), preview at 42% and Open loading state (offscreen images). Pending: Open ignores a second click, seen in a visible window.)
- [x] T10 Verification: all checks pass, and offscreen images show each new state.

## Change 2: progress interface (2026-09-29)

Request: "Show a progress interface to the user with an animated preview of the files being saved and loaded."

### Assumptions

- "Saved" means: the application writes a file into the database.
- "Loaded" means: the application reads the file list at start, and reads the content of a file for the preview or for Open.
- The progress is real. It comes from the bytes that moved. The application adds no artificial delay to the work.
- A small file can complete in a few milliseconds. The highlight of the new row gives visible feedback in that case.

### Design

Store:

- `import` inserts a row with `zeroblob(size)`, then copies the file into the BLOB in blocks of 256 KB. The insert and the copy are in one transaction. A failed import leaves no row.
- `content` reads the BLOB in blocks of 256 KB.
- The two functions call a progress function with (bytes done, bytes total) after each block.
- `rusqlite` feature `blob` is necessary.

Transfer queue (`src/transfer.rs`, pure logic):

- One transfer for each file to save: name, size, state (`Waiting`, `Saving`), bytes done.
- Files that the user adds during a save go to the end of the queue.
- A completed or failed transfer leaves the queue.
- The queue gives: percent for each transfer, the position "2 of 5", and the overall percent by bytes.

Save progress UI:

```
+--------------------------------------------------------------+
| Files  25   Saving 2 of 5                      [ Add files ] |
|==================------------------------------------------- |  <- overall bar
+----------------------+---------------------------------------+
| (o) video.mp4        |                                       |
|     Saving · 68%     |                                       |
| ( ) archive.zip      |                                       |
|     Waiting          |                                       |
|   report.pdf         |  <- new row, highlight fades          |
|     PDF · 2.4 MB     |                                       |
```

- Transfer rows are at the top of the list. Use the `Attachment` component with `AttachmentStatus::Uploading` and `.progress(percent)` if it fits a list row. If it does not fit, use `ListItem` with `ProgressCircle` and `Shimmer`.
- A transfer row cannot be selected.
- The header shows "Saving <n> of <total>" and a thin `Progress` bar at its bottom edge while the queue is not empty.
- When a file completes, the list updates immediately and the new row shows a highlight that fades one time.
- Errors stay as notifications (same copy as before). The success notification shows one time, when the queue becomes empty.

Load progress UI:

- At start, the list pane shows skeleton rows (`Skeleton`) until the first list arrives. The empty state shows only after the list arrives with 0 files.
- The preview frame shows a `ProgressCircle` with the real percent and the text "Loading · <n>%" while the content loads.
- The Open button shows its loading state while the application prepares the file.

Motion:

- Use the animation of the components (`Progress`, `ProgressCircle`, `Skeleton`, `Shimmer`). Use GPUI `with_animation` only for the highlight of the new row.
- All states must stay clear with reduced motion. Do not request frames without a stop condition.
- Update the view only when a percent value changes.

### UI copy

| Location | Text |
| --- | --- |
| Transfer row, waiting | Waiting |
| Transfer row, saving | Saving · <n>% |
| Header summary | Saving <n> of <total> |
| Preview, loading | Loading · <n>% |

### Acceptance criteria

8. During a save, each file shows as a row with its state and its percent. The header shows the position and the overall progress.
9. When a file completes, its row shows in the list with a highlight. The list does not wait for the other files.
10. The user can add more files during a save. They go to the end of the queue.
11. A failed import leaves no row in the database. The other files continue.
12. At start, the list pane shows skeleton rows, not the empty state, until the list arrives.
13. During a content load, the preview frame shows the percent.
14. The UI thread does not read files and does not touch the database.

### Delivery

- Forecast for this change: about 400 to 500 authored lines.
- Commits: not created (user rule).

## Acceptance criteria

1. The user adds one or more files of any type. The files show in the list after the add.
2. The files stay after the application restarts.
3. The user selects a file and sees its name, type, size, and date.
4. An image file shows an image preview. A UTF-8 text file shows a text preview.
5. Open starts the system application for the file.
6. Delete removes the file after confirmation. The list updates.
7. A file larger than 100 MB is rejected with a message.

## Checks

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `cargo build`
- Manual: `cargo run` opens the window.

## Progress

- 2026-09-29: Feature document created. No source change yet.
- 2026-09-29: T1 done. `src/store.rs` with `Store::open`, `import` (100 MB limit, MIME from the file name), `list` (newest first, no content), `content`, `delete`. Commit: not created (user rule).
- 2026-09-29: T2 done. `src/preview.rs` with `classify` (image formats that GPUI decodes, UTF-8 text up to 64 KB, else none), `format_size`, `type_label`. Commit: not created (user rule).
- 2026-09-29: T3 and T4 code done in `src/app.rs` (`FilesView`) and `src/main.rs`. File reads and database work run on the background executor. Commit: not created (user rule).
- 2026-09-29: Accepted additions, with reason: `gpui-kit` dev-dependency with `test-support` (headless view test); `Theme::sync_system_appearance` at start (the content theme did not match the dark native title bar); quit when the window closes (the app has no menu bar, so no other way to quit).
- 2026-09-29: Decision gaps (defaults used, not in the UI copy table): "Nicolaw cannot read the files." (list error), "Nicolaw cannot delete the file. Try again." (delete error), "Nicolaw cannot open "<name>"." (open error). List order: newest first. No automatic selection after add. Text preview shows the first 64 KB. Header count shows as a muted number, hidden at 0. Row subtitle: "PDF · 2.4 MB".
- 2026-09-29: T5 done. All required commands pass. The application starts.
- 2026-09-29: Offscreen layout check (throwaway `harness = false` target with `HeadlessAppContext` and the Metal renderer, now removed). 25 files, light and dark themes. Images are in `/tmp/nicolaw-shots/`.
- 2026-09-29: Defect fixed: the image preview overflowed its frame. `Img` sets the aspect ratio of the image on its style, so with `size_full` the height followed the width (714 x 714 in a 748 x 436 frame). Fix: the image frame is a flex column and the image uses `w_full`, `flex_1`, `min_h_0`. An absolute inset box did not fix it (same 714 x 714).
- 2026-09-29: Observation IDs added with `test_support()` (no layout effect in normal builds): `body`, `list-pane`, `preview`, `image-preview`. New layout test `panes_and_previews_fit_the_window`.
- 2026-09-29: T6 done. `rusqlite` feature `blob`. `import(path, progress)` inserts `zeroblob(size)` and copies the file in blocks of 256 KB in one transaction; a failed import leaves no row. `content(id, progress)` reads in blocks of 256 KB. An empty file gives one call (0, 0). Commit: not created (user rule).
- 2026-09-29: T7 done. `src/transfer.rs`: `Queue` with `add`, `start_next`, `set_progress` (returns true only when the percent changes), `complete`, `fail`, and the readers `percent`, `position`, `overall_percent`, `succeeded`. An empty file counts as one progress unit. Commit: not created (user rule).
- 2026-09-29: T8 and T9 code done in `src/app.rs`. One worker saves the queue, one file at a time, and locks the store for each file. Progress goes to the view through an `async-channel` bounded channel (already in `Cargo.lock` at 2.5.0; GPUI does not re-export a channel). The view updates only when a percent changes. Transfer rows use `Attachment` (medium size, `w_full`). Header: "Saving <n> of <total>" and an xsmall `Progress` bar in an absolute box at the bottom edge. New rows: one-shot `with_animation` highlight, 1.5 s, keyed by the file id, only for files saved in this session. Start: 6 skeleton rows built from `ListItem` and blank text lines, so their height is equal to a file row. Preview: `ProgressCircle` and "Loading · <n>%". Open: `Button::loading` and a guard flag. Commit: not created (user rule).
- 2026-09-29: Offscreen images of the progress states in `/tmp/nicolaw-shots/progress/` (throwaway target, now removed). Defects fixed: skeleton rows were 58 px apart, file rows 64 px (fixed heights against text line heights); transfer rows were 48 px high with `small()` (now medium, 56 px).
- 2026-09-29: T10 done. All required commands pass, and the application starts.

## Verification evidence

- T1 RED (`cargo test`): `error[E0433]: cannot find type Store in this scope`, `error[E0433]: cannot find type ImportError in this scope`, `error[E0425]: cannot find value MAX_FILE_SIZE in this scope`.
- T1 GREEN (`cargo test`): `test result: ok. 7 passed; 0 failed`.
- T1 REFACTOR (unit error variants): `test result: ok. 7 passed; 0 failed`.
- T2 RED (`cargo test`): `error[E0425]: cannot find function classify in this scope` (8), `format_size` (6), `type_label` (3), `error[E0433]: cannot find type Preview in this scope` (8); `could not compile nicolaw (bin "nicolaw" test) due to 30 previous errors`.
- T2 GREEN (`cargo test`): `test result: ok. 13 passed; 0 failed`.
- View test `app::tests::select_shows_the_file_and_delete_removes_it` (written after the view, no RED): passes. Mutation check (delete call removed): `test result: FAILED. 0 passed; 1 failed`. Restored: `14 passed`.
- T5 `cargo fmt --check`: exit 0.
- T5 `cargo clippy --all-targets -- -D warnings`: exit 0, no warnings from `nicolaw`.
- T5 `cargo test`: `test result: ok. 14 passed; 0 failed`.
- T5 `cargo build`: exit 0.
- Layout test `panes_and_previews_fit_the_window`: list-pane height equals body height, body bottom equals window height, Open and Delete visible and inside the window. These assertions passed on the first run. The image assertion was RED: `assertion failed: image.top() >= frame.top() && image.bottom() <= frame.bottom()`. After the fix: GREEN, image 714 x 402 in frame 748 x 436.
- After the layout fix: `cargo fmt --check` exit 0; `cargo clippy --all-targets -- -D warnings` exit 0; `cargo test` `test result: ok. 15 passed; 0 failed`; `cargo build` exit 0.
- T6 RED (`cargo test`): `error[E0061]: this method takes 1 argument but 2 arguments were supplied` (17), `error[E0425]: cannot find value BLOCK_SIZE in this scope` (4); `could not compile nicolaw (bin "nicolaw" test) due to 21 previous errors`.
- T6 GREEN (`cargo test`): `test result: ok. 19 passed; 0 failed`.
- T7 RED (`cargo test`): `error[E0433]: cannot find type Queue in this scope` (2), `error[E0425]: cannot find type Queue in this scope`, `error[E0433]: cannot find type State in this scope` (2), `error[E0433]: cannot find type PathBuf in this scope` (4); `could not compile nicolaw (bin "nicolaw" test) due to 9 previous errors`.
- T7 GREEN (`cargo test transfer::`): `test result: ok. 9 passed; 0 failed`.
- T8/T9 view tests RED (`cargo test`): `error[E0609]: no field queue on type &FilesView`, `no field list_loaded`, `no field highlighted`; `could not compile nicolaw (bin "nicolaw" test) due to 3 previous errors`.
- T8/T9 view tests GREEN (`cargo test`): `test result: ok. 30 passed; 0 failed` (includes `skeleton_shows_until_the_list_arrives`, `add_files_saves_each_file_through_the_queue`, and the two earlier view tests).
- T10 offscreen: the Add files button has the same bounds (977, 12, 107 x 32) with and without the header summary. Highlight row pixel: (227, 227, 227) at 0.6 s, (255, 255, 255) at 1.8 s, the same as a row without highlight.
- T10 `cargo fmt --check` exit 0; `cargo clippy --all-targets -- -D warnings` exit 0; `cargo test` `test result: ok. 30 passed; 0 failed`; `cargo build` exit 0; `git status --short` shows only `.atl/`, `.gitignore`, `Cargo.lock`, `Cargo.toml`, `odd/`, `src/`.
- T10 start check (`HOME=/tmp/nicolaw-home ./target/debug/nicolaw`): alive after 8 s, stderr empty, stopped with SIGTERM.
- T5 start check (`HOME=/tmp/nicolaw-home ./target/debug/nicolaw`): process alive after 8 s, stderr empty, stopped with SIGTERM. Window capture shows the empty state. The window was occluded, so GPUI stopped its display link and later frames (seeded list) did not render in the capture.

## Next step

Manual check in a visible window: file dialog, drag and drop from the desktop, Open (and that a second click is ignored), error and success notifications, a file larger than 100 MB, a large file (for example 50 MB) to see the save progress live. Then check T3, T4, T8, and T9.
