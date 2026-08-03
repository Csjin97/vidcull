pub use imp::Autostart;

impl Autostart {
    pub fn sync(&self, enabled: bool, name: &str, command: &str) -> vidcull_core::Result<()> {
        if enabled {
            self.enable(name, command)
        } else {
            self.disable(name)
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::io;

    use vidcull_core::{Error, Result};
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

    const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub struct Autostart {
        subkey: String,
    }

    impl Autostart {
        #[must_use]
        pub fn system() -> Self {
            Self {
                subkey: RUN_SUBKEY.to_owned(),
            }
        }

        #[must_use]
        pub fn at_registry_subkey(subkey: impl Into<String>) -> Self {
            Self {
                subkey: subkey.into(),
            }
        }

        pub fn enable(&self, name: &str, command: &str) -> Result<()> {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let (key, _) = hkcu.create_subkey(&self.subkey).map_err(Error::Io)?;
            key.set_value(name, &command.to_owned())
                .map_err(Error::Io)?;
            Ok(())
        }

        pub fn disable(&self, name: &str) -> Result<()> {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let key = match hkcu.open_subkey_with_flags(&self.subkey, KEY_READ | KEY_WRITE) {
                Ok(key) => key,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(Error::Io(err)),
            };
            match key.delete_value(name) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(Error::Io(err)),
            }
        }

        pub fn is_enabled(&self, name: &str) -> Result<bool> {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let key = match hkcu.open_subkey(&self.subkey) {
                Ok(key) => key,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(err) => return Err(Error::Io(err)),
            };
            match key.get_value::<String, _>(name) {
                Ok(_) => Ok(true),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(err) => Err(Error::Io(err)),
            }
        }
    }
}

#[cfg(all(unix, not(windows)))]
mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};

    use vidcull_core::{Error, Result};

    pub struct Autostart {
        dir: PathBuf,
    }

    impl Autostart {
        #[must_use]
        pub fn system() -> Self {
            Self { dir: default_dir() }
        }

        #[must_use]
        pub fn at_dir(dir: impl Into<PathBuf>) -> Self {
            Self { dir: dir.into() }
        }

        fn entry_path(&self, name: &str) -> PathBuf {
            self.dir.join(format!("{name}.{ENTRY_EXT}"))
        }

        pub fn enable(&self, name: &str, command: &str) -> Result<()> {
            fs::create_dir_all(&self.dir).map_err(Error::Io)?;
            fs::write(self.entry_path(name), entry_contents(name, command)).map_err(Error::Io)?;
            Ok(())
        }

        pub fn disable(&self, name: &str) -> Result<()> {
            match fs::remove_file(self.entry_path(name)) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(Error::Io(err)),
            }
        }

        pub fn is_enabled(&self, name: &str) -> Result<bool> {
            Ok(self.entry_path(name).is_file())
        }
    }

    #[cfg(target_os = "macos")]
    const ENTRY_EXT: &str = "plist";
    #[cfg(not(target_os = "macos"))]
    const ENTRY_EXT: &str = "desktop";

    fn default_dir() -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            home().join("Library").join("LaunchAgents")
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".config"))
                .join("autostart")
        }
    }

    fn home() -> PathBuf {
        std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
    }

    #[cfg(target_os = "macos")]
    fn entry_contents(name: &str, command: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>Label</key>\n\t<string>{name}</string>\n\
             \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{command}</string>\n\t</array>\n\
             \t<key>RunAtLoad</key>\n\t<true/>\n\
             </dict>\n\
             </plist>\n"
        )
    }

    #[cfg(not(target_os = "macos"))]
    fn entry_contents(name: &str, command: &str) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={name}\n\
             Exec={command}\n\
             X-GNOME-Autostart-enabled=true\n"
        )
    }

    #[cfg(test)]
    pub(super) fn read_command(path: &Path) -> Option<String> {
        let text = fs::read_to_string(path).ok()?;
        #[cfg(target_os = "macos")]
        {
            let after = text.split("<array>").nth(1)?;
            let inner = after.split("<string>").nth(1)?;
            Some(inner.split("</string>").next()?.to_owned())
        }
        #[cfg(not(target_os = "macos"))]
        {
            text.lines()
                .find_map(|line| line.strip_prefix("Exec="))
                .map(str::to_owned)
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use vidcull_core::Result;

    pub struct Autostart;

    impl Autostart {
        #[must_use]
        pub fn system() -> Self {
            Self
        }

        pub fn enable(&self, _name: &str, _command: &str) -> Result<()> {
            Ok(())
        }

        pub fn disable(&self, _name: &str) -> Result<()> {
            Ok(())
        }

        pub fn is_enabled(&self, _name: &str) -> Result<bool> {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "vidcull";

    #[cfg(windows)]
    mod windows {
        use super::*;
        use std::sync::atomic::{AtomicU32, Ordering};
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;

        struct TempKey {
            root: String,
            full: String,
        }

        impl TempKey {
            fn new() -> Self {
                static N: AtomicU32 = AtomicU32::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                let pid = std::process::id();
                let root = format!(r"Software\vidcull-test-{pid}-{n}");
                Self {
                    full: format!(r"{root}\Run"),
                    root,
                }
            }
        }

        impl Drop for TempKey {
            fn drop(&mut self) {
                let hkcu = RegKey::predef(HKEY_CURRENT_USER);
                let _ = hkcu.delete_subkey_all(&self.root);
            }
        }

        #[test]
        fn enable_then_query_then_disable_round_trips() {
            let key = TempKey::new();
            let auto = Autostart::at_registry_subkey(key.full.clone());

            assert!(!auto.is_enabled(APP).expect("query empty"));

            auto.enable(APP, "C:/Program Files/vidcull/vidcull-daemon.exe")
                .expect("enable");
            assert!(auto.is_enabled(APP).expect("query after enable"));

            auto.disable(APP).expect("disable");
            assert!(!auto.is_enabled(APP).expect("query after disable"));
        }

        #[test]
        fn enable_records_the_command_value() {
            let key = TempKey::new();
            let auto = Autostart::at_registry_subkey(key.full.clone());
            let cmd = r#""C:\app\vidcull-daemon.exe" --background"#;
            auto.enable(APP, cmd).expect("enable");

            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let run = hkcu.open_subkey(&key.full).expect("open run key");
            let got: String = run.get_value(APP).expect("read value");
            assert_eq!(got, cmd);
        }

        #[test]
        fn disable_is_idempotent_on_a_missing_key() {
            let key = TempKey::new();
            let auto = Autostart::at_registry_subkey(key.full.clone());
            auto.disable(APP).expect("disable missing");
            assert!(!auto.is_enabled(APP).expect("query"));
        }

        #[test]
        fn sync_mirrors_the_enabled_flag() {
            let key = TempKey::new();
            let auto = Autostart::at_registry_subkey(key.full.clone());
            auto.sync(true, APP, "vidcull-daemon.exe").expect("sync on");
            assert!(auto.is_enabled(APP).expect("query on"));
            auto.sync(false, APP, "vidcull-daemon.exe")
                .expect("sync off");
            assert!(!auto.is_enabled(APP).expect("query off"));
        }
    }

    #[cfg(all(unix, not(windows)))]
    mod unix {
        use super::*;

        #[test]
        fn enable_then_query_then_disable_round_trips() {
            let dir = tempfile::tempdir().expect("tempdir");
            let auto = Autostart::at_dir(dir.path());

            assert!(!auto.is_enabled(APP).expect("query empty"));
            auto.enable(APP, "/usr/bin/vidcull-daemon").expect("enable");
            assert!(auto.is_enabled(APP).expect("query after enable"));
            auto.disable(APP).expect("disable");
            assert!(!auto.is_enabled(APP).expect("query after disable"));
        }

        #[test]
        fn enable_writes_the_command_into_the_entry() {
            let dir = tempfile::tempdir().expect("tempdir");
            let auto = Autostart::at_dir(dir.path());
            let cmd = "/opt/vidcull/vidcull-daemon --background";
            auto.enable(APP, cmd).expect("enable");

            let ext = if cfg!(target_os = "macos") {
                "plist"
            } else {
                "desktop"
            };
            let entry = dir.path().join(format!("{APP}.{ext}"));
            assert!(entry.is_file());
            assert_eq!(imp::read_command(&entry).as_deref(), Some(cmd));
        }

        #[test]
        fn disable_is_idempotent_on_a_missing_entry() {
            let dir = tempfile::tempdir().expect("tempdir");
            let auto = Autostart::at_dir(dir.path());
            auto.disable(APP).expect("disable missing");
            assert!(!auto.is_enabled(APP).expect("query"));
        }

        #[test]
        fn sync_mirrors_the_enabled_flag() {
            let dir = tempfile::tempdir().expect("tempdir");
            let auto = Autostart::at_dir(dir.path());
            auto.sync(true, APP, "/usr/bin/vidcull-daemon")
                .expect("sync on");
            assert!(auto.is_enabled(APP).expect("query on"));
            auto.sync(false, APP, "/usr/bin/vidcull-daemon")
                .expect("sync off");
            assert!(!auto.is_enabled(APP).expect("query off"));
        }
    }
}
