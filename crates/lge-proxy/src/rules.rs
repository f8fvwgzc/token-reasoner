//! The rules the proxy can apply to a tool output, the mode each one runs in,
//! and what is counted about them.
//!
//! Every rule has a mode: `off` (not run), `shadow` (run, its would-be result
//! counted, the original forwarded) or `on` (its result is sent). A mode only
//! applies to items decided after it was set: an item decided once is replayed
//! with the same bytes on every later turn, whatever the modes say then.

use serde::{Deserialize, Serialize};

/// One shortening rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleId {
    /// A test, lint, type-check or build run reduced to its result.
    Check,
    /// A long search result capped, every file left out named.
    SearchCap,
    /// A large output identical to an earlier one replaced by a pointer to it.
    RepeatPointer,
    /// A file read whose bodies the task does not involve left out.
    Skeleton,
    /// Terminal noise removed and runs of identical lines collapsed.
    Inert,
    /// A path list with each folder written once.
    PathFold,
    /// JSON without the whitespace between its tokens.
    JsonMinify,
    /// Very long search match lines clipped around the match.
    LineClip,
    /// Progress lines of installs, downloads and builds left out.
    Noise,
    /// A lock file shown as its package list.
    Lockfile,
    /// A re-run shown as what changed since the earlier run. Measured only.
    RerunDelta,
    /// A re-read shown as what changed since the earlier read. Measured only.
    ReadDelta,
}

/// How a rule's result relates to what it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    /// Something is left out; the marker says how to get it back.
    Lossy,
    /// The original can be rebuilt from the result exactly.
    Lossless,
    /// Never sent in this version: only what it would save is measured.
    MeasureOnly,
}

impl RuleId {
    /// Every rule, in a fixed order.
    pub const ALL: [RuleId; 12] = [
        RuleId::Check,
        RuleId::SearchCap,
        RuleId::RepeatPointer,
        RuleId::Skeleton,
        RuleId::Inert,
        RuleId::PathFold,
        RuleId::JsonMinify,
        RuleId::LineClip,
        RuleId::Noise,
        RuleId::Lockfile,
        RuleId::RerunDelta,
        RuleId::ReadDelta,
    ];

    /// The rule's id in settings, stats and on the page.
    pub fn as_str(self) -> &'static str {
        match self {
            RuleId::Check => "check",
            RuleId::SearchCap => "search_cap",
            RuleId::RepeatPointer => "repeat_pointer",
            RuleId::Skeleton => "skeleton",
            RuleId::Inert => "inert",
            RuleId::PathFold => "path_fold",
            RuleId::JsonMinify => "json_minify",
            RuleId::LineClip => "line_clip",
            RuleId::Noise => "noise",
            RuleId::Lockfile => "lockfile",
            RuleId::RerunDelta => "rerun_delta",
            RuleId::ReadDelta => "read_delta",
        }
    }

    /// The rule with this id.
    pub fn parse(id: &str) -> Option<RuleId> {
        RuleId::ALL.into_iter().find(|r| r.as_str() == id)
    }

    pub fn kind(self) -> RuleKind {
        match self {
            RuleId::Check
            | RuleId::SearchCap
            | RuleId::Skeleton
            | RuleId::LineClip
            | RuleId::Noise
            | RuleId::Lockfile => RuleKind::Lossy,
            RuleId::Inert | RuleId::PathFold | RuleId::JsonMinify | RuleId::RepeatPointer => RuleKind::Lossless,
            RuleId::RerunDelta | RuleId::ReadDelta => RuleKind::MeasureOnly,
        }
    }

    pub fn is_lossy(self) -> bool {
        self.kind() == RuleKind::Lossy
    }

    /// Never sent: `on` is treated as `shadow`.
    pub fn measure_only(self) -> bool {
        self.kind() == RuleKind::MeasureOnly
    }

    /// Later calls watched for a recovery after an item this rule shortened.
    pub fn follow_up_calls(self) -> usize {
        if self == RuleId::Skeleton {
            SKELETON_FOLLOW_UP_CALLS
        } else {
            FOLLOW_UP_CALLS
        }
    }
}

impl std::fmt::Display for RuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Calls after a shortened item in which a re-run, a read of a range a marker
/// named, or a search of a file a note listed counts as a recovery.
pub const FOLLOW_UP_CALLS: usize = 3;
/// The same for a skeleton, where a patch of the read file also counts.
pub const SKELETON_FOLLOW_UP_CALLS: usize = 5;
/// A rule's recovery rate is judged only over at least this many items.
pub const MIN_JUDGED: u64 = 200;
/// A rule in `on` whose recovery rate is above this is moved to `shadow` for
/// new items.
pub const DEMOTE_ABOVE: f64 = 0.15;
/// A rule whose recovery rate is below this is healthy.
pub const HEALTHY_BELOW: f64 = 0.05;

/// What a rule does with a new item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Not run.
    Off,
    /// Run and counted; the original is sent.
    Shadow,
    /// Its result is sent.
    On,
}

fn on() -> Mode {
    Mode::On
}

fn shadow() -> Mode {
    Mode::Shadow
}

/// The mode of every rule. A missing field takes its default, so settings
/// saved by an older version still load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleModes {
    #[serde(default = "on")]
    pub check: Mode,
    #[serde(default = "on")]
    pub search_cap: Mode,
    #[serde(default = "on")]
    pub repeat_pointer: Mode,
    #[serde(default = "shadow")]
    pub skeleton: Mode,
    #[serde(default = "on")]
    pub inert: Mode,
    #[serde(default = "on")]
    pub path_fold: Mode,
    #[serde(default = "on")]
    pub json_minify: Mode,
    #[serde(default = "shadow")]
    pub line_clip: Mode,
    #[serde(default = "shadow")]
    pub noise: Mode,
    #[serde(default = "shadow")]
    pub lockfile: Mode,
    #[serde(default = "shadow")]
    pub rerun_delta: Mode,
    #[serde(default = "shadow")]
    pub read_delta: Mode,
}

impl Default for RuleModes {
    fn default() -> Self {
        RuleModes {
            check: Mode::On,
            search_cap: Mode::On,
            repeat_pointer: Mode::On,
            skeleton: Mode::Shadow,
            inert: Mode::On,
            path_fold: Mode::On,
            json_minify: Mode::On,
            line_clip: Mode::Shadow,
            noise: Mode::Shadow,
            lockfile: Mode::Shadow,
            rerun_delta: Mode::Shadow,
            read_delta: Mode::Shadow,
        }
    }
}

impl RuleModes {
    /// The mode set for `rule`, as written (a measure-only rule may say `on`).
    pub fn get(&self, rule: RuleId) -> Mode {
        *self.slot(rule)
    }

    pub fn set(&mut self, rule: RuleId, mode: Mode) {
        *self.slot_mut(rule) = mode;
    }

    /// Every rule set to `mode`.
    pub fn all(mode: Mode) -> RuleModes {
        let mut modes = RuleModes::default();
        for rule in RuleId::ALL {
            modes.set(rule, mode);
        }
        modes
    }

    fn slot(&self, rule: RuleId) -> &Mode {
        match rule {
            RuleId::Check => &self.check,
            RuleId::SearchCap => &self.search_cap,
            RuleId::RepeatPointer => &self.repeat_pointer,
            RuleId::Skeleton => &self.skeleton,
            RuleId::Inert => &self.inert,
            RuleId::PathFold => &self.path_fold,
            RuleId::JsonMinify => &self.json_minify,
            RuleId::LineClip => &self.line_clip,
            RuleId::Noise => &self.noise,
            RuleId::Lockfile => &self.lockfile,
            RuleId::RerunDelta => &self.rerun_delta,
            RuleId::ReadDelta => &self.read_delta,
        }
    }

    fn slot_mut(&mut self, rule: RuleId) -> &mut Mode {
        match rule {
            RuleId::Check => &mut self.check,
            RuleId::SearchCap => &mut self.search_cap,
            RuleId::RepeatPointer => &mut self.repeat_pointer,
            RuleId::Skeleton => &mut self.skeleton,
            RuleId::Inert => &mut self.inert,
            RuleId::PathFold => &mut self.path_fold,
            RuleId::JsonMinify => &mut self.json_minify,
            RuleId::LineClip => &mut self.line_clip,
            RuleId::Noise => &mut self.noise,
            RuleId::Lockfile => &mut self.lockfile,
            RuleId::RerunDelta => &mut self.rerun_delta,
            RuleId::ReadDelta => &mut self.read_delta,
        }
    }
}

/// What one rule did in one conversation. Each item is counted once, when it
/// is first decided.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleStats {
    /// Items whose sent text this rule changed, and their tokens before and after it.
    pub items: u64,
    pub tokens_before: u64,
    pub tokens_after: u64,
    /// Items it would have changed in `shadow`, and their tokens before and after.
    pub shadow_items: u64,
    pub shadow_tokens_before: u64,
    pub shadow_tokens_after: u64,
    /// Items it changed whose follow-up calls were looked at, and how many of
    /// them were followed by a recovery (a re-run, a read of a range a marker
    /// named, ...).
    pub judged: u64,
    pub recovered: u64,
    /// The same for items it would have changed in `shadow`.
    pub shadow_judged: u64,
    pub shadow_followed: u64,
}

impl RuleStats {
    /// Each counter of `self` plus the one of `other`.
    pub fn plus(&self, other: &RuleStats) -> RuleStats {
        RuleStats {
            items: self.items.saturating_add(other.items),
            tokens_before: self.tokens_before.saturating_add(other.tokens_before),
            tokens_after: self.tokens_after.saturating_add(other.tokens_after),
            shadow_items: self.shadow_items.saturating_add(other.shadow_items),
            shadow_tokens_before: self.shadow_tokens_before.saturating_add(other.shadow_tokens_before),
            shadow_tokens_after: self.shadow_tokens_after.saturating_add(other.shadow_tokens_after),
            judged: self.judged.saturating_add(other.judged),
            recovered: self.recovered.saturating_add(other.recovered),
            shadow_judged: self.shadow_judged.saturating_add(other.shadow_judged),
            shadow_followed: self.shadow_followed.saturating_add(other.shadow_followed),
        }
    }

    /// Recovered over judged items; `None` before anything was judged.
    pub fn recovery_rate(&self) -> Option<f64> {
        (self.judged > 0).then(|| self.recovered as f64 / self.judged as f64)
    }

    /// Each counter of `self` less the one of `other`, never below zero.
    pub fn minus(&self, other: &RuleStats) -> RuleStats {
        RuleStats {
            items: self.items.saturating_sub(other.items),
            tokens_before: self.tokens_before.saturating_sub(other.tokens_before),
            tokens_after: self.tokens_after.saturating_sub(other.tokens_after),
            shadow_items: self.shadow_items.saturating_sub(other.shadow_items),
            shadow_tokens_before: self.shadow_tokens_before.saturating_sub(other.shadow_tokens_before),
            shadow_tokens_after: self.shadow_tokens_after.saturating_sub(other.shadow_tokens_after),
            judged: self.judged.saturating_sub(other.judged),
            recovered: self.recovered.saturating_sub(other.recovered),
            shadow_judged: self.shadow_judged.saturating_sub(other.shadow_judged),
            shadow_followed: self.shadow_followed.saturating_sub(other.shadow_followed),
        }
    }
}

/// First reads of indexed files of one language and size band, for the
/// skeleton rule: how many there were, what a skeleton would have saved, and
/// how many were followed by a patch of the file within
/// [`SKELETON_FOLLOW_UP_CALLS`] calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CohortStats {
    pub reads: u64,
    pub would_save_tokens: u64,
    pub patched_within_5: u64,
}

impl CohortStats {
    /// Each counter of `self` plus the one of `other`.
    pub fn plus(&self, other: &CohortStats) -> CohortStats {
        CohortStats {
            reads: self.reads.saturating_add(other.reads),
            would_save_tokens: self.would_save_tokens.saturating_add(other.would_save_tokens),
            patched_within_5: self.patched_within_5.saturating_add(other.patched_within_5),
        }
    }
}

const KB: usize = 1024;
const MB: usize = 1024 * 1024;

/// The size bands of read outputs, by bytes: `[from, to)`, the last one up to
/// and including 2 MB. Reads outside them are not counted in a cohort.
pub const BANDS: [(usize, usize, &str); 4] = [
    (16 * KB, 64 * KB, "16-64K"),
    (64 * KB, 256 * KB, "64-256K"),
    (256 * KB, MB, "256K-1M"),
    (MB, 2 * MB + 1, "1-2M"),
];

/// The cohort of a read of `path` whose output has `bytes` bytes:
/// `{language}:{band}`, the language as `lge_index` names it by the file's
/// extension. `None` for a file lge does not index or a size outside the bands.
pub fn cohort_key(path: &str, bytes: usize) -> Option<String> {
    let lang = lge_index::lang_of(path)?;
    let band = BANDS.iter().find(|(from, to, _)| (*from..*to).contains(&bytes))?.2;
    Some(format!("{lang}:{band}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_kinds_are_as_documented() {
        for rule in RuleId::ALL {
            assert_eq!(RuleId::parse(rule.as_str()), Some(rule));
            assert_eq!(
                serde_json::to_value(rule).unwrap(),
                serde_json::Value::String(rule.as_str().into())
            );
        }
        let lossy: Vec<&str> = RuleId::ALL
            .iter()
            .filter(|r| r.is_lossy())
            .map(|r| r.as_str())
            .collect();
        assert_eq!(
            lossy,
            ["check", "search_cap", "skeleton", "line_clip", "noise", "lockfile"]
        );
        assert!(RuleId::RerunDelta.measure_only() && RuleId::ReadDelta.measure_only());
        assert_eq!(RuleId::Skeleton.follow_up_calls(), 5);
        assert_eq!(RuleId::Check.follow_up_calls(), 3);
        assert_eq!(RuleId::parse("nope"), None);
    }

    #[test]
    fn modes_default_per_field_and_old_settings_load() {
        let modes: RuleModes = serde_json::from_str("{}").unwrap();
        assert_eq!(modes, RuleModes::default());
        let partial: RuleModes = serde_json::from_str(r#"{"skeleton":"on","check":"off","future":"x"}"#).unwrap();
        assert_eq!(partial.skeleton, Mode::On);
        assert_eq!(partial.check, Mode::Off);
        assert_eq!(partial.search_cap, Mode::On);
        assert_eq!(partial.noise, Mode::Shadow);
        let json = serde_json::to_value(RuleModes::default()).unwrap();
        assert_eq!(json["skeleton"], "shadow");
        assert_eq!(json["check"], "on");
        for rule in RuleId::ALL {
            let mut m = RuleModes::default();
            m.set(rule, Mode::Off);
            assert_eq!(m.get(rule), Mode::Off);
        }
        assert!(RuleId::ALL.iter().all(|r| RuleModes::all(Mode::On).get(*r) == Mode::On));
    }

    #[test]
    fn cohorts_are_keyed_by_language_and_band() {
        assert_eq!(cohort_key("src/app.py", 20 * KB).as_deref(), Some("python:16-64K"));
        assert_eq!(cohort_key("src/app.py", 16 * KB).as_deref(), Some("python:16-64K"));
        assert_eq!(cohort_key("src/app.py", 64 * KB).as_deref(), Some("python:64-256K"));
        assert_eq!(cohort_key("a/b.ts", 300 * KB).as_deref(), Some("typescript:256K-1M"));
        assert_eq!(cohort_key("a/b.ts", 2 * MB).as_deref(), Some("typescript:1-2M"));
        assert_eq!(cohort_key("a/b.ts", 2 * MB + 1), None);
        assert_eq!(cohort_key("src/app.py", 16 * KB - 1), None);
        assert_eq!(cohort_key("notes.unknownext", 20 * KB), None);
    }
}
