use std::{
    fs::{self, File},
    io::{self, Write},
    path::PathBuf,
};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
#[path = "state_windows.rs"]
mod windows;

pub enum StateFile {
    Subscriptions,
    PendingEvents,
    Workspaces,
}

impl StateFile {
    fn name(&self) -> &'static str {
        match self {
            Self::Subscriptions => "event-subscriptions.json",
            Self::PendingEvents => "pending-events.json",
            Self::Workspaces => "trusted-workspaces.json",
        }
    }
}

pub struct StateFiles {
    directory: PathBuf,
    #[cfg(windows)]
    access: windows::StateAccess,
}

impl StateFiles {
    #[cfg(unix)]
    pub fn open(directory: PathBuf) -> io::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state directory must be private",
            ));
        }
        Ok(Self { directory })
    }

    #[cfg(windows)]
    pub fn open(directory: PathBuf) -> io::Result<Self> {
        let (directory, access) = windows::StateAccess::open(&directory)?;
        Ok(Self { directory, access })
    }

    pub fn read(&self, kind: StateFile) -> io::Result<Option<File>> {
        let path = self.directory.join(kind.name());
        #[cfg(unix)]
        let result = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() && meta.permissions().mode() & 0o077 == 0 => {
                File::open(&path)
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state file must be private",
            )),
            Err(error) => Err(error),
        };
        #[cfg(windows)]
        let result = self.access.read(&path);
        match result {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn replace(
        &self,
        kind: StateFile,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        let temporary = self.directory.join(format!(".events-{}", Uuid::new_v4()));
        #[cfg(unix)]
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        #[cfg(windows)]
        let mut file = self.access.create(&temporary)?;

        let result = (|| {
            write(&mut file)?;
            file.flush()?;
            file.sync_all()
        })();
        drop(file);

        let destination = self.directory.join(kind.name());
        let result = result.and_then(|()| {
            #[cfg(unix)]
            return {
                if matches!(kind, StateFile::Subscriptions) {
                    return fs::rename(&temporary, &destination);
                }
                let directory = File::open(&self.directory)?;
                fs::rename(&temporary, &destination)?;
                directory.sync_all()
            };
            #[cfg(windows)]
            return self.access.replace(&temporary, &destination);
        });
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}
