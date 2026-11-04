//! Tool profiles (R4-02): `lite` / `standard` / `full` / `admin`.
//!
//! A profile is a *preset* over the existing configuration — it sets the write
//! mode, `--full-tools`, `--allow-grants` and an approval default. It adds no
//! second authorization system: every profile expands to flags plus ordinary
//! grant TOML, and the unit tests assert exactly that equivalence.

use crate::auth::Tier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub name: &'static str,
    /// Tier granted to a principal when the profile is used as a preset.
    pub max_tier: Tier,
    pub full_tools: bool,
    pub allow_grants: bool,
    pub require_approval: bool,
}

pub const PROFILES: &[Profile] = &[
    Profile {
        name: "lite",
        max_tier: Tier::Read,
        full_tools: false,
        allow_grants: false,
        require_approval: false,
    },
    Profile {
        name: "standard",
        max_tier: Tier::Read,
        full_tools: true,
        allow_grants: false,
        require_approval: false,
    },
    Profile {
        name: "full",
        max_tier: Tier::Write,
        full_tools: true,
        allow_grants: false,
        require_approval: false,
    },
    Profile {
        name: "admin",
        max_tier: Tier::Ddl,
        full_tools: true,
        allow_grants: true,
        require_approval: true,
    },
];

pub fn parse(name: &str) -> Option<Profile> {
    PROFILES
        .iter()
        .copied()
        .find(|profile| profile.name.eq_ignore_ascii_case(name))
}

/// The grant TOML a profile is equivalent to. Used by `docs/PROFILES.md` and
/// asserted by tests, so a preset can never drift into "more than grants".
pub fn grant_toml(profile: Profile) -> String {
    format!(
        "[[grants]]\nprincipal = \"<principal>\"\nsource = \"*\"\nmax_tier = \"{}\"\n",
        profile.max_tier.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TomlConfig;

    #[test]
    fn every_profile_parses_and_expands_to_grant_toml() {
        for profile in PROFILES {
            assert_eq!(parse(profile.name), Some(*profile));
            let toml = grant_toml(*profile);
            let config: TomlConfig = toml::from_str(&toml).expect("grant TOML parses");
            let grant = config.grants.first().expect("one grant");
            assert_eq!(grant.principal, "<principal>");
            assert_eq!(grant.source, "*");
            assert_eq!(grant.max_tier, profile.max_tier.as_str());
        }
        assert!(parse("nope").is_none());
    }

    #[test]
    fn profiles_are_strictly_ordered_by_capability() {
        let lite = parse("lite").unwrap();
        let standard = parse("standard").unwrap();
        let full = parse("full").unwrap();
        let admin = parse("admin").unwrap();
        assert!(lite.max_tier <= standard.max_tier && standard.max_tier <= full.max_tier);
        assert!(full.max_tier <= admin.max_tier);
        assert!(!lite.full_tools && standard.full_tools && full.full_tools);
        assert!(!admin.allow_grants || !full.allow_grants);
    }

    #[test]
    fn admin_is_the_only_profile_that_opens_grants_and_approvals() {
        for profile in PROFILES {
            let is_admin = profile.name == "admin";
            assert_eq!(profile.allow_grants, is_admin);
            assert_eq!(profile.require_approval, is_admin);
        }
    }
}
