use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub const REQUEST_ENV: &str = "HARNESS_RESTART_REQUEST";
pub const EXECUTABLE_ENV: &str = "HARNESS_EXECUTABLE";

/// A per-instance inbox accepts only a restart request, never commands or paths.
pub struct RestartControl {
    directory: PathBuf,
    pub requested: bool,
    committed: Arc<AtomicBool>,
}

impl RestartControl {
    pub fn new(committed: Arc<AtomicBool>) -> std::io::Result<Self> {
        let directory =
            std::env::temp_dir().join(format!("harness-restart-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            directory,
            requested: false,
            committed,
        })
    }

    pub fn request_path(&self) -> PathBuf {
        self.directory.join("request")
    }

    pub fn set_handoff(&mut self, committed: Arc<AtomicBool>) {
        self.committed = committed;
    }

    pub fn poll(&mut self) {
        if std::fs::remove_file(self.request_path()).is_ok() {
            self.requested = true;
        }
    }

    pub fn commit(&self) {
        self.committed.store(true, Ordering::SeqCst);
    }
}

impl Drop for RestartControl {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub fn request_from_environment() -> Result<(), String> {
    let path = std::env::var_os(REQUEST_ENV).ok_or(
        "No running Skyforge supplied a restart inbox. Use the Restart button in Skyforge.",
    )?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("Cannot request Skyforge restart: {error}")),
    }
}

pub fn executable() -> Result<PathBuf, String> {
    let path = std::env::current_exe().map_err(|e| e.to_string())?;
    if !path.is_file() {
        return Err("The Skyforge executable is missing. Rebuild it before restarting.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::metadata(&path)
            .map_err(|e| e.to_string())?
            .permissions();
        if permissions.mode() & 0o111 == 0 {
            return Err("The Skyforge executable is not executable.".into());
        }
    }
    Ok(path)
}

pub fn command(path: &Path) -> Command {
    let mut command = Command::new(path);
    // The database's last_project selects the active project. Do not replay CLI
    // flags or developer capture settings and accidentally request another restart.
    command
        .env_remove(REQUEST_ENV)
        .env_remove(EXECUTABLE_ENV)
        .env_remove("HARNESS_CAPTURE_PATH")
        .env_remove("HARNESS_CAPTURE_EXIT");
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_consumed_once_and_inbox_is_cleaned_up() {
        let committed = Arc::new(AtomicBool::new(false));
        let mut restart = RestartControl::new(committed.clone()).unwrap();
        let directory = restart.directory.clone();
        std::fs::write(restart.request_path(), "").unwrap();
        restart.poll();
        assert!(restart.requested);
        assert!(!committed.load(Ordering::SeqCst));
        restart.requested = false;
        restart.poll();
        assert!(!restart.requested);
        restart.commit();
        assert!(committed.load(Ordering::SeqCst));
        drop(restart);
        assert!(!directory.exists());
    }

    #[test]
    fn relaunch_preserves_data_environment_without_replaying_control_flags() {
        let command = command(Path::new("/tmp/Skyforge with spaces"));
        assert_eq!(command.get_program(), "/tmp/Skyforge with spaces");
        assert_eq!(command.get_args().count(), 0);
        let removed = command
            .get_envs()
            .filter_map(|(key, value)| value.is_none().then_some(key))
            .collect::<Vec<_>>();
        assert!(removed.contains(&std::ffi::OsStr::new(REQUEST_ENV)));
        assert!(removed.contains(&std::ffi::OsStr::new("HARNESS_CAPTURE_EXIT")));
        assert!(!removed.contains(&std::ffi::OsStr::new("HARNESS_DATA_DIR")));
    }
}
