mod analysis;
mod app;
mod extract;
mod preview;
mod store;
mod transfer;

use gpui_kit::component::Theme;
use gpui_kit::*;

use crate::app::FilesView;
use crate::store::Store;

fn main() {
    let data_dir = dirs::data_dir()
        .expect("Nicolaw cannot find the data folder")
        .join("nicolaw");
    std::fs::create_dir_all(&data_dir).expect("Nicolaw cannot create the data folder");
    let store =
        Store::open(&data_dir.join("nicolaw.db")).expect("Nicolaw cannot open the database");

    application().with_assets(assets::Assets).run(|cx| {
        init(cx);
        Theme::sync_system_appearance(None, cx);
        // Without a menu bar, closing the window is the way to quit.
        cx.on_window_closed(|cx, _| cx.quit()).detach();

        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Nicolaw".into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1100.), px(720.)),
                cx,
            ))),
            window_min_size: Some(size(px(800.), px(520.))),
            ..Default::default()
        };
        open_window(options, cx, |window, cx| {
            cx.new(|cx| FilesView::new(store, window, cx))
        })
        .expect("Nicolaw cannot open the window");
    });
}
