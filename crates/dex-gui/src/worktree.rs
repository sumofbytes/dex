//! Per-thread git worktrees: a thread can work on its own branch (`dex/<id>`) in a checkout
//! under the data directory, to be merged back into the project or thrown away.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq)]
pub struct Worktree {
    pub path: String,
    pub repo: String,
    pub branch: String,
}

fn root_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir)
        .join("dex/worktrees")
}

fn git(dir: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let e = String::from_utf8_lossy(&out.stderr);
        let o = String::from_utf8_lossy(&out.stdout);
        Err(format!(
            "{}{}",
            e.trim(),
            if e.trim().is_empty() { o.trim() } else { "" }
        ))
    }
}

/// The worktree a directory is, if it is one made here: read from its `.git` file, which
/// points back at the main repository.
pub fn info(cwd: &str) -> Option<Worktree> {
    let p = Path::new(cwd);
    if !p.starts_with(root_dir()) {
        return None;
    }
    let text = std::fs::read_to_string(p.join(".git")).ok()?;
    let gitdir = Path::new(text.strip_prefix("gitdir:")?.trim());
    // <repo>/.git/worktrees/<name>
    let repo = gitdir.parent()?.parent()?.parent()?;
    let id = p.file_name()?.to_string_lossy();
    Some(Worktree {
        path: cwd.to_string(),
        repo: repo.display().to_string(),
        branch: format!("dex/{id}"),
    })
}

/// Where threads of `cwd` are grouped in the sidebar: the main repository for a worktree.
pub fn project_root(cwd: &str) -> String {
    info(cwd).map_or_else(|| cwd.to_string(), |w| w.repo)
}

/// A new worktree of the repository at `repo`, on a fresh branch from its HEAD.
pub fn create(repo: &str) -> Result<String, String> {
    git(repo, &["rev-parse", "--show-toplevel"]).map_err(|_| "not a git repository".to_string())?;
    let name = Path::new(repo)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() ^ (d.as_secs() as u32));
    let id = format!("{nanos:08x}");
    let path = root_dir().join(&name).join(&id);
    std::fs::create_dir_all(path.parent().unwrap_or(&path)).map_err(|e| e.to_string())?;
    let p = path.display().to_string();
    git(
        repo,
        &["worktree", "add", "-b", &format!("dex/{id}"), &p, "HEAD"],
    )?;
    Ok(p)
}

/// Commit what the worktree changed and merge its branch into the main checkout's current
/// branch, then remove the worktree. A conflict aborts the merge and leaves the worktree.
pub fn merge(w: &Worktree) -> Result<String, String> {
    if !git(&w.path, &["status", "--porcelain"])?.is_empty() {
        git(&w.path, &["add", "-A"])?;
        git(
            &w.path,
            &["commit", "-m", &format!("dex: changes from {}", w.branch)],
        )?;
    }
    if let Err(e) = git(
        &w.repo,
        &[
            "merge",
            "--no-ff",
            "-m",
            &format!("Merge {}", w.branch),
            &w.branch,
        ],
    ) {
        let _ = git(&w.repo, &["merge", "--abort"]);
        return Err(format!("merge failed, worktree kept: {e}"));
    }
    remove(w)?;
    Ok(format!("Merged {} into the project", w.branch))
}

/// Delete the worktree and its branch, dropping any unmerged work.
pub fn discard(w: &Worktree) -> Result<String, String> {
    remove(w)?;
    Ok(format!("Discarded {}", w.branch))
}

fn remove(w: &Worktree) -> Result<(), String> {
    git(&w.repo, &["worktree", "remove", "--force", &w.path])?;
    git(&w.repo, &["branch", "-D", &w.branch])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_merge_and_discard() {
        let tmp = std::env::temp_dir().join(format!("dex-wt-test-{}", std::process::id()));
        let repo = tmp.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        let r = repo.display().to_string();
        for a in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
        ] {
            git(&r, a).unwrap();
        }
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&r, &["add", "."]).unwrap();
        git(&r, &["commit", "-qm", "init"]).unwrap();

        let wt = create(&r).unwrap();
        let w = info(&wt).expect("is a worktree");
        assert_eq!(
            Path::new(&w.repo).canonicalize().unwrap(),
            repo.canonicalize().unwrap()
        );
        assert_eq!(project_root(&wt), w.repo);
        std::fs::write(Path::new(&wt).join("b.txt"), "two\n").unwrap();
        merge(&w).unwrap();
        assert!(repo.join("b.txt").exists());
        assert!(!Path::new(&wt).exists());

        let wt2 = create(&r).unwrap();
        let w2 = info(&wt2).unwrap();
        std::fs::write(Path::new(&wt2).join("c.txt"), "x\n").unwrap();
        discard(&w2).unwrap();
        assert!(!Path::new(&wt2).exists());
        assert!(!repo.join("c.txt").exists());
        let _ = std::fs::remove_dir_all(tmp);
    }
}
