fn main() {
    // App commands need an explicit permission, granted only to the local
    // `main` window (capabilities/main.json). The remote Invoke window has no
    // capability, so it can call nothing.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "get_snapshot",
            "start_session",
            "stop_session",
            "open_invoke",
            "dismiss",
            "set_secret",
            "check_credit",
            "scan_orphans",
            "destroy_orphan",
            "reattach_orphan",
            "confirm_close",
        ]),
    ))
    .expect("tauri build");
}
