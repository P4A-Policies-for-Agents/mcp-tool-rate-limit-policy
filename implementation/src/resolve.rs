// Copyright 2026 Salesforce, Inc. All rights reserved.
//
//! Per-tool rate-limit tier resolution.
//!
//! At configure time the operator-supplied regexes (from `unmeteredTools` and
//! `toolOverrides`) are compiled ONCE into a [`ToolResolver`]. On each request
//! the resolver maps an MCP tool name to a [`Resolution`], which the request
//! filter turns into either a passthrough or a rate-limit bucket lookup.
//!
//! Resolution order (see `docs/spec.md`):
//!   1. `unmeteredTools` in list order — first match → [`Resolution::Unmetered`].
//!   2. `toolOverrides` in list order — first match → [`Resolution::Metered`]
//!      with that entry's tier and group id.
//!   3. Otherwise the default tier → [`Resolution::Metered`] with the default
//!      group id.
//!
//! The bucket GROUP id encodes the tier (`max` + `period`) so that a config
//! change to a limit forces a fresh bucket rather than reusing a stale window.

use crate::generated::config::Config;
use anyhow::{anyhow, Context};
use regex::Regex;

/// The bucket group id for the default tier.
pub const DEFAULT_GROUP: &str = "default";

/// A rate-limit tier: a request ceiling over a rolling window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
    pub max_requests: u64,
    pub period_in_millis: u64,
}

/// The outcome of resolving a tool name against the compiled configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution<'a> {
    /// Tool matched an `unmeteredTools` entry — bypass rate limiting entirely.
    Unmetered,
    /// Tool is rate-limited under the given bucket group and tier.
    Metered { group: &'a str, tier: Tier },
}

/// A single compiled override: its anchored regex, bucket group id, and tier.
struct CompiledOverride {
    regex: Regex,
    group: String,
    tier: Tier,
}

/// Compiled, request-time-ready view of the tool rate-limit configuration.
///
/// Built once at configure time and shared (via `Arc`) with every request. The
/// default tier is always present; the override and unmetered lists preserve
/// operator list order so that first-match-wins semantics hold.
pub struct ToolResolver {
    default_tier: Tier,
    /// Tier-encoded bucket group id for the default tier, interned once so
    /// `resolve` can return a borrow of it.
    default_group: String,
    overrides: Vec<CompiledOverride>,
    unmetered: Vec<Regex>,
}

/// Compile a single tool-name regex as an anchored full-match.
///
/// Wrapping in `^(?:PATTERN)$` guarantees the whole tool name matches, so
/// `get_.*` matches `get_x` but not `xget_x`. The inner non-capturing group
/// keeps top-level alternations (`a|b`) anchored as a unit.
fn compile_anchored(pattern: &str, source: &str) -> anyhow::Result<Regex> {
    let anchored = format!("^(?:{})$", pattern);
    Regex::new(&anchored)
        .with_context(|| format!("invalid {} regex: {:?}", source, pattern))
}

impl ToolResolver {
    /// Compile all operator regexes from the policy config.
    ///
    /// Returns a hard error if ANY override or unmetered pattern fails to
    /// compile — the policy must fail loud at configure time rather than serve
    /// traffic with a silently-dropped rule.
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let overrides: Vec<(String, i64, i64)> = config
            .tool_overrides
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|o| {
                (
                    o.tool_name,
                    o.maximum_requests,
                    o.time_period_in_milliseconds,
                )
            })
            .collect();
        Self::from_parts(
            config.maximum_requests,
            config.time_period_in_milliseconds,
            &overrides,
            &config.unmetered_tools.clone().unwrap_or_default(),
        )
    }

    /// Config-shape-independent core of [`from_config`], so the regex/tier
    /// validation is unit-testable without constructing a full generated
    /// [`Config`] (whose `keySelector` is a compiled `Script`).
    ///
    /// `overrides` items are `(pattern, max_requests, period_in_millis)` in
    /// operator list order.
    fn from_parts(
        default_max: i64,
        default_period: i64,
        overrides: &[(String, i64, i64)],
        unmetered_patterns: &[String],
    ) -> anyhow::Result<Self> {
        let default_tier = Tier {
            max_requests: u64::try_from(default_max)
                .map_err(|_| anyhow!("maximumRequests must be non-negative"))?,
            period_in_millis: u64::try_from(default_period)
                .map_err(|_| anyhow!("timePeriodInMilliseconds must be non-negative"))?,
        };

        let unmetered = unmetered_patterns
            .iter()
            .map(|p| compile_anchored(p, "unmeteredTools"))
            .collect::<anyhow::Result<Vec<_>>>()?;

        let overrides = overrides
            .iter()
            .enumerate()
            .map(|(i, (pattern, max, period))| {
                let regex = compile_anchored(pattern, "toolOverrides")?;
                let tier = Tier {
                    max_requests: u64::try_from(*max).map_err(|_| {
                        anyhow!("toolOverrides[{}].maximumRequests must be non-negative", i)
                    })?,
                    period_in_millis: u64::try_from(*period).map_err(|_| {
                        anyhow!(
                            "toolOverrides[{}].timePeriodInMilliseconds must be non-negative",
                            i
                        )
                    })?,
                };
                // Group id encodes the tier so a limit change forces a fresh
                // bucket (no stale-window reuse across a re-tiering).
                let group = format!("tool:{}:{}:{}", i, tier.max_requests, tier.period_in_millis);
                Ok(CompiledOverride { regex, group, tier })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let default_group = format!(
            "{}:{}:{}",
            DEFAULT_GROUP, default_tier.max_requests, default_tier.period_in_millis
        );

        Ok(Self {
            default_tier,
            default_group,
            overrides,
            unmetered,
        })
    }

    /// The default bucket group id (tier-encoded). Used by tests to assert the
    /// tier-encoding; production reads the field via [`bucket_specs`] and
    /// [`resolve`].
    #[cfg(test)]
    pub fn default_group(&self) -> &str {
        &self.default_group
    }

    /// Every (group, tier) pair that must be registered as a rate-limit
    /// bucket at configure time: the default plus one per override entry.
    pub fn bucket_specs(&self) -> Vec<(String, Tier)> {
        let mut specs = Vec::with_capacity(self.overrides.len() + 1);
        specs.push((self.default_group.clone(), self.default_tier));
        for o in &self.overrides {
            specs.push((o.group.clone(), o.tier));
        }
        specs
    }

    /// Resolve a tool name to its rate-limit decision. The returned `group`
    /// borrows from `self` (either an override's group or the interned default
    /// group), so no allocation happens on the request hot path.
    pub fn resolve(&self, tool_name: &str) -> Resolution<'_> {
        // 1. Unmetered wins, checked first, in list order.
        for re in &self.unmetered {
            if re.is_match(tool_name) {
                return Resolution::Unmetered;
            }
        }
        // 2. First matching override in list order.
        for o in &self.overrides {
            if o.regex.is_match(tool_name) {
                return Resolution::Metered {
                    group: &o.group,
                    tier: o.tier,
                };
            }
        }
        // 3. Default tier.
        Resolution::Metered {
            group: &self.default_group,
            tier: self.default_tier,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The generated `Config` needs a compiled `keySelector` Script we cannot
    // build without the scripting engine at unit-test time, so these tests
    // construct `ToolResolver` fields directly via a small helper that mirrors
    // `from_config` for the regex/tier portions.
    fn resolver(
        default_tier: Tier,
        overrides: Vec<(&str, u64, u64)>,
        unmetered: Vec<&str>,
    ) -> ToolResolver {
        let overrides = overrides
            .into_iter()
            .enumerate()
            .map(|(i, (pat, max, per))| {
                let tier = Tier {
                    max_requests: max,
                    period_in_millis: per,
                };
                CompiledOverride {
                    regex: compile_anchored(pat, "test").unwrap(),
                    group: format!("tool:{}:{}:{}", i, max, per),
                    tier,
                }
            })
            .collect();
        let unmetered = unmetered
            .into_iter()
            .map(|p| compile_anchored(p, "test").unwrap())
            .collect();
        let default_group = format!(
            "{}:{}:{}",
            DEFAULT_GROUP, default_tier.max_requests, default_tier.period_in_millis
        );
        ToolResolver {
            default_tier,
            default_group,
            overrides,
            unmetered,
        }
    }

    const DEF: Tier = Tier {
        max_requests: 100,
        period_in_millis: 60_000,
    };

    #[test]
    fn empty_arrays_resolve_to_default_backcompat() {
        let r = resolver(DEF, vec![], vec![]);
        assert_eq!(
            r.resolve("anything"),
            Resolution::Metered {
                group: "default:100:60000",
                tier: DEF
            }
        );
        // Only the default bucket is registered.
        assert_eq!(r.bucket_specs().len(), 1);
    }

    #[test]
    fn unmetered_beats_override_and_default() {
        // Same tool matches both an unmetered entry and an override; unmetered
        // is checked first and wins.
        let r = resolver(DEF, vec![("get_.*", 5, 1000)], vec!["get_.*"]);
        assert_eq!(r.resolve("get_customer"), Resolution::Unmetered);
    }

    #[test]
    fn override_used_when_no_unmetered_match() {
        let r = resolver(DEF, vec![("validate_binding", 10, 30_000)], vec![]);
        match r.resolve("validate_binding") {
            Resolution::Metered { group, tier } => {
                assert_eq!(group, "tool:0:10:30000");
                assert_eq!(
                    tier,
                    Tier {
                        max_requests: 10,
                        period_in_millis: 30_000
                    }
                );
            }
            other => panic!("expected metered override, got {:?}", other),
        }
    }

    #[test]
    fn regex_is_anchored_full_match() {
        let r = resolver(DEF, vec![("get_.*", 5, 1000)], vec![]);
        // Matches full name...
        assert!(matches!(
            r.resolve("get_x"),
            Resolution::Metered { group, .. } if group == "tool:0:5:1000"
        ));
        // ...but NOT a suffix match (xget_x). Falls through to default.
        assert!(matches!(
            r.resolve("xget_x"),
            Resolution::Metered { group, .. } if group == "default:100:60000"
        ));
    }

    #[test]
    fn first_matching_override_wins_for_overlapping_patterns() {
        let r = resolver(
            DEF,
            vec![("get_.*", 5, 1000), ("get_customer.*", 999, 99_000)],
            vec![],
        );
        // Both patterns match "get_customer_serials"; the FIRST in list wins.
        match r.resolve("get_customer_serials") {
            Resolution::Metered { group, tier } => {
                assert_eq!(group, "tool:0:5:1000");
                assert_eq!(tier.max_requests, 5);
            }
            other => panic!("expected first override, got {:?}", other),
        }
    }

    #[test]
    fn per_tool_isolation_shares_group_not_key() {
        // Two distinct tools under ONE regex override entry share the SAME
        // group (tier) but — because the keySelector folds vars.toolName into
        // the bucket KEY — they get independent windows. The resolver's job is
        // only to prove they resolve to the same group; key isolation is
        // exercised end-to-end in the lib.rs integration tests.
        let r = resolver(DEF, vec![("tool_.*", 3, 5000)], vec![]);
        let a = r.resolve("tool_a");
        let b = r.resolve("tool_b");
        assert_eq!(a, b, "same regex entry => same group/tier");
        assert!(matches!(
            a,
            Resolution::Metered { group, .. } if group == "tool:0:3:5000"
        ));
    }

    #[test]
    fn group_id_changes_when_tier_changes() {
        // A re-tiering (limit change) must produce a different group id so the
        // rate limiter allocates a fresh bucket instead of reusing a stale
        // window under the old count.
        let r1 = resolver(DEF, vec![("t", 5, 1000)], vec![]);
        let r2 = resolver(DEF, vec![("t", 6, 1000)], vec![]);
        let group1 = match r1.resolve("t") {
            Resolution::Metered { group, .. } => group.to_string(),
            _ => unreachable!(),
        };
        let group2 = match r2.resolve("t") {
            Resolution::Metered { group, .. } => group.to_string(),
            _ => unreachable!(),
        };
        assert_ne!(group1, group2, "tier change must change group id");
        assert_eq!(group1, "tool:0:5:1000");
        assert_eq!(group2, "tool:0:6:1000");
    }

    #[test]
    fn default_group_encodes_default_tier() {
        let r = resolver(DEF, vec![], vec![]);
        assert_eq!(r.default_group(), "default:100:60000");
    }

    #[test]
    fn from_parts_rejects_invalid_override_regex() {
        // "get_[" is an unterminated character class.
        let result =
            ToolResolver::from_parts(100, 60_000, &[("get_[".to_string(), 5, 1000)], &[]);
        let err = result.err().expect("must reject invalid regex");
        assert!(
            err.to_string().contains("toolOverrides"),
            "error should name the offending source; got: {}",
            err
        );
    }

    #[test]
    fn from_parts_rejects_invalid_unmetered_regex() {
        // "(" is an unbalanced parenthesis.
        let result = ToolResolver::from_parts(100, 60_000, &[], &["(".to_string()]);
        let err = result.err().expect("must reject invalid regex");
        assert!(
            err.to_string().contains("unmeteredTools"),
            "error should name the offending source; got: {}",
            err
        );
    }

    #[test]
    fn from_parts_compiles_valid_config() {
        let r = ToolResolver::from_parts(
            50,
            30_000,
            &[("admin_.*".to_string(), 5, 1000)],
            &["health".to_string()],
        )
        .expect("valid config must compile");
        assert_eq!(r.resolve("health"), Resolution::Unmetered);
        assert!(matches!(
            r.resolve("admin_reset"),
            Resolution::Metered { group, .. } if group == "tool:0:5:1000"
        ));
    }

    #[test]
    fn bucket_specs_include_default_and_each_override() {
        let r = resolver(
            DEF,
            vec![("a", 1, 1000), ("b", 2, 2000)],
            vec!["u"], // unmetered has no bucket
        );
        let specs = r.bucket_specs();
        assert_eq!(specs.len(), 3, "default + 2 overrides; unmetered has no bucket");
        assert_eq!(specs[0].0, "default:100:60000");
        assert_eq!(specs[1].0, "tool:0:1:1000");
        assert_eq!(specs[2].0, "tool:1:2:2000");
    }
}
