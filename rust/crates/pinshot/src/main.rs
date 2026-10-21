//! PinShot binary. Release Windows builds hide the console (`windows_subsystem`);
//! `--debug` / `PINSHOT_DEBUG` attach one and also log to the data dir.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    // Covers normal returns from settings, installer helpers and forwarded IPC.
    // Fatal process::exit paths flush in report_runtime_error instead.
    struct FlushDiagnostics;
    impl Drop for FlushDiagnostics {
        fn drop(&mut self) { pinshot::flush_diagnostics(); }
    }
    let _diagnostics = FlushDiagnostics;
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Run before the GUI/single-instance path so MCP also works with a resident app.
    if let Some(i) = args.iter().position(|a| a == "--capture-to") {
        pinshot::init_diagnostics(&args);
        let result = args.get(i + 1)
            .filter(|p| !p.starts_with("--"))
            .ok_or_else(|| "--capture-to requires an output PNG path".to_string())
            .and_then(|p| pinshot::capture_to_file(std::path::Path::new(p)));
        if let Err(e) = result {
            pinshot::report_runtime_error("capture-to", &e);
            std::process::exit(1);
        }
        return;
    }
    if args.iter().any(|a| a == "--settings-window") {
        pinshot::run_preferences();
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--quit-for-install") {
        pinshot::init_diagnostics(&args);
        let install_dir = args.get(i + 1).map(std::path::Path::new);
        if let Err(e) = pinshot::update_run::quit_for_install(install_dir) {
            pinshot::report_runtime_error("installer: quit-for-install", &e.to_string());
            std::process::exit(1);
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--install-after-quit") {
        if let Some(path) = args.get(i + 1) {
            pinshot::init_diagnostics(&args);
            pinshot::logutil_ensure();
            if let Err(e) = pinshot::update_run::run_install_after_quit(std::path::Path::new(path))
            {
                pinshot::report_runtime_error("installer: install-after-quit", &e.to_string());
                std::process::exit(1);
            }
            return;
        }
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        pinshot::logutil_ensure();
        println!(
            "PinShot {} ({})",
            pinshot_core::VERSION,
            pinshot::platform_tag()
        );
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        pinshot::logutil_ensure();
        println!(
            "PinShot {} ({})\n  --capture-to <output.png>  --snip  --pin-clipboard  --prefs  --translate  --debug  --version\n  --quit-for-install  --install-after-quit <setup.exe>",
            pinshot_core::VERSION,
            pinshot::platform_tag()
        );
        return;
    }

    if let Err(e) = pinshot::run(args) {
        pinshot::report_runtime_error("app: fatal startup or event loop failure", &e.to_string());
        std::process::exit(1);
    }
}
