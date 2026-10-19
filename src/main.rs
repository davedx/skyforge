mod app;
mod delegation;
mod delegation_ipc;
mod editor;
mod local;
mod markdown;
mod repository;
mod research;
mod restart;
mod runner;
mod store;
mod theme;
mod thinker;
mod wakes;

fn main() -> eframe::Result<()> {
    let argument = std::env::args_os().nth(1);
    if argument.as_deref().is_some_and(|a| a == "--wake-tools") {
        let result = (|| -> Result<(), String> {
            let args: Vec<String> = std::env::args().skip(2).collect();
            if args.len() != 3 {
                return Err("Missing wake tool configuration".into());
            }
            let endpoint = serde_json::from_str(&args[2]).map_err(|e| e.to_string())?;
            wakes::serve(&args[0], &args[1], &endpoint)
        })();
        return result.map_err(|e| eframe::Error::AppCreation(Box::new(std::io::Error::other(e))));
    }
    if argument.as_deref().is_some_and(|a| a == "--thinker-tools") {
        let args: Vec<_> = std::env::args_os().skip(2).collect();
        if args.len() != 6 {
            return Err(eframe::Error::AppCreation(Box::new(std::io::Error::other(
                "Missing Thinker tool configuration",
            ))));
        }
        let project = args[2].to_string_lossy();
        let endpoint = serde_json::from_str::<delegation_ipc::Endpoint>(&args[5].to_string_lossy())
            .map_err(|e| eframe::Error::AppCreation(Box::new(e)))?;
        return thinker::serve(
            std::path::Path::new(&args[0]),
            std::path::Path::new(&args[1]),
            (!project.is_empty()).then_some(project.as_ref()),
            &args[3].to_string_lossy(),
            &args[4].to_string_lossy(),
            &endpoint,
        )
        .map_err(|e| eframe::Error::AppCreation(Box::new(std::io::Error::other(e))));
    }
    if argument
        .as_deref()
        .is_some_and(|a| a == "--request-restart")
    {
        return match restart::request_from_environment() {
            Ok(()) => {
                println!("Skyforge will restart when its active runs finish.");
                Ok(())
            }
            Err(error) => Err(eframe::Error::AppCreation(Box::new(std::io::Error::other(
                error,
            )))),
        };
    }
    if argument
        .as_deref()
        .is_some_and(|a| a == "--help" || a == "-h")
    {
        println!("Skyforge — native coding-agent workspace\n\nUsage: skyforge [PROJECT_DIRECTORY]\n       skyforge --request-restart\n\nWithout a directory, opens the last project or the current directory.\nInside a Skyforge agent run, --request-restart asks the GUI to restart after all runs finish.");
        return Ok(());
    }
    let project = argument.map(std::path::PathBuf::from);
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../icon.png"))
        .map_err(|e| eframe::Error::AppCreation(Box::new(e)))?;
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_icon(icon)
            .with_inner_size([1360.0, 900.0])
            .with_active(std::env::var_os("HARNESS_CAPTURE_PATH").is_none())
            .with_min_inner_size([850.0, 600.0]),
        ..Default::default()
    };
    let restart_committed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app_restart = restart_committed.clone();
    eframe::run_native(
        "Skyforge",
        options,
        Box::new(move |cc| {
            Ok(Box::new(
                app::Skyforge::new(cc, project, app_restart).map_err(std::io::Error::other)?,
            ))
        }),
    )?;
    // The GUI, SQLite connection, and workers have shut down before relaunch.
    // This executes in the GUI host, with its normal macOS permissions.
    if restart_committed.load(std::sync::atomic::Ordering::SeqCst) {
        let path = restart::executable()
            .map_err(|e| eframe::Error::AppCreation(Box::new(std::io::Error::other(e))))?;
        restart::command(&path)
            .spawn()
            .map_err(|e| eframe::Error::AppCreation(Box::new(e)))?;
    }
    Ok(())
}
