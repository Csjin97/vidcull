use std::collections::{BTreeMap, BTreeSet, HashMap};

use vidcull_core::Result;
use vidcull_core::types::FileId;
use vidcull_db::Database;
use vidcull_db::repo::{DuplicateGroupsRepo, TrustLevel};

fn trust_strength(trust: TrustLevel) -> u8 {
    match trust {
        TrustLevel::Exact => 3,
        TrustLevel::VeryLikely => 2,
        TrustLevel::Possible => 1,
    }
}

fn stronger(a: TrustLevel, b: TrustLevel) -> TrustLevel {
    if trust_strength(b) > trust_strength(a) {
        b
    } else {
        a
    }
}

fn is_transitive(trust: TrustLevel, non_transitive: bool) -> bool {
    matches!(trust, TrustLevel::Exact | TrustLevel::VeryLikely) && !non_transitive
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMembership {
    pub group_id: i64,
    pub trust: TrustLevel,
    pub members: Vec<FileId>,
    pub non_transitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterMember {
    pub file_id: FileId,
    pub trust: TrustLevel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cluster {
    pub members: Vec<ClusterMember>,
    pub representative_trust: TrustLevel,
    pub group_ids: Vec<i64>,
}

impl Cluster {
    #[must_use]
    pub fn member_ids(&self) -> Vec<FileId> {
        self.members.iter().map(|m| m.file_id).collect()
    }
}

#[must_use]
pub fn cluster_components(groups: &[GroupMembership]) -> Vec<Cluster> {
    let transitive: Vec<&GroupMembership> = groups
        .iter()
        .filter(|g| is_transitive(g.trust, g.non_transitive) && !g.members.is_empty())
        .collect();

    let mut clusters = merge_transitive(&transitive);

    let transitive_cluster_of: HashMap<FileId, usize> = clusters
        .iter()
        .enumerate()
        .flat_map(|(idx, c)| c.members.iter().map(move |m| (m.file_id, idx)))
        .collect();

    let mut fanout: BTreeMap<FanoutKey, FanoutCard> = BTreeMap::new();
    for group in groups
        .iter()
        .filter(|g| g.trust == TrustLevel::Possible && !g.non_transitive && !g.members.is_empty())
    {
        if fully_inside_one_transitive_cluster(&group.members, &transitive_cluster_of) {
            continue;
        }
        if let Some(key) = fanout_key(&group.members, &transitive_cluster_of) {
            let entry = fanout.entry(key).or_default();
            entry.0.extend(group.members.iter().copied());
            entry.1.push(group.group_id);
            continue;
        }
        let mut members: Vec<FileId> = group.members.clone();
        members.sort_unstable();
        members.dedup();
        clusters.push(Cluster {
            members: members
                .into_iter()
                .map(|file_id| ClusterMember {
                    file_id,
                    trust: group.trust,
                })
                .collect(),
            representative_trust: group.trust,
            group_ids: vec![group.group_id],
        });
    }

    for (_key, (members, mut group_ids)) in fanout {
        group_ids.sort_unstable();
        group_ids.dedup();
        clusters.push(Cluster {
            members: members
                .into_iter()
                .map(|file_id| ClusterMember {
                    file_id,
                    trust: TrustLevel::Possible,
                })
                .collect(),
            representative_trust: TrustLevel::Possible,
            group_ids,
        });
    }

    for group in groups
        .iter()
        .filter(|g| g.non_transitive && !g.members.is_empty())
    {
        let mut members: Vec<FileId> = group.members.clone();
        members.sort_unstable();
        members.dedup();
        clusters.push(Cluster {
            members: members
                .into_iter()
                .map(|file_id| ClusterMember {
                    file_id,
                    trust: group.trust,
                })
                .collect(),
            representative_trust: group.trust,
            group_ids: vec![group.group_id],
        });
    }

    clusters.sort_by_key(|c| c.members.first().map_or(FileId(i64::MAX), |m| m.file_id));
    clusters
}

fn fully_inside_one_transitive_cluster(
    members: &[FileId],
    cluster_of: &HashMap<FileId, usize>,
) -> bool {
    let mut shared: Option<usize> = None;
    for member in members {
        let Some(&idx) = cluster_of.get(member) else {
            return false;
        };
        match shared {
            Some(prev) if prev != idx => return false,
            _ => shared = Some(idx),
        }
    }
    shared.is_some()
}

type FanoutKey = (Vec<FileId>, usize);

type FanoutCard = (BTreeSet<FileId>, Vec<i64>);

fn fanout_key(members: &[FileId], cluster_of: &HashMap<FileId, usize>) -> Option<FanoutKey> {
    let mut cluster_idx: Option<usize> = None;
    let mut clip_ends: Vec<FileId> = Vec::new();
    for member in members {
        match cluster_of.get(member) {
            Some(&idx) => match cluster_idx {
                Some(prev) if prev != idx => return None,
                _ => cluster_idx = Some(idx),
            },
            None => clip_ends.push(*member),
        }
    }
    let cluster_idx = cluster_idx?;
    if clip_ends.is_empty() {
        return None;
    }
    clip_ends.sort_unstable();
    clip_ends.dedup();
    Some((clip_ends, cluster_idx))
}

fn merge_transitive(transitive: &[&GroupMembership]) -> Vec<Cluster> {
    let mut id_set: BTreeSet<FileId> = BTreeSet::new();
    for group in transitive {
        id_set.extend(group.members.iter().copied());
    }
    let ids: Vec<FileId> = id_set.into_iter().collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let position: HashMap<FileId, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();

    let mut dsu = DisjointSet::new(ids.len());
    for group in transitive {
        if let Some((first, rest)) = group.members.split_first() {
            for member in rest {
                dsu.union(position[first], position[member]);
            }
        }
    }

    let mut member_trust: HashMap<FileId, TrustLevel> = HashMap::new();
    for group in transitive {
        for member in &group.members {
            member_trust
                .entry(*member)
                .and_modify(|t| *t = stronger(*t, group.trust))
                .or_insert(group.trust);
        }
    }

    let mut members_by_root: BTreeMap<usize, Vec<FileId>> = BTreeMap::new();
    for &id in &ids {
        members_by_root
            .entry(dsu.find(position[&id]))
            .or_default()
            .push(id);
    }
    let mut groups_by_root: BTreeMap<usize, Vec<i64>> = BTreeMap::new();
    let mut rep_by_root: BTreeMap<usize, TrustLevel> = BTreeMap::new();
    for group in transitive {
        let root = dsu.find(position[&group.members[0]]);
        groups_by_root.entry(root).or_default().push(group.group_id);
        rep_by_root
            .entry(root)
            .and_modify(|t| *t = stronger(*t, group.trust))
            .or_insert(group.trust);
    }

    members_by_root
        .into_iter()
        .map(|(root, mut members)| {
            members.sort_unstable();
            let representative_trust = rep_by_root[&root];
            let mut group_ids = groups_by_root.remove(&root).unwrap_or_default();
            group_ids.sort_unstable();
            Cluster {
                members: members
                    .into_iter()
                    .map(|file_id| ClusterMember {
                        file_id,
                        trust: member_trust[&file_id],
                    })
                    .collect(),
                representative_trust,
                group_ids,
            }
        })
        .collect()
}

struct DisjointSet {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl DisjointSet {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (big, small) = if self.size[ra] >= self.size[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClusterReport {
    pub clusters: usize,
    pub members_total: usize,
    pub exact_clusters: usize,
    pub very_likely_clusters: usize,
    pub possible_clusters: usize,
    pub largest_cluster: usize,
    pub cross_trust_clusters: usize,
    pub multi_group_clusters: usize,
}

#[must_use]
pub fn summarize_clusters(clusters: &[Cluster]) -> ClusterReport {
    let mut report = ClusterReport {
        clusters: clusters.len(),
        ..ClusterReport::default()
    };
    for cluster in clusters {
        report.members_total += cluster.members.len();
        match cluster.representative_trust {
            TrustLevel::Exact => report.exact_clusters += 1,
            TrustLevel::VeryLikely => report.very_likely_clusters += 1,
            TrustLevel::Possible => report.possible_clusters += 1,
        }
        report.largest_cluster = report.largest_cluster.max(cluster.members.len());

        let mut seen = [false; 3];
        for member in &cluster.members {
            seen[(trust_strength(member.trust) - 1) as usize] = true;
        }
        if seen.iter().filter(|&&s| s).count() > 1 {
            report.cross_trust_clusters += 1;
        }
        if cluster.group_ids.len() > 1 {
            report.multi_group_clusters += 1;
        }
    }
    report
}

impl std::fmt::Display for ClusterReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} clusters ({} members; exact={} very_likely={} possible={}); \
             largest={} cross_trust={} multi_group={}",
            self.clusters,
            self.members_total,
            self.exact_clusters,
            self.very_likely_clusters,
            self.possible_clusters,
            self.largest_cluster,
            self.cross_trust_clusters,
            self.multi_group_clusters,
        )
    }
}

pub fn build_clusters(db: &Database) -> Result<Vec<Cluster>> {
    let memberships: Vec<GroupMembership> = DuplicateGroupsRepo::new(db.conn())
        .list_all_with_members()?
        .into_iter()
        .map(|(group, members)| GroupMembership {
            group_id: group.id,
            trust: group.trust_level,
            members,
            non_transitive: group.non_transitive,
        })
        .collect();
    Ok(cluster_components(&memberships))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn membership(group_id: i64, trust: TrustLevel, members: &[i64]) -> GroupMembership {
        GroupMembership {
            group_id,
            trust,
            members: members.iter().map(|&i| FileId(i)).collect(),
            non_transitive: false,
        }
    }

    fn membership_non_transitive(
        group_id: i64,
        trust: TrustLevel,
        members: &[i64],
    ) -> GroupMembership {
        GroupMembership {
            group_id,
            trust,
            members: members.iter().map(|&i| FileId(i)).collect(),
            non_transitive: true,
        }
    }

    fn member(cluster: &Cluster, file_id: i64) -> ClusterMember {
        *cluster
            .members
            .iter()
            .find(|m| m.file_id == FileId(file_id))
            .expect("member present")
    }

    #[test]
    fn empty_input_yields_no_clusters() {
        assert!(cluster_components(&[]).is_empty());
    }

    #[test]
    fn trust_strength_orders_exact_over_very_likely_over_possible() {
        assert!(trust_strength(TrustLevel::Exact) > trust_strength(TrustLevel::VeryLikely));
        assert!(trust_strength(TrustLevel::VeryLikely) > trust_strength(TrustLevel::Possible));
        assert_eq!(
            stronger(TrustLevel::Possible, TrustLevel::Exact),
            TrustLevel::Exact
        );
        assert_eq!(
            stronger(TrustLevel::Exact, TrustLevel::Possible),
            TrustLevel::Exact
        );
    }

    #[test]
    fn transitive_chain_merges_across_trust_levels() {
        let groups = [
            membership(1, TrustLevel::Exact, &[1, 2]),
            membership(2, TrustLevel::VeryLikely, &[2, 3]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(clusters.len(), 1);
        let c = &clusters[0];
        assert_eq!(c.member_ids(), vec![FileId(1), FileId(2), FileId(3)]);
        assert_eq!(c.representative_trust, TrustLevel::Exact);
        assert_eq!(member(c, 1).trust, TrustLevel::Exact);
        assert_eq!(member(c, 2).trust, TrustLevel::Exact);
        assert_eq!(member(c, 3).trust, TrustLevel::VeryLikely);
        assert_eq!(c.group_ids, vec![1, 2]);
    }

    #[test]
    fn member_retains_strongest_whole_file_trust() {
        let groups = [
            membership(1, TrustLevel::VeryLikely, &[2, 9]),
            membership(2, TrustLevel::Exact, &[1, 2]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(clusters.len(), 1);
        assert_eq!(member(&clusters[0], 2).trust, TrustLevel::Exact);
        assert_eq!(member(&clusters[0], 9).trust, TrustLevel::VeryLikely);
    }

    #[test]
    fn disjoint_exact_groups_stay_separate() {
        let groups = [
            membership(1, TrustLevel::Exact, &[1, 2]),
            membership(2, TrustLevel::Exact, &[3, 4]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].member_ids(), vec![FileId(1), FileId(2)]);
        assert_eq!(clusters[1].member_ids(), vec![FileId(3), FileId(4)]);
    }

    #[test]
    fn possible_group_is_not_transitive() {
        let groups = [
            membership(1, TrustLevel::VeryLikely, &[1, 2]),
            membership(2, TrustLevel::Possible, &[2, 3]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(clusters.len(), 2);
        let whole = clusters
            .iter()
            .find(|c| c.member_ids() == vec![FileId(1), FileId(2)]);
        let whole = whole.expect("whole-file cluster present");
        assert_eq!(whole.representative_trust, TrustLevel::VeryLikely);
        let clip = clusters
            .iter()
            .find(|c| c.member_ids() == vec![FileId(2), FileId(3)]);
        let clip = clip.expect("clip cluster present");
        assert_eq!(clip.representative_trust, TrustLevel::Possible);
        assert_eq!(member(clip, 3).trust, TrustLevel::Possible);
    }

    #[test]
    fn possible_group_duplicating_a_transitive_pair_is_dropped() {
        let groups = [
            membership(1, TrustLevel::VeryLikely, &[1, 2]),
            membership(2, TrustLevel::Possible, &[1, 2]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(clusters.len(), 1, "duplicate POSSIBLE pair adds no cluster");
        let c = &clusters[0];
        assert_eq!(c.member_ids(), vec![FileId(1), FileId(2)]);
        assert_eq!(c.representative_trust, TrustLevel::VeryLikely);
        assert_eq!(c.group_ids, vec![1]);
        let report = summarize_clusters(&clusters);
        assert_eq!(report.possible_clusters, 0);
    }

    #[test]
    fn possible_group_not_fully_inside_a_transitive_cluster_is_kept() {
        let groups = [
            membership(1, TrustLevel::VeryLikely, &[1, 2]),
            membership(2, TrustLevel::Possible, &[2, 3]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(clusters.len(), 2, "partial-only clip cluster preserved");
        let report = summarize_clusters(&clusters);
        assert_eq!(report.possible_clusters, 1);
    }

    #[test]
    fn non_transitive_very_likely_group_stays_its_own_cluster() {
        let groups = [
            membership_non_transitive(1, TrustLevel::VeryLikely, &[1, 2]),
            membership(2, TrustLevel::VeryLikely, &[2, 3]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(
            clusters.len(),
            2,
            "flagged group must not cascade-merge via shared member 2"
        );
        let flagged = clusters
            .iter()
            .find(|c| c.member_ids() == vec![FileId(1), FileId(2)])
            .expect("flagged pair stands alone");
        assert_eq!(flagged.representative_trust, TrustLevel::VeryLikely);
        assert_eq!(flagged.group_ids, vec![1]);
        let transitive = clusters
            .iter()
            .find(|c| c.member_ids() == vec![FileId(2), FileId(3)])
            .expect("unrelated transitive pair stands alone too");
        assert_eq!(transitive.group_ids, vec![2]);
    }

    #[test]
    fn non_transitive_group_is_not_deduped_by_128_containment() {
        let groups = [
            membership(1, TrustLevel::Exact, &[1, 2]),
            membership_non_transitive(2, TrustLevel::VeryLikely, &[1, 2]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(
            clusters.len(),
            2,
            "flagged group survives even though {{1,2}} is already EXACT"
        );
        let flagged = clusters
            .iter()
            .find(|c| c.group_ids == vec![2])
            .expect("flagged group present as its own card");
        assert_eq!(flagged.member_ids(), vec![FileId(1), FileId(2)]);
    }

    #[test]
    fn non_transitive_group_is_not_absorbed_by_167_fanout() {
        let groups = [
            membership(1, TrustLevel::Exact, &[4, 5]),
            membership(2, TrustLevel::Possible, &[2, 4]),
            membership(3, TrustLevel::Possible, &[2, 5]),
            membership_non_transitive(4, TrustLevel::VeryLikely, &[4, 9]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(clusters.len(), 3);
        let flagged = clusters
            .iter()
            .find(|c| c.group_ids == vec![4])
            .expect("flagged group is its own card, not folded into the fan-out");
        assert_eq!(flagged.member_ids(), vec![FileId(4), FileId(9)]);
        let fanout = clusters
            .iter()
            .find(|c| c.representative_trust == TrustLevel::Possible)
            .expect("possible fan-out card present");
        assert_eq!(fanout.member_ids(), vec![FileId(2), FileId(4), FileId(5)]);
    }

    #[test]
    fn two_possible_clips_into_unrelated_sources_stay_separate() {
        let groups = [
            membership(1, TrustLevel::Possible, &[1, 2]),
            membership(2, TrustLevel::Possible, &[1, 3]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(clusters.len(), 2, "shared clip must not merge two sources");
    }

    #[test]
    fn possible_fanout_over_one_near_dup_cluster_collapses_to_single_card() {
        let groups = [
            membership(1, TrustLevel::Exact, &[4, 5]),
            membership(2, TrustLevel::VeryLikely, &[4, 6]),
            membership(3, TrustLevel::Possible, &[2, 4]),
            membership(4, TrustLevel::Possible, &[2, 5]),
            membership(5, TrustLevel::Possible, &[2, 6]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(clusters.len(), 2);
        let report = summarize_clusters(&clusters);
        assert_eq!(report.possible_clusters, 1);
        let clip = clusters
            .iter()
            .find(|c| c.representative_trust == TrustLevel::Possible)
            .expect("collapsed clip card present");
        assert_eq!(
            clip.member_ids(),
            vec![FileId(2), FileId(4), FileId(5), FileId(6)]
        );
        assert_eq!(clip.group_ids, vec![3, 4, 5]);
    }

    #[test]
    fn two_distinct_clips_over_one_cluster_stay_two_cards() {
        let groups = [
            membership(1, TrustLevel::VeryLikely, &[4, 5, 6]),
            membership(2, TrustLevel::Possible, &[2, 4]),
            membership(3, TrustLevel::Possible, &[2, 5]),
            membership(4, TrustLevel::Possible, &[2, 6]),
            membership(5, TrustLevel::Possible, &[10, 4]),
            membership(6, TrustLevel::Possible, &[10, 5]),
            membership(7, TrustLevel::Possible, &[10, 6]),
        ];
        let clusters = cluster_components(&groups);
        let report = summarize_clusters(&clusters);
        assert_eq!(clusters.len(), 3);
        assert_eq!(report.possible_clusters, 2);
        let clip2 = clusters
            .iter()
            .find(|c| c.member_ids() == vec![FileId(2), FileId(4), FileId(5), FileId(6)])
            .expect("clip-2 card present");
        assert_eq!(clip2.group_ids, vec![2, 3, 4]);
        let clip10 = clusters
            .iter()
            .find(|c| c.member_ids() == vec![FileId(4), FileId(5), FileId(6), FileId(10)])
            .expect("clip-10 card present");
        assert_eq!(clip10.group_ids, vec![5, 6, 7]);
    }

    #[test]
    fn clusters_sorted_by_smallest_member_and_group_ids_sorted() {
        let groups = [
            membership(5, TrustLevel::Exact, &[10, 11]),
            membership(1, TrustLevel::Exact, &[1, 2]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(clusters[0].member_ids()[0], FileId(1));
        assert_eq!(clusters[1].member_ids()[0], FileId(10));
    }

    #[test]
    fn group_ids_are_collected_and_sorted() {
        let groups = [
            membership(7, TrustLevel::Exact, &[2, 3]),
            membership(3, TrustLevel::Exact, &[1, 2]),
        ];
        let clusters = cluster_components(&groups);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].group_ids, vec![3, 7]);
    }

    #[test]
    fn summarize_empty_is_all_zero() {
        let report = summarize_clusters(&[]);
        assert_eq!(report, ClusterReport::default());
        assert_eq!(report.clusters, 0);
    }

    #[test]
    fn summarize_counts_trust_mix_and_fp_signals() {
        let groups = [
            membership(1, TrustLevel::Exact, &[1, 2]),
            membership(2, TrustLevel::VeryLikely, &[2, 3]),
            membership(3, TrustLevel::Possible, &[10, 11]),
        ];
        let clusters = cluster_components(&groups);
        let report = summarize_clusters(&clusters);

        assert_eq!(report.clusters, 2);
        assert_eq!(report.members_total, 5);
        assert_eq!(report.exact_clusters, 1);
        assert_eq!(report.very_likely_clusters, 0);
        assert_eq!(report.possible_clusters, 1);
        assert_eq!(report.largest_cluster, 3);
        assert_eq!(report.cross_trust_clusters, 1);
        assert_eq!(report.multi_group_clusters, 1);
    }

    #[test]
    fn summarize_single_trust_cluster_is_not_cross_trust() {
        let clusters = cluster_components(&[membership(1, TrustLevel::Exact, &[1, 2, 3])]);
        let report = summarize_clusters(&clusters);
        assert_eq!(report.clusters, 1);
        assert_eq!(report.cross_trust_clusters, 0);
        assert_eq!(report.multi_group_clusters, 0);
        assert_eq!(report.largest_cluster, 3);
    }

    #[test]
    fn deterministic_across_runs() {
        let groups = [
            membership(2, TrustLevel::VeryLikely, &[3, 7]),
            membership(1, TrustLevel::Exact, &[1, 3]),
            membership(9, TrustLevel::Possible, &[7, 20]),
        ];
        assert_eq!(cluster_components(&groups), cluster_components(&groups));
    }
}
