mod analysis;
mod app;
mod audio;
mod extract;
mod hearings;
mod models;
mod preview;
mod review;
mod store;
mod transfer;

use gpui_kit::component::Theme;
use gpui_kit::*;

use std::sync::Arc;

use crate::app::FilesView;
use crate::audio::native::NativeBackend;
use crate::hearings::HearingsEnv;
use crate::models::ModelDirs;
use crate::store::Store;

actions!(nicolaw, [Quit]);

/// The menu bar and Cmd+Q. The menu item, Cmd+Q, and a window close all quit through `cx.quit()`.
fn install_menus(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
    hearings::bind_keys(cx);
    cx.set_menus([Menu::new("Nicolaw").items([MenuItem::action("Salir", Quit)])]);
}

fn main() {
    let data_dir = dirs::data_dir()
        .expect("Nicolaw cannot find the data folder")
        .join("nicolaw");
    std::fs::create_dir_all(&data_dir).expect("Nicolaw cannot create the data folder");
    let store =
        Store::open(&data_dir.join("nicolaw.db")).expect("Nicolaw cannot open the database");
    // Copies that Abrir wrote in an earlier run. A failure here does not stop the app.
    app::remove_open_copies(&app::open_copies_dir()).ok();
    let hearings_env = HearingsEnv {
        backend: Arc::new(NativeBackend),
        model_dirs: ModelDirs::system(),
        data_dir: data_dir.clone(),
    };

    application().with_assets(assets::Assets).run(|cx| {
        init(cx);
        Theme::sync_system_appearance(None, cx);
        install_menus(cx);
        // Nicolaw has one window: closing it quits the app.
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
            cx.new(|cx| FilesView::new(store, hearings_env, window, cx))
        })
        .expect("Nicolaw cannot open the window");
    });
}

#[cfg(test)]
mod tests {
    use gpui_kit::{OwnedMenuItem, TestAppContext};

    use super::{Quit, install_menus};

    #[gpui_kit::test]
    fn the_menu_and_cmd_q_reach_the_quit_action(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install_menus(cx);

            let menus = cx.get_menus().unwrap();
            assert_eq!(menus.len(), 1);
            assert_eq!(menus[0].name.as_ref(), "Nicolaw");
            assert!(matches!(
                &menus[0].items[..],
                [OwnedMenuItem::Action { name, action, .. }]
                    if name == "Salir" && action.partial_eq(&Quit)
            ));
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            let keys: Vec<String> = keymap
                .bindings_for_action(&Quit)
                .flat_map(|binding| binding.keystrokes().iter().map(|key| key.unparse()))
                .collect();
            assert_eq!(keys, ["cmd-q"]);
            assert!(cx.is_action_available(&Quit));
        });
    }
}
