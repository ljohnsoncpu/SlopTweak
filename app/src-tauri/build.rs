fn main() {
    // App commands need an explicit permission, granted only to the local
    // `main` window (capabilities/main.json). The remote Invoke window has no
    // capability, so it can call nothing.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "get_snapshot",
            "refresh_catalog",
            "estimate",
            "start_session",
            "stop_session",
            "open_invoke",
            "dismiss",
            "set_secret",
            "check_credit",
            "save_settings",
            "select_model",
            "pick_output_folder",
            "reset_output_folder",
            "open_output_folder",
            "show_tutorial",
            "add_lora",
            "remove_lora",
            "set_lora_enabled",
            "open_link",
            "scan_orphans",
            "destroy_orphan",
            "reattach_orphan",
            "confirm_close",
            "copy_diagnostics",
            "check_update",
            "install_update",
        ]),
    ))
    .expect("tauri build");
}
