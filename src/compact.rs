//! In-place compaction: a session becomes one tarball in its own bucket,
//! beside where it lived, and unpacks back to exactly the same paths.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::json;

use crate::archive::{self, CompressArgs, Manifest};
use crate::paths::Roots;
use crate::units::{self, Act, Age, Candidate, Choice, Ctx, Report, Scope, Status, Unit, Verb};
use crate::{human_size, Side};

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List sessions packed in place, with what their archives record.
    List(ListArgs),
    /// Pack sessions unused for a while into tarballs beside where they live.
    Pack(PackArgs),
    /// Unpack sessions packed in place back where they were.
    Unpack(UnpackArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub scope: Scope,
    /// Print every recorded detail of each session.
    #[arg(short, long)]
    pub long: bool,
    /// Print JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PackArgs {
    #[command(flatten)]
    pub act: Act,
    #[command(flatten)]
    pub scope: Scope,
    /// Offer sessions unused for at least this long: 30d, 2w, 36h, or days.
    #[arg(long, default_value = "30d")]
    pub older_than: Age,
    #[command(flatten)]
    pub compress: CompressArgs,
}

#[derive(Debug, Args)]
pub struct UnpackArgs {
    #[command(flatten)]
    pub act: Act,
    #[command(flatten)]
    pub scope: Scope,
    /// Keep the archive after unpacking.
    #[arg(long)]
    pub keep: bool,
}

pub fn run(ctx: &Ctx, command: Command) -> Result<i32> {
    match command {
        Command::List(args) => list(ctx, &args).map(|()| 0),
        Command::Pack(args) => pack(ctx, &args),
        Command::Unpack(args) => unpack(ctx, &args),
    }
}

/// A session packed in place.
#[derive(Debug, Clone)]
pub struct Local {
    pub path: PathBuf,
    pub manifest: Manifest,
    pub archive_bytes: u64,
}

/// Every archive packed in place under the roots of `sides`, oldest use first.
pub fn discover(roots: &Roots, sides: &[Side]) -> Vec<Local> {
    let mut out = Vec::new();
    for &side in sides {
        let Ok(buckets) = fs::read_dir(roots.of(side)) else { continue };
        for bucket in buckets.flatten().filter(|b| b.file_type().is_ok_and(|t| t.is_dir())) {
            out.extend(bucket_archives(side, &bucket.path()));
        }
    }
    out.sort_by_key(|l| l.manifest.last_used_ms());
    out
}

/// Archives packed in place in one bucket of `side`'s root.
fn bucket_archives(side: Side, bucket: &Path) -> Vec<Local> {
    let Ok(files) = fs::read_dir(bucket) else { return Vec::new() };
    let bucket_name = bucket.file_name().and_then(|n| n.to_str());
    let mut out = Vec::new();
    for file in files.flatten() {
        let is_archive = file.file_name().to_str().is_some_and(archive::is_archive_name);
        if !is_archive || !file.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let path = file.path();
        match archive::read_manifest(&path) {
            Ok(manifest) if manifest.harness == side && Some(manifest.bucket.as_str()) == bucket_name => {
                let archive_bytes = file.metadata().map_or(0, |m| m.len());
                out.push(Local { path, manifest, archive_bytes });
            }
            Ok(_) => eprintln!("note: {} belongs to another harness or bucket; skipped", path.display()),
            Err(err) => eprintln!("note: {} is unreadable: {err:#}", path.display()),
        }
    }
    out
}

/// Picker row for a session packed in place; blocked while it exists live.
pub fn local_row(ctx: &Ctx, local: &Local) -> units::Candidate {
    let mut row = units::archive_row(&local.manifest, local.archive_bytes, &local.path, ctx.now_ms, Vec::new());
    let taken = local.manifest.occupied(ctx.roots.of(local.manifest.harness));
    if !taken.is_empty() {
        row.blocked = Some(format!("a live copy exists: {}", taken.join(", ")));
    }
    Candidate { id: local.manifest.id.clone(), path: Some(local.path.clone()), row }
}

fn list(ctx: &Ctx, args: &ListArgs) -> Result<()> {
    let locals: Vec<Local> = discover(&ctx.roots, &args.scope.sides())
        .into_iter()
        .filter(|l| args.scope.admits(&format!("{} {} {}", l.manifest.id, l.manifest.title, l.manifest.cwd)))
        .collect();
    if args.json {
        let rows: Vec<_> = locals
            .iter()
            .map(|l| json!({"archive": l.path, "archiveBytes": l.archive_bytes, "manifest": l.manifest}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if !args.long {
        units::print_header();
    }
    for local in &locals {
        let m = &local.manifest;
        let days = units::age_days(ctx.now_ms, m.last_used_ms());
        if args.long {
            println!("── {} ──", if m.title.is_empty() { &m.id } else { &m.title });
            units::print_details(&units::manifest_details(m, local.archive_bytes, &local.path, ctx.now_ms));
            println!();
        } else {
            let size = human_size(m.bytes);
            units::print_line(days, &size, &human_size(local.archive_bytes), m.harness, &m.id, &m.cwd, &m.title);
        }
    }
    let original: u64 = locals.iter().map(|l| l.manifest.bytes).sum();
    let packed: u64 = locals.iter().map(|l| l.archive_bytes).sum();
    println!(
        "{} session(s) packed in place · {} holding {} of sessions",
        locals.len(),
        human_size(packed),
        human_size(original)
    );
    Ok(())
}

/// Where a session packs to in place: its own bucket.
fn destination(root: &Path, unit: &Unit, extension: &str) -> PathBuf {
    root.join(&unit.bucket).join(format!("{}{extension}", unit.name))
}

fn pack(ctx: &Ctx, args: &PackArgs) -> Result<i32> {
    let compression = args.compress.resolve()?;
    let found = units::discover(&ctx.roots, &args.scope.sides());
    let candidates = found
        .iter()
        .map(|u| Candidate { id: u.id.clone(), path: None, row: units::unit_row(u, ctx.roots.of(u.harness), ctx.now_ms) })
        .collect();
    let choice = Choice {
        title: format!("compact pack · in place · {compression}"),
        verb: "pack",
        threshold: Some(args.older_than),
    };
    let chosen: Vec<&Unit> =
        units::choose(candidates, &args.act, &args.scope, choice)?.into_iter().map(|i| &found[i]).collect();
    if chosen.is_empty() {
        println!("nothing chosen");
        return Ok(0);
    }
    let held = units::held_pi(ctx, &chosen);
    let mut report = Report::new(Verb::Pack, args.act.dry_run);
    for unit in chosen {
        let root = ctx.roots.of(unit.harness);
        let dest = destination(root, unit, compression.algo.extension());
        let mut sizes = human_size(unit.bytes);
        let status = if held.contains(&unit.id) {
            Status::Held
        } else if dest.symlink_metadata().is_ok() {
            Status::Conflict(format!("{} exists; unpack or remove it first", dest.display()))
        } else if args.act.dry_run {
            Status::Would
        } else {
            match archive::pack(root, unit, &dest, compression, &ctx.system, ctx.now_ms) {
                Ok(packed) => {
                    sizes = format!("{} → {}", human_size(unit.bytes), human_size(packed.archive_bytes));
                    match units::remove_members(root, &unit.members) {
                        Ok(()) => {
                            report.freed += unit.bytes.saturating_sub(packed.archive_bytes);
                            Status::Done
                        }
                        Err(err) => Status::Failed(format!("packed to {}, but {err:#}", dest.display())),
                    }
                }
                Err(err) => Status::from_error(err),
            }
        };
        report.line(&status, unit.harness, &unit.id, &unit.title, &sizes);
    }
    Ok(report.finish())
}

fn unpack(ctx: &Ctx, args: &UnpackArgs) -> Result<i32> {
    let locals = discover(&ctx.roots, &args.scope.sides());
    let candidates = locals.iter().map(|l| local_row(ctx, l)).collect();
    let choice = Choice { title: "compact unpack · back into place".to_string(), verb: "unpack", threshold: None };
    let chosen: Vec<&Local> =
        units::choose(candidates, &args.act, &args.scope, choice)?.into_iter().map(|i| &locals[i]).collect();
    if chosen.is_empty() {
        println!("nothing chosen");
        return Ok(0);
    }
    let mut report = Report::new(Verb::Unpack, args.act.dry_run);
    for local in chosen {
        let m = &local.manifest;
        let root = ctx.roots.of(m.harness);
        let sizes = format!("{} → {}", human_size(local.archive_bytes), human_size(m.bytes));
        let taken = m.occupied(root);
        let status = if !taken.is_empty() {
            Status::Conflict(format!("{} already exists", taken.join(", ")))
        } else if args.act.dry_run {
            Status::Would
        } else {
            match archive::extract(&local.path, root) {
                Ok(_) if args.keep => Status::Done,
                Ok(_) => match fs::remove_file(&local.path) {
                    Ok(()) => Status::Done,
                    Err(err) => Status::Failed(format!("unpacked, but could not remove {}: {err}", local.path.display())),
                },
                Err(err) => Status::from_error(err),
            }
        };
        if status.ok() {
            report.restored += m.bytes;
        }
        report.line(&status, m.harness, &m.id, &m.title, &sizes);
    }
    Ok(report.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Algo;
    use crate::units::tests::{tree, Scratch};

    fn act(sessions: &[&str]) -> Act {
        Act { sessions: sessions.iter().map(|s| s.to_string()).collect(), yes: true, dry_run: false }
    }

    fn scope() -> Scope {
        Scope { harness: vec![Side::Pi], filter: None }
    }

    fn pack_args(algo: Algo, sessions: &[&str]) -> PackArgs {
        PackArgs {
            act: act(sessions),
            scope: scope(),
            older_than: "0d".parse().unwrap(),
            compress: CompressArgs { algo, level: None, threads: Some(0) },
        }
    }

    #[test]
    fn pack_leaves_one_tarball_in_the_bucket_and_unpack_restores_the_session() {
        let scratch = Scratch::new("compact");
        let ctx = scratch.ctx();
        scratch.pi_session("one", "/work/a");
        scratch.pi_session("two", "/work/a");
        let before = tree(&scratch.roots.pi);

        assert_eq!(pack(&ctx, &pack_args(Algo::Zstd, &["one"])).unwrap(), 0);
        let locals = discover(&ctx.roots, &[Side::Pi]);
        assert_eq!(locals.len(), 1);
        let local = &locals[0];
        assert_eq!(local.manifest.id, "one");
        assert!(local.path.to_string_lossy().ends_with("_one.tar.zst"));
        assert!(local.manifest.members.iter().all(|m| !scratch.roots.pi.join(m).exists()));
        assert_eq!(units::discover(&ctx.roots, &[Side::Pi]).len(), 1, "only the other session stays live");

        let args = UnpackArgs { act: act(&["one"]), scope: scope(), keep: false };
        assert_eq!(unpack(&ctx, &args).unwrap(), 0);
        assert_eq!(tree(&scratch.roots.pi), before);
    }

    #[test]
    fn the_age_threshold_keeps_recent_sessions_live() {
        let scratch = Scratch::new("compact-age");
        let mut ctx = scratch.ctx();
        scratch.pi_session("recent", "/work/a");
        ctx.now_ms = chrono::Utc::now().timestamp_millis();
        let mut args = pack_args(Algo::Gzip, &[]);
        args.older_than = "30d".parse().unwrap();
        assert_eq!(pack(&ctx, &args).unwrap(), 0);
        assert!(discover(&ctx.roots, &[Side::Pi]).is_empty());
        assert_eq!(units::discover(&ctx.roots, &[Side::Pi]).len(), 1);
    }

    #[test]
    fn unpacking_onto_a_live_session_is_skipped_and_keeps_the_archive() {
        let scratch = Scratch::new("compact-conflict");
        let ctx = scratch.ctx();
        scratch.pi_session("one", "/work/a");
        pack(&ctx, &pack_args(Algo::Gzip, &["one"])).unwrap();
        scratch.pi_session("one", "/work/a");
        let args = UnpackArgs { act: act(&["one"]), scope: scope(), keep: false };
        assert_eq!(unpack(&ctx, &args).unwrap(), 1);
        assert_eq!(discover(&ctx.roots, &[Side::Pi]).len(), 1);
    }
}
