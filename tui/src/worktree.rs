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
