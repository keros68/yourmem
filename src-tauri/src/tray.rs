use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    App, AppHandle, Emitter, Manager,
};

pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub fn setup(app: &mut App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开 yourmem", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    TrayIconBuilder::with_id("yourmem")
        .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/128x128.png"))?)
        .tooltip("yourmem · 后台采集，每分钟检查一次")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;

    // A single sequential worker survives window hiding. The shared ingest entry
    // point handles disabled sources, custom roots and the cross-process lock.
    // Delay the first run so the first-launch screen can render before collection.
    // The data home is resolved on every pass: the first-run wizard and local
    // cleanup change it while the app is running, and an unconfigured home must
    // not be created by collection.
    // After collection it notifies the window so the open page refreshes, then
    // runs whatever maintenance is due (snapshots, self-check).
    let handle = app.handle().clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        let home = yourmem::data_home();
        if yourmem::is_first_run(&home) {
            continue;
        }
        match yourmem::ingest::import_defaults(&home) {
            Ok(o) if o.messages_added > 0 || o.memory_revisions_added > 0 => {
                let _ = handle.emit("collected", yourmem::ingest::outcome_json(&o));
            }
            Ok(_) => {}
            // 导入锁被占说明有人正在写库，维护留到下一轮
            Err(e) => {
                eprintln!("yourmem background import: {e:#}");
                continue;
            }
        }
        match yourmem::maintenance::run_due(&home) {
            Ok(r) if r["ran"].as_array().is_some_and(|a| !a.is_empty()) => {
                let _ = handle.emit("maintenance", yourmem::maintenance::status(&home));
            }
            Ok(_) => {}
            Err(e) => eprintln!("yourmem maintenance: {e:#}"),
        }
    });
    Ok(())
}
