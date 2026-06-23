// Git worktree management — each session gets its own worktree off latest master.

use std::process::{Command, Stdio};

/// Create a git worktree at `worktree_path` branching off `origin/master`.
///
/// Steps:
///   1. cd into repo_path
///   2. git fetch origin
///   3. git pull origin master (update main worktree)
///   4. git worktree add <worktree_path> -b worktree-<name> origin/master
///
/// All git output is captured (not inherited) so it doesn't corrupt the TUI.
pub fn create_worktree(repo_path: &str, worktree_path: &str, name: &str) -> std::io::Result<()> {
    let branch_name = format!("worktree-{name}");

    // Fetch latest from origin.
    let fetch_output = Command::new("git")
        .arg("fetch")
        .arg("origin")
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !fetch_output.status.success() {
        let err = String::from_utf8_lossy(&fetch_output.stderr);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("git fetch origin failed: {err}"),
        ));
    }

    // Update master in the main worktree.
    let pull_output = Command::new("git")
        .arg("pull")
        .arg("origin")
        .arg("master")
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !pull_output.status.success() {
        let err = String::from_utf8_lossy(&pull_output.stderr);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("git pull origin master failed: {err}"),
        ));
    }

    // Create the worktree.
    let worktree_output = Command::new("git")
        .arg("worktree")
        .arg("add")
        .arg(worktree_path)
        .arg("-b")
        .arg(&branch_name)
        .arg("origin/master")
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !worktree_output.status.success() {
        let err = String::from_utf8_lossy(&worktree_output.stderr);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("git worktree add failed for branch {branch_name}: {err}"),
        ));
    }

    // Write an AGENTS.md file into the worktree. Pi discovers AGENTS.md
    // in the working directory and loads it as context. This overrides
    // the prototype repo's CLAUDE.md "Team Skills" section, which tells
    // agents to write skills to agents/skills/ (Skillshare). We want
    // orchestra sessions to write skills to ~/.orchestra/skills/ instead.
    let agents_md = format!("{worktree_path}/AGENTS.md");
    std::fs::write(&agents_md, "\
# Orchestra Session

You are running as an orchestra session in a worktree of the prototype repo.

## Skills — IMPORTANT

When asked to \"write a skill\", write it to:

    ~/.orchestra/skills/<name>.md

This is the ONLY correct location for orchestra skills. After writing,
commit and push:

    cd ~/orchestra && git add skills/ && git commit -m \"skill: <name>\" && git push origin main

Do NOT write skills to `agents/skills/` in this repo. That directory
contains team Skillshare skills — a completely different system. The
CLAUDE.md in this repo describes those as \"Team Skills\" but that does
NOT apply to orchestra skills.

When someone asks \"where would you write a skill\", the answer is
`~/.orchestra/skills/<name>.md`. Not `agents/skills/`, not Skillshare,
not `SKILL.md` files. Always `~/.orchestra/skills/<name>.md`.
")?;

    Ok(())
}

/// Remove a worktree and delete its branch.
pub fn remove_worktree(repo_path: &str, worktree_path: &str, name: &str) -> std::io::Result<()> {
    let branch_name = format!("worktree-{name}");

    // Remove the worktree.
    let _ = Command::new("git")
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(worktree_path)
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .status();

    // Delete the branch.
    let _ = Command::new("git")
        .arg("branch")
        .arg("-D")
        .arg(&branch_name)
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .status();

    Ok(())
}
