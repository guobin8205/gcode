use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

static TEMP_INDEX_SEQ: AtomicU64 = AtomicU64::new(1);

/// 检测工作区是否位于有效的 Git 仓库工作树内。
pub fn is_git_repo(workspace_root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

/// 使用独立临时索引拍摄当前工作区的轻量 Git Tree 快照（排除 `.gcode/`），
/// 完全不修改用户的 `.git/index` 或提交历史。非 Git 仓库返回 `Ok(None)`。
pub fn capture_tree_snapshot(workspace_root: &Path) -> Result<Option<String>> {
    if !is_git_repo(workspace_root) {
        return Ok(None);
    }

    let temp_index = alloc_temp_index_path();
    let _guard = TempFileGuard(temp_index.clone());

    // 若存在 HEAD，先载入基线树（空仓库无 HEAD 时忽略）
    let has_head = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_head {
        let out = Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .env("GIT_INDEX_FILE", &temp_index)
            .args(["read-tree", "HEAD"])
            .output()
            .context("执行 git read-tree HEAD 失败")?;
        if !out.status.success() {
            bail!(
                "git read-tree HEAD 失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }

    // 将工作区当前状态（含未跟踪文件，排除 .gcode/）写入临时索引；关闭 autocrlf 保证字节级保真
    let add_out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .env("GIT_INDEX_FILE", &temp_index)
        .args(["-c", "core.autocrlf=false", "add", "-A", "--", ".", ":!.gcode"])
        .output()
        .context("执行 git add 拍摄快照失败")?;
    if !add_out.status.success() {
        bail!(
            "git add 快照失败: {}",
            String::from_utf8_lossy(&add_out.stderr).trim()
        );
    }

    // 产出 Tree Hash
    let tree_out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .env("GIT_INDEX_FILE", &temp_index)
        .args(["write-tree"])
        .output()
        .context("执行 git write-tree 失败")?;
    if !tree_out.status.success() {
        bail!(
            "git write-tree 失败: {}",
            String::from_utf8_lossy(&tree_out.stderr).trim()
        );
    }

    let hash = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();
    if hash.is_empty() {
        Ok(None)
    } else {
        Ok(Some(hash))
    }
}

/// 将工作区磁盘文件精确还原至指定 `tree_hash` 快照（同时清理快照之后新增的文件，但保留 `.gcode/` 与用户真实暂存区）。
pub fn restore_tree_snapshot(workspace_root: &Path, tree_hash: &str) -> Result<()> {
    if !is_git_repo(workspace_root) {
        bail!("当前工作区不是 Git 仓库，无法还原磁盘快照");
    }

    // 1. 列出目标快照包含的所有文件路径
    let ls_tree = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["ls-tree", "-r", "--name-only", "-z", tree_hash])
        .output()
        .context("执行 git ls-tree 失败")?;
    if !ls_tree.status.success() {
        bail!(
            "无效的快照哈希 {tree_hash}: {}",
            String::from_utf8_lossy(&ls_tree.stderr).trim()
        );
    }
    let snapshot_files: HashSet<String> = ls_tree
        .stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();

    // 2. 列出当前工作区中所有已跟踪 + 未被忽略的未跟踪文件，删除不在快照内且非 .gcode/ 的文件
    let cur_files = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .context("执行 git ls-files 失败")?;
    if cur_files.status.success() {
        for raw in cur_files.stdout.split(|&b| b == 0).filter(|s| !s.is_empty()) {
            let rel = String::from_utf8_lossy(raw);
            if rel == ".gcode" || rel.starts_with(".gcode/") {
                continue;
            }
            if !snapshot_files.contains(rel.as_ref()) {
                let target = workspace_root.join(rel.as_ref());
                if target.is_file() {
                    let _ = fs::remove_file(&target);
                }
            }
        }
    }

    // 3. 使用独立临时索引还原快照内的全部文件，不修改用户真实 .git/index
    let temp_index = alloc_temp_index_path();
    let _guard = TempFileGuard(temp_index.clone());

    let read_out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .env("GIT_INDEX_FILE", &temp_index)
        .args(["read-tree", tree_hash])
        .output()
        .context("执行 git read-tree 还原快照失败")?;
    if !read_out.status.success() {
        bail!(
            "git read-tree 还原失败: {}",
            String::from_utf8_lossy(&read_out.stderr).trim()
        );
    }

    let checkout_out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .env("GIT_INDEX_FILE", &temp_index)
        .args(["-c", "core.autocrlf=false", "checkout-index", "-a", "-f"])
        .output()
        .context("执行 git checkout-index 还原工作区失败")?;
    if !checkout_out.status.success() {
        bail!(
            "git checkout-index 还原失败: {}",
            String::from_utf8_lossy(&checkout_out.stderr).trim()
        );
    }

    Ok(())
}

fn alloc_temp_index_path() -> PathBuf {
    let seq = TEMP_INDEX_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("gcode_git_idx_{}_{seq}", std::process::id()))
}

struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_and_restore_snapshot_preserves_gcode_and_cleans_new_files() {
        let dir = std::env::temp_dir().join(format!("gcode_snap_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let init = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init"])
            .output()
            .unwrap();
        assert!(init.status.success());

        fs::write(dir.join("main.txt"), "v1\n").unwrap();
        fs::create_dir_all(dir.join(".gcode")).unwrap();
        fs::write(dir.join(".gcode").join("keep.log"), "session-log\n").unwrap();

        let tree = capture_tree_snapshot(&dir).unwrap().expect("应生成 tree hash");
        assert_eq!(tree.len(), 40);

        // 模拟 Turn 中修改旧文件、新增新文件，并更新 .gcode 日志
        fs::write(dir.join("main.txt"), "v2-broken\n").unwrap();
        fs::write(dir.join("extra.txt"), "should-be-removed\n").unwrap();
        fs::write(dir.join(".gcode").join("keep.log"), "session-log-updated\n").unwrap();

        restore_tree_snapshot(&dir, &tree).unwrap();

        assert_eq!(fs::read_to_string(dir.join("main.txt")).unwrap(), "v1\n");
        assert!(!dir.join("extra.txt").exists());
        assert_eq!(
            fs::read_to_string(dir.join(".gcode").join("keep.log")).unwrap(),
            "session-log-updated\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
