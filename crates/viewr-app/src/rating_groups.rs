use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) fn build_owner_members(owners: &[Option<PathBuf>]) -> Vec<Option<Arc<[usize]>>> {
    let mut first_by_owner: HashMap<&std::path::Path, usize> = HashMap::new();
    let mut duplicate_groups: HashMap<&std::path::Path, Vec<usize>> = HashMap::new();
    for (index, owner) in owners.iter().enumerate() {
        if let Some(owner) = owner {
            let owner = owner.as_path();
            if let Some(&first) = first_by_owner.get(owner) {
                duplicate_groups
                    .entry(owner)
                    .or_insert_with(|| vec![first])
                    .push(index);
            } else {
                first_by_owner.insert(owner, index);
            }
        }
    }

    let mut members = vec![None; owners.len()];
    for group in duplicate_groups.into_values() {
        let shared = Arc::<[usize]>::from(group);
        for &index in shared.iter() {
            members[index] = Some(shared.clone());
        }
    }
    members
}

pub(crate) fn install_rating_for_members(
    ratings: &mut HashMap<usize, u8>,
    members: &[usize],
    rating: u8,
    mut transition_matters: impl FnMut(u8, u8) -> bool,
) -> bool {
    let mut changed = false;
    for &index in members {
        let old_rating = ratings.get(&index).copied().unwrap_or(0);
        ratings.insert(index, rating);
        changed |= transition_matters(old_rating, rating);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_members_are_shared_and_unresolved_entries_stay_independent() {
        let owner = PathBuf::from("/photos/photo.xmp");
        let members = build_owner_members(&[
            Some(owner.clone()),
            Some(owner),
            Some(PathBuf::from("/photos/other.xmp")),
            None,
            None,
        ]);

        assert_eq!(members[0].as_deref(), Some([0, 1].as_slice()));
        assert!(Arc::ptr_eq(
            members[0].as_ref().unwrap(),
            members[1].as_ref().unwrap()
        ));
        assert!(members[2].is_none());
        assert!(members[3].is_none());
        assert!(members[4].is_none());
    }

    #[test]
    fn rating_install_touches_only_the_precomputed_owner_group() {
        let mut ratings = HashMap::from([(0, 1), (1, 2), (2, 3)]);

        assert!(install_rating_for_members(
            &mut ratings,
            &[0, 1],
            5,
            |old, new| old < 5 && new >= 5
        ));
        assert_eq!(ratings, HashMap::from([(0, 5), (1, 5), (2, 3)]));
    }
}
