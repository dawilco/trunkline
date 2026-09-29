//! Motorola ASTRO 25 patch-group (supergroup) membership tracking.
//!
//! Dispatch consoles can temporarily "patch" several talkgroups together. The
//! control channel then carries grants for a vendor supergroup instead of the
//! member talkgroups. This registry resolves a supergroup grant back to every
//! member so a configured target keeps being followed while it is patched.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub struct PatchRegistry {
    groups: HashMap<u16, Vec<u16>>,
}

impl PatchRegistry {
    pub fn add(&mut self, supergroup: u16, members: impl IntoIterator<Item = u16>) {
        let entry = self.groups.entry(supergroup).or_default();
        for member in members {
            if member != 0 && !entry.contains(&member) {
                entry.push(member);
            }
        }
    }

    pub fn delete(&mut self, supergroup: u16) {
        self.groups.remove(&supergroup);
    }

    /// Every talkgroup a grant for `supergroup` should be considered for:
    /// the supergroup itself plus all transitively patched members.
    pub fn grant_targets(&self, supergroup: u16) -> Vec<u16> {
        let mut targets = vec![supergroup];
        let mut visited = HashSet::new();
        self.collect_members(supergroup, &mut visited, &mut targets);
        targets.sort_unstable();
        targets.dedup();
        targets
    }

    fn collect_members(&self, group: u16, visited: &mut HashSet<u16>, targets: &mut Vec<u16>) {
        if !visited.insert(group) {
            return;
        }
        let Some(members) = self.groups.get(&group) else {
            return;
        };
        for member in members {
            if *member == group {
                continue;
            }
            targets.push(*member);
            self.collect_members(*member, visited, targets);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_nested_members_without_cycles() {
        let mut patches = PatchRegistry::default();
        patches.add(50_213, [50_215, 50_213]);
        patches.add(50_215, [44_455, 44_458, 44_455]);

        assert_eq!(
            patches.grant_targets(50_213),
            vec![44_455, 44_458, 50_213, 50_215]
        );

        patches.delete(50_215);
        assert_eq!(patches.grant_targets(50_213), vec![50_213, 50_215]);
    }

    #[test]
    fn ignores_zero_members_and_duplicates() {
        let mut patches = PatchRegistry::default();
        patches.add(100, [0, 7, 7]);
        assert_eq!(patches.grant_targets(100), vec![7, 100]);
        assert_eq!(patches.grant_targets(999), vec![999]);
    }
}
