use std::collections::{BTreeMap, BTreeSet};

use vidcull_core::Result;
use vidcull_core::types::{Blake3Hash, FileId};
use vidcull_db::Database;
use vidcull_db::repo::{DuplicateGroupsRepo, FilesRepo, TrustLevel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactGroup {
    pub hash: Blake3Hash,
    pub members: Vec<FileId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExactGroupingPlan {
    pub groups: Vec<ExactGroup>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildOutcome {
    pub groups_created: usize,
    pub groups_extended: usize,
    pub members_added: usize,
    pub buckets_skipped_ambiguous: usize,
}

#[must_use]
pub fn plan_exact_groups<I>(hashed: I) -> ExactGroupingPlan
where
    I: IntoIterator<Item = (FileId, Blake3Hash)>,
{
    let mut by_hash: BTreeMap<Blake3Hash, Vec<FileId>> = BTreeMap::new();
    for (file_id, hash) in hashed {
        by_hash.entry(hash).or_default().push(file_id);
    }
    let mut groups: Vec<ExactGroup> = by_hash
        .into_iter()
        .filter_map(|(hash, mut members)| {
            members.sort_unstable();
            members.dedup();
            if members.len() < 2 {
                return None;
            }
            Some(ExactGroup { hash, members })
        })
        .collect();
    groups.sort_by_key(|g| g.members[0]);
    ExactGroupingPlan { groups }
}

pub fn rebuild_exact_groups(db: &mut Database, now_unix_s: i64) -> Result<RebuildOutcome> {
    db.transaction(|conn| {
        let files = FilesRepo::new(conn);
        let groups = DuplicateGroupsRepo::new(conn);

        let plan = plan_exact_groups(files.list_hashed_active()?);

        let mut outcome = RebuildOutcome::default();
        for group in plan.groups {
            apply_group(&groups, &group, now_unix_s, &mut outcome)?;
        }
        Ok(outcome)
    })
}

fn apply_group(
    groups: &DuplicateGroupsRepo<'_>,
    group: &ExactGroup,
    now_unix_s: i64,
    outcome: &mut RebuildOutcome,
) -> Result<()> {
    let mut existing_groups: BTreeSet<i64> = BTreeSet::new();
    let mut unmapped: Vec<FileId> = Vec::new();
    for fid in &group.members {
        match groups.find_exact_group_containing(*fid)? {
            Some(gid) => {
                existing_groups.insert(gid);
            }
            None => unmapped.push(*fid),
        }
    }

    match existing_groups.len() {
        0 => create_new(groups, group, now_unix_s, outcome),
        1 => {
            let gid = existing_groups
                .into_iter()
                .next()
                .expect("len == 1 implies first() is Some");
            extend_existing(groups, gid, group, &unmapped, outcome)
        }
        _ => {
            tracing::warn!(
                hash = %group.hash.to_hex(),
                spans_existing_groups = existing_groups.len(),
                "skipping exact bucket — members span multiple EXACT groups",
            );
            outcome.buckets_skipped_ambiguous += 1;
            Ok(())
        }
    }
}

fn create_new(
    groups: &DuplicateGroupsRepo<'_>,
    group: &ExactGroup,
    now_unix_s: i64,
    outcome: &mut RebuildOutcome,
) -> Result<()> {
    let gid = groups.create(TrustLevel::Exact, now_unix_s)?;
    for fid in &group.members {
        groups.add_member(gid, *fid)?;
    }
    outcome.groups_created += 1;
    outcome.members_added += group.members.len();
    Ok(())
}

fn extend_existing(
    groups: &DuplicateGroupsRepo<'_>,
    gid: i64,
    group: &ExactGroup,
    unmapped: &[FileId],
    outcome: &mut RebuildOutcome,
) -> Result<()> {
    let bucket_set: BTreeSet<FileId> = group.members.iter().copied().collect();
    let current = groups.list_members(gid)?;
    let has_foreign = current.iter().any(|m| !bucket_set.contains(m));
    if has_foreign {
        tracing::warn!(
            group_id = gid,
            hash = %group.hash.to_hex(),
            "skipping exact bucket — candidate group has members outside this bucket",
        );
        outcome.buckets_skipped_ambiguous += 1;
        return Ok(());
    }
    if unmapped.is_empty() {
        return Ok(());
    }
    for fid in unmapped {
        groups.add_member(gid, *fid)?;
    }
    outcome.groups_extended += 1;
    outcome.members_added += unmapped.len();
    Ok(())
}

#[cfg(test)]
mod tests {
    use vidcull_core::types::HASH_LEN;

    use super::*;

    fn h(byte: u8) -> Blake3Hash {
        Blake3Hash::from_bytes([byte; HASH_LEN])
    }

    #[test]
    fn plan_empty_input_yields_empty_plan() {
        let plan = plan_exact_groups(std::iter::empty());
        assert!(plan.groups.is_empty());
    }

    #[test]
    fn plan_singletons_are_excluded() {
        let plan = plan_exact_groups(vec![(FileId(1), h(0xaa))]);
        assert!(
            plan.groups.is_empty(),
            "single-member buckets are not duplicates",
        );
    }

    #[test]
    fn plan_three_identical_hashes_form_one_group() {
        let plan = plan_exact_groups(vec![
            (FileId(3), h(0x07)),
            (FileId(1), h(0x07)),
            (FileId(2), h(0x07)),
        ]);
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].hash, h(0x07));
        assert_eq!(
            plan.groups[0].members,
            vec![FileId(1), FileId(2), FileId(3)],
        );
    }

    #[test]
    fn plan_two_distinct_hashes_yield_two_groups() {
        let plan = plan_exact_groups(vec![
            (FileId(10), h(0x10)),
            (FileId(11), h(0x10)),
            (FileId(20), h(0x20)),
            (FileId(21), h(0x20)),
        ]);
        assert_eq!(plan.groups.len(), 2);
    }

    #[test]
    fn plan_groups_sorted_by_smallest_member() {
        let plan = plan_exact_groups(vec![
            (FileId(10), h(0xee)),
            (FileId(11), h(0xee)),
            (FileId(1), h(0x11)),
            (FileId(2), h(0x11)),
        ]);
        assert_eq!(plan.groups.len(), 2);
        assert_eq!(plan.groups[0].members[0], FileId(1));
        assert_eq!(plan.groups[1].members[0], FileId(10));
    }

    #[test]
    fn plan_dedups_repeated_file_id_inputs() {
        let plan = plan_exact_groups(vec![
            (FileId(1), h(0x05)),
            (FileId(1), h(0x05)),
            (FileId(2), h(0x05)),
        ]);
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].members, vec![FileId(1), FileId(2)]);
    }

    #[test]
    fn plan_self_duplicates_remain_singletons() {
        let plan = plan_exact_groups(vec![(FileId(1), h(0xab)), (FileId(1), h(0xab))]);
        assert!(
            plan.groups.is_empty(),
            "duplicated input for one file must not synthesise a group",
        );
    }
}
