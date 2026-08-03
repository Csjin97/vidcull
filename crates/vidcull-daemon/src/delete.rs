use std::collections::BTreeSet;
use std::path::Path;

use vidcull_core::types::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    Trash,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    Trashed,
    PermanentlyDeleted,
    AlreadyAbsent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteReject {
    NoneSelected,
    UnknownMember,
    DeleteAll,
    BestUnconfirmed,
}

impl DeleteReject {
    #[must_use]
    pub fn code_str(self) -> &'static str {
        match self {
            Self::NoneSelected => "NONE_SELECTED",
            Self::UnknownMember => "UNKNOWN_MEMBER",
            Self::DeleteAll => "DELETE_ALL",
            Self::BestUnconfirmed => "BEST_UNCONFIRMED",
        }
    }
}

pub fn plan_deletion(
    members: &[FileId],
    best: Option<FileId>,
    selected: &[FileId],
    confirm_best: bool,
) -> Result<Vec<FileId>, DeleteReject> {
    if selected.is_empty() {
        return Err(DeleteReject::NoneSelected);
    }
    let member_set: BTreeSet<FileId> = members.iter().copied().collect();
    let mut seen: BTreeSet<FileId> = BTreeSet::new();
    let mut to_delete: Vec<FileId> = Vec::new();
    for &id in selected {
        if !member_set.contains(&id) {
            return Err(DeleteReject::UnknownMember);
        }
        if seen.insert(id) {
            to_delete.push(id);
        }
    }
    if to_delete.len() >= member_set.len() {
        return Err(DeleteReject::DeleteAll);
    }
    if let Some(best) = best {
        if to_delete.contains(&best) && !confirm_best {
            return Err(DeleteReject::BestUnconfirmed);
        }
    }
    Ok(to_delete)
}

pub trait FileRemover: Send + Sync {
    fn remove(&self, path: &Path, mode: DeleteMode) -> std::io::Result<RemoveOutcome>;
}

pub struct OsFileRemover;

impl FileRemover for OsFileRemover {
    fn remove(&self, path: &Path, mode: DeleteMode) -> std::io::Result<RemoveOutcome> {
        let absent_before = path_is_absent(path);
        let result = match mode {
            DeleteMode::Trash => {
                trash::delete(path).map_err(|err| std::io::Error::other(err.to_string()))
            }
            DeleteMode::Permanent => std::fs::remove_file(path),
        };
        match result {
            Ok(()) if absent_before => Ok(RemoveOutcome::AlreadyAbsent),
            Ok(()) => Ok(match mode {
                DeleteMode::Trash => RemoveOutcome::Trashed,
                DeleteMode::Permanent => RemoveOutcome::PermanentlyDeleted,
            }),
            Err(err) if path_is_absent(path) => {
                let _ = err;
                Ok(RemoveOutcome::AlreadyAbsent)
            }
            Err(err) => Err(err),
        }
    }
}

fn path_is_absent(path: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[i64]) -> Vec<FileId> {
        v.iter().map(|&i| FileId(i)).collect()
    }

    #[test]
    fn deletes_the_selected_non_best_members() {
        let members = ids(&[1, 2, 3]);
        let got = plan_deletion(&members, Some(FileId(1)), &ids(&[2, 3]), false)
            .expect("non-best selection is allowed");
        assert_eq!(got, ids(&[2, 3]));
    }

    #[test]
    fn empty_selection_is_rejected() {
        assert_eq!(
            plan_deletion(&ids(&[1, 2]), None, &[], false),
            Err(DeleteReject::NoneSelected)
        );
    }

    #[test]
    fn member_outside_the_group_is_rejected() {
        assert_eq!(
            plan_deletion(&ids(&[1, 2]), None, &ids(&[2, 9]), false),
            Err(DeleteReject::UnknownMember)
        );
    }

    #[test]
    fn deleting_every_member_is_rejected() {
        assert_eq!(
            plan_deletion(&ids(&[1, 2]), None, &ids(&[1, 2]), true),
            Err(DeleteReject::DeleteAll)
        );
        assert_eq!(
            plan_deletion(&ids(&[1, 2]), None, &ids(&[1, 1, 2]), true),
            Err(DeleteReject::DeleteAll)
        );
    }

    #[test]
    fn deleting_best_without_confirmation_is_rejected() {
        assert_eq!(
            plan_deletion(&ids(&[1, 2, 3]), Some(FileId(1)), &ids(&[1, 2]), false),
            Err(DeleteReject::BestUnconfirmed)
        );
    }

    #[test]
    fn deleting_best_with_confirmation_is_allowed() {
        let got = plan_deletion(&ids(&[1, 2, 3]), Some(FileId(1)), &ids(&[1, 2]), true)
            .expect("acknowledged best deletion proceeds");
        assert_eq!(got, ids(&[1, 2]));
    }

    #[test]
    fn duplicate_selection_is_de_duplicated() {
        let got = plan_deletion(&ids(&[1, 2, 3]), None, &ids(&[2, 2, 3]), false)
            .expect("dedup keeps first-seen order");
        assert_eq!(got, ids(&[2, 3]));
    }

    #[test]
    fn os_remover_is_idempotent_when_the_file_is_already_gone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("already-deleted.mp4");
        assert!(!missing.exists());
        assert_eq!(
            OsFileRemover
                .remove(&missing, DeleteMode::Permanent)
                .expect("removing an absent path is idempotent success"),
            RemoveOutcome::AlreadyAbsent,
        );
        assert_eq!(
            OsFileRemover
                .remove(&missing, DeleteMode::Trash)
                .expect("trashing an absent path is idempotent success"),
            RemoveOutcome::AlreadyAbsent,
            "an already-gone path is not a successful trash (W1-5)",
        );
    }

    #[test]
    fn os_remover_permanently_deletes_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let victim = dir.path().join("dupe.mp4");
        std::fs::write(&victim, b"payload").expect("write file");
        assert!(victim.exists());
        assert_eq!(
            OsFileRemover
                .remove(&victim, DeleteMode::Permanent)
                .expect("a present file is removed"),
            RemoveOutcome::PermanentlyDeleted,
        );
        assert!(!victim.exists(), "the file is gone from disk");
    }

    #[test]
    fn os_remover_reports_a_real_trash_landing_as_trashed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let victim = dir.path().join("trash-me.mp4");
        std::fs::write(&victim, b"payload").expect("write file");
        assert!(victim.exists());
        assert_eq!(
            OsFileRemover
                .remove(&victim, DeleteMode::Trash)
                .expect("a present file is trashed"),
            RemoveOutcome::Trashed,
        );
        assert!(!victim.exists(), "the file left its original location");
    }

    #[cfg(windows)]
    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn os_remover_handles_a_readonly_file_without_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ro = dir.path().join("readonly.mp4");
        std::fs::write(&ro, b"payload").expect("write file");
        let mut perms = std::fs::metadata(&ro).expect("meta").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&ro, perms).expect("set readonly");

        match OsFileRemover.remove(&ro, DeleteMode::Permanent) {
            Ok(_) => assert!(!ro.exists(), "a 'successful' removal really deleted it"),
            Err(err) => {
                assert_ne!(
                    err.kind(),
                    std::io::ErrorKind::NotFound,
                    "a present-but-unremovable file must not look vanished: {err:?}"
                );
                assert!(ro.exists(), "an errored removal left the file in place");
                let mut perms = std::fs::metadata(&ro).expect("meta").permissions();
                perms.set_readonly(false);
                std::fs::set_permissions(&ro, perms).expect("clear readonly");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn os_remover_errs_without_panic_on_a_locked_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let locked = dir.path().join("locked.mp4");
        std::fs::write(&locked, b"payload").expect("write file");
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .expect("hold an exclusive handle");

        let err = OsFileRemover
            .remove(&locked, DeleteMode::Permanent)
            .expect_err("a locked file cannot be removed");
        assert_ne!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "a locked-but-present file must not look like a vanished one: {err:?}"
        );
        drop(handle);
    }
}
