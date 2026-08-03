use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write as _};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

const MEDIA_EXTS: [&str; 9] = [
    "mp4", "mkv", "avi", "mov", "webm", "ts", "m4v", "wmv", "flv",
];

const KEYMAP_HEADER: &str =
    "# vidcull keymap v1 — LOCAL ONLY, never submit. Maps redacted token -> real path.";

fn media_ext(path: &str) -> &'static str {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let ext = file.rsplit_once('.').map_or("", |(_, e)| e);
    let lower = ext.to_ascii_lowercase();
    MEDIA_EXTS
        .into_iter()
        .find(|known| *known == lower)
        .unwrap_or("bin")
}

fn hash64(path_normalized: &str, salt: &[u8; 32]) -> u64 {
    let h = blake3::keyed_hash(salt, path_normalized.as_bytes());
    let bytes = h.as_bytes();
    u64::from_le_bytes(bytes[..8].try_into().expect("blake3 hash is 32 bytes"))
}

fn token(hash: u64, path: &str) -> String {
    let mut s = String::with_capacity(21);
    let _ = write!(s, "{hash:016x}");
    s.push('.');
    s.push_str(media_ext(path));
    s
}

#[must_use]
pub fn redact_with_salt(path: &str, salt: &[u8; 32]) -> String {
    let normalized = path.replace('\\', "/");
    token(hash64(&normalized, salt), &normalized)
}

struct KeymapState {
    writer: Option<File>,
    seen: HashSet<u64>,
}

pub struct Redactor {
    salt: [u8; 32],
    keymap: Mutex<KeymapState>,
}

impl Redactor {
    #[must_use]
    pub fn with_salt(salt: [u8; 32]) -> Self {
        Self {
            salt,
            keymap: Mutex::new(KeymapState {
                writer: None,
                seen: HashSet::new(),
            }),
        }
    }

    fn open_or_create(keymap_path: &Path) -> Self {
        if let Some((salt, seen)) = load_keymap(keymap_path) {
            let writer = OpenOptions::new().append(true).open(keymap_path).ok();
            return Self {
                salt,
                keymap: Mutex::new(KeymapState { writer, seen }),
            };
        }

        let salt = generate_salt();
        match create_keymap(keymap_path, &salt) {
            Some(writer) => Self {
                salt,
                keymap: Mutex::new(KeymapState {
                    writer: Some(writer),
                    seen: HashSet::new(),
                }),
            },
            None => Self::with_salt(salt),
        }
    }

    #[must_use]
    pub fn redact(&self, path: &str) -> String {
        let normalized = path.replace('\\', "/");
        let hash = hash64(&normalized, &self.salt);
        let tok = token(hash, &normalized);

        if let Ok(mut state) = self.keymap.lock() {
            if state.seen.insert(hash) {
                if let Some(writer) = state.writer.as_mut() {
                    if writeln!(writer, "{tok}\t{normalized}").is_err() {
                        state.writer = None;
                    }
                }
            }
        }
        tok
    }
}

fn load_keymap(keymap_path: &Path) -> Option<([u8; 32], HashSet<u64>)> {
    let file = File::open(keymap_path).ok()?;
    let reader = BufReader::new(file);
    let mut salt: Option<[u8; 32]> = None;
    let mut seen = HashSet::new();

    for line in reader.lines() {
        let Ok(line) = line else { return None };
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(hex) = line.strip_prefix("salt ") {
            salt = parse_salt_hex(hex);
            continue;
        }
        if let Some((tok, _)) = line.split_once('\t') {
            if let Some(hex) = tok.split('.').next() {
                if let Ok(hash) = u64::from_str_radix(hex, 16) {
                    seen.insert(hash);
                }
            }
        }
    }
    salt.map(|s| (s, seen))
}

fn parse_salt_hex(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut salt = [0u8; 32];
    for (i, byte) in salt.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(salt)
}

fn create_keymap(keymap_path: &Path, salt: &[u8; 32]) -> Option<File> {
    if let Some(parent) = keymap_path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .read(false)
        .open(keymap_path)
        .ok()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }

    let mut salt_hex = String::with_capacity(64);
    for b in salt {
        let _ = write!(salt_hex, "{b:02x}");
    }
    writeln!(file, "{KEYMAP_HEADER}").ok()?;
    writeln!(file, "salt {salt_hex}").ok()?;
    OpenOptions::new().append(true).open(keymap_path).ok()
}

fn generate_salt() -> [u8; 32] {
    let mut salt = [0u8; 32];
    if getrandom::fill(&mut salt).is_ok() {
        return salt;
    }
    let mut h = blake3::Hasher::new();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.update(&nanos.to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    let stack_anchor = 0u8;
    h.update(&(std::ptr::addr_of!(stack_anchor) as usize).to_le_bytes());
    *h.finalize().as_bytes()
}

static REDACTOR: OnceLock<Redactor> = OnceLock::new();

fn global() -> &'static Redactor {
    REDACTOR.get_or_init(|| {
        let keymap_path = crate::settings::data_dir().join("keymap");
        Redactor::open_or_create(&keymap_path)
    })
}

#[must_use]
pub fn redact_path(path: &str) -> String {
    global().redact(path)
}

#[must_use]
pub fn redact_fs_path<P: AsRef<Path>>(path: P) -> String {
    global().redact(&path.as_ref().to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PATH: &str = "C:/Users/alice/Videos/holiday_2024.mp4";

    fn salt_a() -> [u8; 32] {
        [7u8; 32]
    }
    fn salt_b() -> [u8; 32] {
        [42u8; 32]
    }

    fn ext_of(token: &str) -> &str {
        token.rsplit_once('.').expect("token has an extension").1
    }

    #[test]
    fn token_omits_home_username_filename_and_original_ext() {
        let out = redact_with_salt("C:/Users/alice/secret movie.mov", &salt_a());
        assert!(!out.contains("alice"), "username leaked: {out}");
        assert!(!out.contains("Users"), "home leaked: {out}");
        assert!(!out.contains("secret"), "filename leaked: {out}");
        assert!(!out.contains("movie"), "filename leaked: {out}");
        assert_eq!(ext_of(&out), "mov", "ext not preserved: {out}");
        assert_eq!(out.len(), 16 + 1 + 3);
    }

    #[test]
    fn non_whitelisted_ext_collapses_to_bin() {
        let out = redact_with_salt("/home/bob/private.docx", &salt_a());
        assert_eq!(ext_of(&out), "bin", "{out}");
        assert!(!out.contains("docx"));
        let no_ext = redact_with_salt("/home/bob/justname", &salt_a());
        assert_eq!(ext_of(&no_ext), "bin", "{no_ext}");
    }

    #[test]
    fn per_install_salt_changes_the_hash() {
        let a = redact_with_salt(TEST_PATH, &salt_a());
        let b = redact_with_salt(TEST_PATH, &salt_b());
        assert_ne!(a, b, "different salts must yield different tokens");
        assert_eq!(ext_of(&a), "mp4");
        assert_eq!(ext_of(&b), "mp4");
    }

    #[test]
    fn redaction_is_stable_for_same_salt_and_path() {
        assert_eq!(
            redact_with_salt(TEST_PATH, &salt_a()),
            redact_with_salt(TEST_PATH, &salt_a())
        );
    }

    #[test]
    fn backslash_and_forward_slash_paths_hash_identically() {
        let win = redact_with_salt(r"C:\Users\alice\Videos\clip.mkv", &salt_a());
        let unix = redact_with_salt("C:/Users/alice/Videos/clip.mkv", &salt_a());
        assert_eq!(win, unix);
    }

    #[test]
    fn keymap_round_trips_token_back_to_real_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keymap = dir.path().join("keymap");

        let tok_holiday = {
            let r = Redactor::open_or_create(&keymap);
            let t = r.redact(TEST_PATH);
            let _ = r.redact("D:/clips/b.mkv");
            t
        };

        let contents = std::fs::read_to_string(&keymap).expect("read keymap");
        assert!(
            contents.contains(&format!("{tok_holiday}\t{TEST_PATH}")),
            "keymap missing reverse mapping:\n{contents}"
        );
        let r2 = Redactor::open_or_create(&keymap);
        assert_eq!(r2.redact(TEST_PATH), tok_holiday);
    }

    #[test]
    fn redact_without_keymap_is_graceful() {
        let r = Redactor::with_salt(salt_a());
        let a = r.redact(TEST_PATH);
        let b = r.redact(TEST_PATH);
        assert_eq!(a, b);
        assert_eq!(ext_of(&a), "mp4");
    }

    #[test]
    fn media_ext_is_case_insensitive() {
        assert_eq!(media_ext("/x/Y.MP4"), "mp4");
        assert_eq!(media_ext("/x/Y.MkV"), "mkv");
        assert_eq!(media_ext("/x/y.heic"), "bin");
    }
}
