pub const HIDDEN_FLAG: &str = "--hidden";

#[must_use]
pub fn launched_hidden() -> bool {
    std::env::args().any(|arg| arg == HIDDEN_FLAG)
}

pub fn sync(run_on_boot: bool) {
    let result = if run_on_boot {
        imp::enable()
    } else {
        imp::disable()
    };
    if let Err(err) = result {
        tracing::warn!(
            error = %err,
            enabled = run_on_boot,
            "could not sync app autostart registration",
        );
    }
}

#[cfg(windows)]
mod imp {
    use std::io;

    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    use winreg::RegKey;

    const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "vidcull";

    pub fn enable() -> io::Result<()> {
        let exe = std::env::current_exe()?;
        let command = format!("\"{}\" {}", exe.display(), super::HIDDEN_FLAG);
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(RUN_SUBKEY)?;
        key.set_value(VALUE_NAME, &command)
    }

    pub fn disable() -> io::Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = match hkcu.open_subkey_with_flags(RUN_SUBKEY, KEY_READ | KEY_WRITE) {
            Ok(key) => key,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        match key.delete_value(VALUE_NAME) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    pub fn enable() -> io::Result<()> {
        Ok(())
    }

    pub fn disable() -> io::Result<()> {
        Ok(())
    }
}
