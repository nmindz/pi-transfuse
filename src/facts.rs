//! What listings and pickers sort and filter sessions by, live or archived.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use clap::{Args, ValueEnum};

use crate::{paths, Side};

/// Every harness, in listing order.
pub const ALL_SIDES: [Side; 3] = [Side::Pi, Side::Dsh, Side::Claude];

/// The sortable, filterable facts of one session.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub harness: Side,
    pub id: String,
    pub title: String,
    /// Working directory the session ran in.
    pub path: String,
    /// Name of that directory: the project.
    pub project: String,
    pub created_ms: i64,
    pub last_used_ms: i64,
    pub bytes: u64,
    /// Archive size, for archived sessions.
    pub archive_bytes: Option<u64>,
}

impl Facts {
    /// True when the id, title, or path contains `needle`, given in lowercase.
    pub fn mentions(&self, needle: &str) -> bool {
        [&self.id, &self.title, &self.path].iter().any(|field| contains_ci(field, needle))
    }
}

/// Project name of a working directory: its last component.
pub fn project_name(path: &str) -> String {
    Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// Case-insensitive substring test; `needle` is already lowercase.
fn contains_ci(hay: &str, needle: &str) -> bool {
    needle.is_empty()
        || hay.char_indices().any(|(i, _)| {
            let mut rest = hay[i..].chars().flat_map(char::to_lowercase);
            needle.chars().all(|c| rest.next() == Some(c))
        })
}

fn cmp_ci(a: &str, b: &str) -> Ordering {
    a.chars().flat_map(char::to_lowercase).cmp(b.chars().flat_map(char::to_lowercase))
}

/// What a listing is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SortKey {
    /// Last use, oldest first.
    #[value(alias = "age")]
    Used,
    /// Size on disk before archiving, largest first.
    Size,
    /// Archive size, largest first.
    Archive,
    /// Creation time, oldest first.
    Created,
    /// Working directory, A to Z.
    Path,
    /// Project folder name, A to Z.
    Project,
    /// Harness, A to Z.
    Harness,
    /// Title, A to Z.
    Title,
}

impl SortKey {
    pub const ALL: [SortKey; 8] = [
        SortKey::Used,
        SortKey::Size,
        SortKey::Archive,
        SortKey::Created,
        SortKey::Path,
        SortKey::Project,
        SortKey::Harness,
        SortKey::Title,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Used => "last used",
            SortKey::Size => "size",
            SortKey::Archive => "archive size",
            SortKey::Created => "created",
            SortKey::Path => "path",
            SortKey::Project => "project",
            SortKey::Harness => "harness",
            SortKey::Title => "title",
        }
    }

    /// Picker key that sorts by this.
    pub fn key(self) -> char {
        match self {
            SortKey::Used => 'u',
            SortKey::Size => 's',
            SortKey::Archive => 'z',
            SortKey::Created => 'c',
            SortKey::Path => 'd',
            SortKey::Project => 'n',
            SortKey::Harness => 'h',
            SortKey::Title => 't',
        }
    }

    fn natural(self, a: &Facts, b: &Facts) -> Ordering {
        match self {
            SortKey::Used => a.last_used_ms.cmp(&b.last_used_ms),
            SortKey::Size => b.bytes.cmp(&a.bytes),
            // Sessions without an archive go last.
            SortKey::Archive => b.archive_bytes.cmp(&a.archive_bytes),
            SortKey::Created => a.created_ms.cmp(&b.created_ms),
            SortKey::Path => cmp_ci(&a.path, &b.path),
            SortKey::Project => cmp_ci(&a.project, &b.project).then_with(|| cmp_ci(&a.path, &b.path)),
            SortKey::Harness => a.harness.key().cmp(b.harness.key()),
            SortKey::Title => cmp_ci(&a.title, &b.title),
        }
    }

    fn direction(self, reverse: bool) -> &'static str {
        match (self, reverse) {
            (SortKey::Used | SortKey::Created, false) => "oldest first",
            (SortKey::Used | SortKey::Created, true) => "newest first",
            (SortKey::Size | SortKey::Archive, false) => "largest first",
            (SortKey::Size | SortKey::Archive, true) => "smallest first",
            (_, false) => "A→Z",
            (_, true) => "Z→A",
        }
    }
}

/// An order: a key and whether it runs backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub reverse: bool,
}

impl Sort {
    pub fn compare(self, a: &Facts, b: &Facts) -> Ordering {
        let primary = self.key.natural(a, b);
        let primary = if self.reverse { primary.reverse() } else { primary };
        primary
            .then_with(|| a.last_used_ms.cmp(&b.last_used_ms))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.id.cmp(&b.id))
    }

    pub fn describe(self) -> String {
        format!("{}, {}", self.key.label(), self.key.direction(self.reverse))
    }

    /// Sorts by `key`; choosing the current key again flips the direction.
    pub fn toggled(self, key: SortKey) -> Sort {
        Sort { key, reverse: key == self.key && !self.reverse }
    }
}

/// Order switches of the listing commands and pickers.
#[derive(Debug, Args)]
pub struct SortArgs {
    /// Order by this.
    #[arg(long, value_enum)]
    pub sort: Option<SortKey>,
    /// Reverse the order.
    #[arg(long)]
    pub reverse: bool,
}

impl SortArgs {
    pub fn resolve(&self, default: SortKey) -> Sort {
        Sort { key: self.sort.unwrap_or(default), reverse: self.reverse }
    }
}

/// One `FIELD=VALUE[,VALUE…]` condition; it holds when any value matches.
#[derive(Debug, Clone, PartialEq)]
pub enum Rule {
    Harness(Vec<Side>),
    /// Project names, lowercase.
    Project(Vec<String>),
    /// Directories; a session matches at or below one. Keeps what was typed,
    /// to resolve again against another host's home.
    Path { typed: Vec<String>, dirs: Vec<PathBuf> },
}

impl FromStr for Rule {
    type Err = String;

    fn from_str(text: &str) -> Result<Rule, String> {
        let (field, values) = text
            .split_once('=')
            .ok_or_else(|| format!("{text:?}: use FIELD=VALUE[,VALUE…] with harness, project, or path"))?;
        let values: Vec<&str> = values.split(',').map(str::trim).filter(|v| !v.is_empty()).collect();
        if values.is_empty() {
            return Err(format!("{text:?} names no value"));
        }
        match field.trim().to_lowercase().as_str() {
            "harness" => values
                .iter()
                .map(|v| {
                    Side::from_str(v, true)
                        .map_err(|_| format!("unknown harness {v:?}: use pi (omp), dsh, or claude (claude-code)"))
                })
                .collect::<Result<_, _>>()
                .map(Rule::Harness),
            "project" => Ok(Rule::Project(values.iter().map(|v| v.to_lowercase()).collect())),
            "path" => Ok(Rule::Path {
                typed: values.iter().map(|v| v.to_string()).collect(),
                dirs: values.iter().flat_map(|v| path_forms(v)).collect(),
            }),
            other => Err(format!("unknown field {other:?}: use harness, project, or path")),
        }
    }
}

/// A directory as typed, with `~` expanded and made absolute, plus its
/// symlink-resolved form, since sessions record either.
fn path_forms(text: &str) -> Vec<PathBuf> {
    let expanded = match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => paths::home().join(rest.trim_start_matches('/')),
        _ => PathBuf::from(text),
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().map_or(expanded.clone(), |cwd| cwd.join(&expanded))
    };
    let mut out = vec![absolute.clone()];
    if let Ok(canon) = std::fs::canonicalize(&absolute) {
        if canon != absolute {
            out.push(canon);
        }
    }
    out
}

/// A directory as typed, resolved on another host: `~` is `home` there, and
/// a relative path is taken from that home, since this host's working
/// directory and symlinks mean nothing there.
fn remote_forms(text: &str, home: &str) -> PathBuf {
    let home = Path::new(home);
    match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => home.join(rest.trim_start_matches('/')),
        _ if Path::new(text).is_absolute() => PathBuf::from(text),
        _ => home.join(text),
    }
}

impl Rule {
    fn matches(&self, facts: &Facts) -> bool {
        match self {
            Rule::Harness(sides) => sides.contains(&facts.harness),
            Rule::Project(names) => names.iter().any(|n| cmp_ci(n, &facts.project) == Ordering::Equal),
            Rule::Path { dirs, .. } => dirs.iter().any(|d| Path::new(&facts.path).starts_with(d)),
        }
    }

    fn on_host(&self, home: &str) -> Rule {
        match self {
            Rule::Path { typed, .. } => Rule::Path {
                typed: typed.clone(),
                dirs: typed.iter().map(|t| remote_forms(t, home)).collect(),
            },
            other => other.clone(),
        }
    }
}

fn lowercase(text: &str) -> Result<String, String> {
    Ok(text.to_lowercase())
}

/// Which sessions a command considers.
#[derive(Debug, Default, Args)]
pub struct Scope {
    /// Keep only sessions matching FIELD=VALUE[,VALUE…]. Fields: harness (pi or
    /// omp, dsh, claude or claude-code), project (folder name), path (that
    /// directory and below). A session must match every --include.
    #[arg(long, value_name = "FIELD=VALUES")]
    pub include: Vec<Rule>,
    /// Leave out sessions matching FIELD=VALUE[,VALUE…], same fields as --include.
    #[arg(long, value_name = "FIELD=VALUES")]
    pub exclude: Vec<Rule>,
    /// Keep only sessions whose id, title, or path contains this text.
    #[arg(short, long, value_parser = lowercase)]
    pub filter: Option<String>,
    /// Leave out sessions without a title.
    #[arg(long)]
    pub hide_untitled: bool,
}

impl Scope {
    /// Harnesses the rules leave in play, so the others are not even read.
    pub fn sides(&self) -> Vec<Side> {
        ALL_SIDES
            .into_iter()
            .filter(|side| {
                let holds = |rule: &Rule| matches!(rule, Rule::Harness(sides) if sides.contains(side));
                let excluded = self.exclude.iter().any(holds);
                let included = self.include.iter().all(|r| !matches!(r, Rule::Harness(_)) || holds(r));
                included && !excluded
            })
            .collect()
    }

    pub fn admits(&self, facts: &Facts) -> bool {
        self.include.iter().all(|r| r.matches(facts))
            && !self.exclude.iter().any(|r| r.matches(facts))
            && self.filter.as_deref().is_none_or(|needle| facts.mentions(needle))
            && (!self.hide_untitled || !facts.title.trim().is_empty())
    }

    /// This scope for sessions of another host whose home is `home`: path
    /// rules resolve there.
    pub fn on_host(&self, home: &str) -> Scope {
        Scope {
            include: self.include.iter().map(|r| r.on_host(home)).collect(),
            exclude: self.exclude.iter().map(|r| r.on_host(home)).collect(),
            filter: self.filter.clone(),
            hide_untitled: self.hide_untitled,
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn facts(harness: Side, id: &str, path: &str, last_used_ms: i64, bytes: u64) -> Facts {
        Facts {
            harness,
            id: id.to_string(),
            title: format!("title {id}"),
            path: path.to_string(),
            project: project_name(path),
            created_ms: 0,
            last_used_ms,
            bytes,
            archive_bytes: None,
        }
    }

    fn scope(include: &[&str], exclude: &[&str]) -> Scope {
        Scope {
            include: include.iter().map(|r| r.parse().unwrap()).collect(),
            exclude: exclude.iter().map(|r| r.parse().unwrap()).collect(),
            filter: None,
            hide_untitled: false,
        }
    }

    #[test]
    fn path_rules_resolve_against_another_hosts_home() {
        let there = |path: &str| facts(Side::Pi, "a", path, 0, 0);
        let remote = scope(&["path=~/Projects/_myself"], &["path=Projects/_myself/old"]).on_host("/home/eve");
        assert!(remote.admits(&there("/home/eve/Projects/_myself/notes")));
        assert!(!remote.admits(&there("/home/eve/Projects/_myself/old/x")), "relative means under that home");
        assert!(!remote.admits(&there("/home/eve/Projects/_myselfish")), "whole components only");
        let absolute = scope(&["path=/srv/work"], &[]).on_host("/home/eve");
        assert!(absolute.admits(&there("/srv/work/api")));
    }

    #[test]
    fn hide_untitled_rejects_empty_titles() {
        let mut bare = facts(Side::Pi, "a", "/w", 0, 0);
        bare.title.clear();
        let named = facts(Side::Pi, "b", "/w", 0, 0);
        let scope = Scope { hide_untitled: true, ..Scope::default() };
        assert!(!scope.admits(&bare));
        assert!(scope.admits(&named));
        let spaced = {
            let mut f = facts(Side::Pi, "c", "/w", 0, 0);
            f.title = "   ".into();
            f
        };
        assert!(!scope.admits(&spaced));
    }

    #[test]
    fn harness_rules_take_aliases_and_narrow_what_is_read() {
        assert_eq!(scope(&["harness=claude-code,omp"], &[]).sides(), vec![Side::Pi, Side::Claude]);
        assert_eq!(scope(&[], &["harness=dsh"]).sides(), vec![Side::Pi, Side::Claude]);
        assert_eq!(scope(&["harness=pi,dsh"], &["harness=omp"]).sides(), vec![Side::Dsh]);
        assert!("harness=cursor".parse::<Rule>().is_err());
        assert!("color=red".parse::<Rule>().is_err());
        assert!("harness=".parse::<Rule>().is_err());
    }

    #[test]
    fn project_matches_the_folder_name_and_path_a_directory_and_below() {
        let a = facts(Side::Pi, "a", "/work/Veritas", 0, 0);
        let b = facts(Side::Pi, "b", "/work/veritas-old", 0, 0);
        let c = facts(Side::Pi, "c", "/other/veritas/sub", 0, 0);
        let only = |s: Scope| [&a, &b, &c].iter().filter(|f| s.admits(f)).map(|f| f.id.clone()).collect::<Vec<_>>();
        assert_eq!(only(scope(&["project=veritas"], &[])), vec!["a"]);
        assert_eq!(only(scope(&["path=/work/Veritas"], &[])), vec!["a"], "a component boundary, not a prefix");
        assert_eq!(only(scope(&["path=/other"], &[])), vec!["c"]);
        assert_eq!(only(scope(&[], &["path=/work"])), vec!["c"]);
        assert_eq!(only(scope(&["path=/work", "project=veritas-old"], &[])), vec!["b"], "every include must hold");
    }

    #[test]
    fn each_key_has_its_natural_direction_and_choosing_it_again_flips_it() {
        let old_small = facts(Side::Pi, "a", "/b/zeta", 1, 10);
        let new_big = facts(Side::Dsh, "b", "/a/alpha", 2, 20);
        let first = |sort: Sort| if sort.compare(&old_small, &new_big) == Ordering::Less { "a" } else { "b" };
        let by = |key| Sort { key, reverse: false };
        assert_eq!(first(by(SortKey::Used)), "a");
        assert_eq!(first(by(SortKey::Size)), "b");
        assert_eq!(first(by(SortKey::Path)), "b");
        assert_eq!(first(by(SortKey::Project)), "b");
        assert_eq!(first(by(SortKey::Harness)), "b");
        let flipped = by(SortKey::Size).toggled(SortKey::Size);
        assert!(flipped.reverse);
        assert_eq!(first(flipped), "a");
        assert!(!flipped.toggled(SortKey::Used).reverse, "a new key starts in its natural direction");
    }
}
