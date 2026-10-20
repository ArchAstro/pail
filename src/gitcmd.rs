//! Git's inspection commands over pail data: `show`, `cat-file`, `ls-tree`, `rev-parse`.
//! They take Git's arguments and print Git's output, so scripts written against Git work
//! against a pail store (bare or through a working copy).

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use anyhow::Result;

use crate::gitdiff::quote_path;
use crate::gitlog::{Fatal, LogOpts, Pathspec, Printer, Shown};
use crate::gitobj::{GitObjects, Object, TreeItem, display_mode};
use crate::rev::Ctx;

fn fatal(s: impl Into<String>) -> anyhow::Error {
    Fatal(s.into()).into()
}

async fn resolve(objs: &GitObjects<'_>, name: &str) -> Result<Object> {
    match objs.resolve(name).await {
        Ok(o) => Ok(o),
        Err(e) if e.to_string().starts_with("path '") => Err(fatal(e.to_string())),
        Err(_) => Err(fatal(format!("Not a valid object name {name}"))),
    }
}

/// `git cat-file (-t | -s | -e | -p | <type>) <object>` and `--batch[-check]`.
pub async fn cat_file(ctx: &Ctx, args: &[String], out: &mut dyn Write) -> Result<()> {
    let objs = GitObjects::new(ctx);
    let batch = args.iter().any(|a| a == "--batch");
    if batch || args.iter().any(|a| a == "--batch-check") {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let name = line?;
            let name = name.trim();
            match objs.resolve(name).await {
                Ok(o) => {
                    let data = objs.contents(&o).await?;
                    writeln!(out, "{} {} {}", objs.id(&o).await?, o.kind(), data.len())?;
                    if batch {
                        out.write_all(&data)?;
                        out.write_all(b"\n")?;
                    }
                }
                Err(_) => writeln!(out, "{name} missing")?,
            }
            out.flush()?;
        }
        return Ok(());
    }
    let (mode, name) = match args {
        [m, n] => (m.as_str(), n.as_str()),
        _ => return Err(fatal("usage: pail cat-file (-t | -s | -e | -p | <type>) <object>")),
    };
    if mode == "-e" {
        return match objs.resolve(name).await {
            Ok(_) => Ok(()),
            Err(_) => std::process::exit(1),
        };
    }
    let o = resolve(&objs, name).await?;
    match mode {
        "-t" => writeln!(out, "{}", o.kind())?,
        "-s" => writeln!(out, "{}", objs.contents(&o).await?.len())?,
        "-p" => match &o {
            Object::Tree { commit, path } => {
                for it in objs.tree_items(*commit, path, false).await? {
                    writeln!(out, "{} {} {}\t{}", display_mode(it.mode), it.kind, it.oid, quote_path(&it.name))?;
                }
            }
            _ => out.write_all(&objs.contents(&o).await?)?,
        },
        t if t == o.kind() => out.write_all(&objs.contents(&o).await?)?,
        t if ["blob", "tree", "commit"].contains(&t) => return Err(fatal(format!("git cat-file {name}: bad file"))),
        _ => return Err(fatal(format!("invalid object type \"{mode}\""))),
    }
    Ok(())
}

/// `git ls-tree [-d] [-r] [-t] [-l] [-z] [--name-only] [--object-only] [--abbrev[=n]]
/// <tree-ish> [<path>...]`, with Git's pathspec walk (show_recursive).
pub async fn ls_tree(ctx: &Ctx, args: &[String], out: &mut dyn Write) -> Result<()> {
    let (mut recursive, mut trees_only, mut show_trees, mut long, mut z, mut name_only, mut object_only) = (false, false, false, false, false, false, false);
    let mut abbrev: Option<usize> = None;
    let mut rest = vec![];
    for a in args {
        match a.as_str() {
            "-r" => recursive = true,
            "-d" => trees_only = true,
            "-t" => show_trees = true,
            "-l" | "--long" => long = true,
            "-z" => z = true,
            "--name-only" | "--name-status" => name_only = true,
            "--object-only" => object_only = true,
            "--full-name" | "--full-tree" => {}
            "--abbrev" => abbrev = Some(7),
            _ if a.starts_with("--abbrev=") => abbrev = a[9..].parse().ok(),
            _ if a.starts_with('-') && a.len() > 1 => return Err(fatal(format!("unknown option `{}'", a.trim_start_matches('-')))),
            _ => rest.push(a.clone()),
        }
    }
    if trees_only && recursive {
        show_trees = true;
    }
    let Some((treeish, paths)) = rest.split_first() else { return Err(fatal("usage: pail ls-tree [<options>] <tree-ish> [<path>...]")) };
    let objs = GitObjects::new(ctx);
    let (commit, base) = match objs.resolve(treeish).await {
        Ok(Object::Commit { hash, .. }) => (hash, String::new()),
        Ok(Object::Tree { commit, path }) => (commit, path),
        _ => return Err(fatal(format!("Not a valid object name {treeish}"))),
    };
    // Everything under the tree, then Git's traversal over it.
    let all = objs.tree_items(commit, &base, true).await?;
    let rel = |p: &str| if base.is_empty() { p.to_string() } else { p[base.len() + 1..].to_string() };
    let mut children: BTreeMap<String, Vec<TreeItem>> = BTreeMap::new();
    for it in all {
        let r = rel(&it.path);
        let parent = r.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
        children.entry(parent).or_default().push(TreeItem { path: r, ..it });
    }
    let specs: Vec<String> = paths.iter().map(|p| p.trim_start_matches("./").to_string()).collect();
    let interesting = |path: &str, is_tree: bool| -> bool {
        specs.is_empty()
            || specs.iter().any(|s| {
                let s = s.trim_end_matches('/');
                s.is_empty() || path == s || path.starts_with(&format!("{s}/")) || (is_tree && s.starts_with(&format!("{path}/")))
            })
    };
    let show_recursive = |path: &str| -> bool { recursive || specs.iter().any(|s| s.len() > path.len() && s.starts_with(&format!("{path}/"))) };
    let term = if z { '\0' } else { '\n' };
    let name = |p: &str| if z { p.to_string() } else { quote_path(p) };
    let oid = |o: &str| match abbrev {
        Some(n) => o[..n.clamp(4, 40)].to_string(),
        None => o.to_string(),
    };
    let mut lines = vec![];
    // Depth-first in tree order.
    fn visit(
        dir: &str,
        children: &BTreeMap<String, Vec<TreeItem>>,
        f: &mut dyn FnMut(&TreeItem) -> bool,
    ) {
        if let Some(items) = children.get(dir) {
            for it in items {
                if f(it) {
                    visit(&it.path, children, f);
                }
            }
        }
    }
    let mut emit = |it: &TreeItem| -> bool {
        let is_tree = it.kind == "tree";
        if !interesting(&it.path, is_tree) {
            return false;
        }
        let mut recurse = false;
        if is_tree && show_recursive(&it.path) {
            recurse = true;
            if !show_trees {
                return true;
            }
        }
        if !is_tree && trees_only {
            return false;
        }
        lines.push(it.clone());
        recurse
    };
    visit("", &children, &mut emit);
    for it in lines {
        if object_only {
            write!(out, "{}{term}", oid(&it.oid))?;
        } else if name_only {
            write!(out, "{}{term}", name(&it.path))?;
        } else if long {
            let size = it.size.map(|s| s.to_string()).unwrap_or_else(|| "-".into());
            write!(out, "{} {} {} {size:>7}\t{}{term}", display_mode(it.mode), it.kind, oid(&it.oid), name(&it.path))?;
        } else {
            write!(out, "{} {} {}\t{}{term}", display_mode(it.mode), it.kind, oid(&it.oid), name(&it.path))?;
        }
    }
    Ok(())
}

/// `git rev-parse [--short[=n]] [--verify] [--abbrev-ref] [--show-toplevel]
/// [--is-bare-repository] <name>...`
pub async fn rev_parse(ctx: &Ctx, args: &[String], out: &mut dyn Write) -> Result<()> {
    let objs = GitObjects::new(ctx);
    let mut short: Option<usize> = None;
    let mut abbrev_ref = false;
    // --short and --verify want exactly one revision.
    let verify = args.iter().any(|a| a == "--verify" || a.starts_with("--short"));
    // Ids are printed once every name resolved.
    let mut lines: Vec<u8> = vec![];
    if verify && args.iter().filter(|a| !a.starts_with('-')).count() != 1 {
        return Err(fatal("Needed a single revision"));
    }
    for a in args {
        match a.as_str() {
            "--short" => short = Some(ctx.default_abbrev().await?),
            _ if a.starts_with("--short=") => short = a[8..].parse().ok(),
            "--abbrev-ref" => abbrev_ref = true,
            "--verify" | "-q" | "--quiet" => {}
            "--show-toplevel" => match &ctx.worktree {
                Some(r) => writeln!(out, "{}", r.display())?,
                None => return Err(fatal("this operation must be run in a work tree")),
            },
            "--is-bare-repository" => writeln!(out, "{}", ctx.worktree.is_none())?,
            "--is-inside-work-tree" => writeln!(out, "{}", ctx.worktree.is_some())?,
            _ if a.starts_with('-') => return Err(fatal(format!("unknown option {a}"))),
            name if abbrev_ref => {
                if name == "HEAD" || name == "@" {
                    writeln!(out, "{}", ctx.branch().await?)?;
                } else {
                    writeln!(out, "{name}")?;
                }
            }
            name => {
                let o = match objs.resolve(name).await {
                    Ok(o) => o,
                    Err(e) if e.to_string().starts_with("path '") => {
                        out.write_all(&lines)?;
                        writeln!(out, "{name}")?;
                        return Err(fatal(e.to_string()));
                    }
                    Err(_) => {
                        // Git echoes the argument before rejecting it.
                        out.write_all(&lines)?;
                        writeln!(out, "{name}")?;
                        return Err(fatal(format!(
                            "ambiguous argument '{name}': unknown revision or path not in the working tree.\nUse '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'"
                        )));
                    }
                };
                let id = objs.id(&o).await?;
                match short {
                    Some(n) => writeln!(lines, "{}", &id[..n.clamp(4, id.len())])?,
                    None => writeln!(lines, "{id}")?,
                }
            }
        }
    }
    out.write_all(&lines)?;
    Ok(())
}

/// `git show [<log options>] [<object>...]`: commits with their patch, trees as listings,
/// blobs as their bytes.
pub async fn show(ctx: &Ctx, args: &[String], out: &mut dyn Write, tty: bool) -> Result<()> {
    let mut o = LogOpts::parse_with(args, &ctx.config)?;
    if !o.diff.any_output() && !o.diff.no_patch {
        o.diff.patch = true;
    }
    let names = if o.revs.is_empty() { vec!["HEAD".to_string()] } else { std::mem::take(&mut o.revs) };
    let objs = GitObjects::new(ctx);
    let spec = Pathspec::new(&o.paths)?;
    let mut printer = Printer::new(ctx, &o, tty, true).await?;
    for name in &names {
        match resolve(&objs, name).await? {
            Object::Commit { hash, commit } => {
                // Merges show Git's combined diff, which pail's first-parent history lacks.
                let merge = commit.meta.get("git.parents").is_some_and(|p| p.split_whitespace().count() > 1);
                let all = match ctx.repo.commit_obj(&hash).await?.change_list() {
                    Some(cs) => cs,
                    None => {
                        let from = ctx.repo.root_of(commit.parent).await?;
                        ctx.repo.diff(from, commit.root).await?
                    }
                };
                // With paths, commits that don't touch them are not shown (as in log).
                if !spec.is_empty() && !all.iter().any(|c| spec.matches(&c.path)) {
                    continue;
                }
                let changes = if merge { Some(vec![]) } else { Some(all.into_iter().filter(|c| spec.matches(&c.path)).collect()) };
                printer.show(&Shown { hash, commit, changes }, out).await?;
            }
            Object::Tree { commit, path } => {
                if printer.shown_one {
                    out.write_all(b"\n")?;
                }
                writeln!(out, "tree {name}\n")?;
                for it in objs.tree_items(commit, &path, false).await? {
                    writeln!(out, "{}{}", it.name, if it.kind == "tree" { "/" } else { "" })?;
                }
                printer.shown_one = true;
            }
            o @ Object::Blob { .. } => out.write_all(&objs.contents(&o).await?)?,
        }
    }
    Ok(())
}
