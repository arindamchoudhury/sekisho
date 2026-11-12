//! Who may use an environment, and who is an admin.

use std::collections::BTreeSet;

/// What decides a person's access.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Access {
    /// An admin disabled the person, or they were offboarded.
    pub disabled: bool,
    /// The identity provider refused them at the last check.
    pub idp_refused: bool,
    /// The identity provider's groups and the person's Sekisho groups.
    pub groups: BTreeSet<String>,
}

/// Whether the person may use an environment with these access groups: not
/// disabled, not refused, and in at least one of the groups. An environment
/// with no access groups is open to no one.
pub fn can_use(access: &Access, access_groups: &[String]) -> bool {
    !access.disabled
        && !access.idp_refused
        && access_groups.iter().any(|g| access.groups.contains(g))
}

/// Whether the person is an admin: marked as one, or in one of the admin
/// groups. Never while disabled.
pub fn is_admin(marked: bool, access: &Access, admin_groups: &[String]) -> bool {
    !access.disabled && (marked || admin_groups.iter().any(|g| access.groups.contains(g)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(groups: &[&str]) -> Access {
        Access {
            disabled: false,
            idp_refused: false,
            groups: groups.iter().map(|g| g.to_string()).collect(),
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|g| g.to_string()).collect()
    }

    #[test]
    fn access_needs_a_shared_group() {
        let a = access(&["a", "b"]);
        assert!(can_use(&a, &strings(&["b"])));
        assert!(can_use(&a, &strings(&["c", "a"])));
        assert!(!can_use(&a, &[]));
        assert!(!can_use(&a, &strings(&["c"])));
        assert!(!can_use(&a, &strings(&["A"])), "group names match exactly");
        assert!(!can_use(&access(&[]), &strings(&["a"])));
        let disabled = Access {
            disabled: true,
            ..a.clone()
        };
        assert!(!can_use(&disabled, &strings(&["a"])));
        let refused = Access {
            idp_refused: true,
            ..a.clone()
        };
        assert!(!can_use(&refused, &strings(&["a"])));
    }

    #[test]
    fn admins_by_mark_or_group() {
        let ops = access(&["ops"]);
        assert!(is_admin(true, &access(&[]), &[]));
        assert!(is_admin(false, &ops, &strings(&["ops"])));
        assert!(is_admin(false, &ops, &strings(&["admins", "ops"])));
        assert!(!is_admin(false, &ops, &strings(&["admins"])));
        assert!(!is_admin(false, &ops, &[]));
        let disabled = Access {
            disabled: true,
            ..ops.clone()
        };
        assert!(!is_admin(true, &disabled, &strings(&["ops"])));
        assert!(!is_admin(false, &disabled, &strings(&["ops"])));
    }
}
