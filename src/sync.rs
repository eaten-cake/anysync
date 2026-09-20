use crate::config::{self, Config};
use crate::webdav::{Entry, WebDav};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use walkdir::WalkDir;

/// 比较修改时间时的容差（秒），用于吸收不同系统间的时钟与精度差异。
const TOLERANCE: i64 = 2;
const VERIFY_RETRIES: usize = 3;
const PROGRESS_WIDTH: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// 需要把 src 中的文件复制到 dst
    Transfer,
    /// dst 较新，跳过
    Skip,
    /// 修改时间相同但大小不同，视为冲突，跳过
    Conflict,
}

fn scan_local(root: &Path) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let walk = WalkDir::new(root).into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        name != ".anysync" && name != ".git"
    });
    for item in walk {
        let item = item.with_context(|| format!("读取目录失败：{}", root.display()))?;
        if !item.file_type().is_file() {
            continue; // 目录随文件按需创建，符号链接不跟随
        }
        let meta = item.metadata()?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        entries.push(Entry {
            path: item
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
            mtime,
            size: meta.len(),
            is_dir: false,
        });
    }
    Ok(entries)
}

/// 对比 src 与 dst 的文件列表，生成以 src 为源的执行计划。
pub fn plan<'a>(src: &'a [Entry], dst: &'a [Entry]) -> Vec<(&'a Entry, Action)> {
    let dst: BTreeMap<&str, &Entry> = dst.iter().map(|e| (e.path.as_str(), e)).collect();
    src.iter()
        .filter(|e| !e.is_dir)
        .map(|s| {
            let action = match dst.get(s.path.as_str()) {
                None => Action::Transfer,
                Some(d) if s.mtime > d.mtime + TOLERANCE => Action::Transfer,
                Some(d) if d.mtime > s.mtime + TOLERANCE => Action::Skip,
                Some(d) if d.size != s.size => Action::Conflict,
                _ => Action::Skip,
            };
            (s, action)
        })
        .collect()
}

pub fn pull() -> Result<()> {
    let (root, dav) = connect_repo()?;
    let local = scan_local(&root)?;
    let remote = dav.list_recursive()?;
    let mut updated = 0;
    for (e, action) in plan(&remote, &local) {
        match action {
            Action::Transfer => {
                download(&dav, &root, e)?;
                println!("pull      {}", e.path);
                updated += 1;
            }
            Action::Conflict => {
                println!("conflict  {}（修改时间相同但大小不同，已跳过）", e.path)
            }
            Action::Skip => {}
        }
    }
    println!("pull 完成：更新 {updated} 个文件");
    Ok(())
}

pub fn push() -> Result<()> {
    let (root, dav) = connect_repo()?;
    let local = scan_local(&root)?;
    let remote = dav.list_recursive()?;
    let todo: Vec<_> = plan(&local, &remote)
        .into_iter()
        .filter(|(_, a)| *a == Action::Transfer)
        .collect();
    if todo.is_empty() {
        println!("push 完成：远端已是最新");
        return Ok(());
    }

    // 先递归创建配置的远端根目录，再创建文件所在目录。
    dav.ensure_root()?;
    let mut dirs: Vec<&str> = todo
        .iter()
        .flat_map(|(e, _)| parent_dirs(&e.path))
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    for dir in dirs {
        dav.mkcol(dir)?;
    }

    let display = Arc::new(Mutex::new(ProgressDisplay::new()));
    for (index, (entry, _)) in todo.iter().enumerate() {
        upload_file(&dav, &root, entry, index + 1, todo.len(), &display)?;
    }

    // 部分服务器写入后不会立即出现在目录列表中，有限次数重新上传并复核。
    let mut missing_paths = Vec::new();
    for verify_round in 0..=VERIFY_RETRIES {
        let remote_after = dav.list_recursive()?;
        let stored: BTreeMap<&str, u64> = remote_after
            .iter()
            .map(|e| (e.path.as_str(), e.size))
            .collect();
        let mut missing = Vec::new();
        for (index, item) in todo.iter().enumerate() {
            let entry = item.0;
            if stored.get(entry.path.as_str()) != Some(&entry.size) {
                missing.push((index, entry));
            }
        }
        missing_paths = missing.iter().map(|(_, e)| e.path.clone()).collect();
        if missing.is_empty() {
            break;
        }
        if verify_round == VERIFY_RETRIES {
            break;
        }

        println!(
            "远端暂未确认 {} 个文件，{} 秒后重试上传（第 {}/{} 次）",
            missing.len(),
            1_u64 << verify_round,
            verify_round + 1,
            VERIFY_RETRIES
        );
        thread::sleep(Duration::from_millis(500 * (1_u64 << verify_round)));
        for (index, entry) in missing {
            upload_file(&dav, &root, entry, index + 1, todo.len(), &display)?;
        }
    }

    if missing_paths.is_empty() {
        println!("push 完成：更新 {} 个文件", todo.len());
    } else {
        for path in &missing_paths {
            eprintln!(
                "warning: 自动重试后服务器仍未保留 {path}（可能被服务端规则丢弃，后续 push 会继续补传）"
            );
        }
        println!(
            "push 完成：更新 {} 个文件，其中 {} 个重试后仍未在远端确认",
            todo.len(),
            missing_paths.len()
        );
    }
    Ok(())
}

struct ProgressDisplay {
    terminal: bool,
    last_attempt: usize,
    last_percent: u64,
}

impl ProgressDisplay {
    fn new() -> Self {
        Self {
            terminal: std::io::stdout().is_terminal(),
            last_attempt: 0,
            last_percent: 0,
        }
    }

    fn update(
        &mut self,
        index: usize,
        total_files: usize,
        path: &str,
        attempt: usize,
        bytes: u64,
        total: u64,
    ) {
        if !self.terminal {
            return;
        }
        let (percent, filled) = if total == 0 {
            (100, PROGRESS_WIDTH)
        } else {
            let current = bytes.min(total) as u128;
            let total = total as u128;
            (
                (current * 100 / total) as u64,
                (current * PROGRESS_WIDTH as u128 / total) as usize,
            )
        };
        if attempt == self.last_attempt && percent == self.last_percent && bytes != total {
            return;
        }
        self.last_attempt = attempt;
        self.last_percent = percent;
        let bar = format!(
            "{}{}",
            "#".repeat(filled),
            "-".repeat(PROGRESS_WIDTH - filled)
        );
        print!(
            "\rpush [{index}/{total_files}] {path} [{bar}] {:>3}% {bytes}/{total} B（第 {attempt} 次）",
            percent
        );
        let _ = std::io::stdout().flush();
    }

    fn finish(&mut self, index: usize, total_files: usize, path: &str, attempt: usize, total: u64) {
        if self.terminal {
            self.update(index, total_files, path, attempt, total, total);
            println!();
        } else {
            println!("push      {path}");
        }
    }

    fn line_break(&mut self) {
        if self.terminal {
            println!();
        }
    }
}

fn upload_file(
    dav: &WebDav,
    root: &Path,
    entry: &Entry,
    index: usize,
    total_files: usize,
    display: &Arc<Mutex<ProgressDisplay>>,
) -> Result<()> {
    let path = entry.path.clone();
    let data = fs::read(root.join(&path)).with_context(|| format!("读取本地文件失败：{path}"))?;
    let progress_display = Arc::clone(display);
    let progress_path = path.clone();
    let result = dav.put_with_progress(&path, &data, move |attempt, bytes, total| {
        if let Ok(mut display) = progress_display.lock() {
            display.update(index, total_files, &progress_path, attempt, bytes, total);
        }
    });
    match result {
        Ok(attempt) => {
            if let Ok(mut display) = display.lock() {
                display.finish(index, total_files, &path, attempt, data.len() as u64);
            }
            Ok(())
        }
        Err(err) => {
            if let Ok(mut display) = display.lock() {
                display.line_break();
            }
            Err(err).with_context(|| format!("上传本地文件失败：{path}"))
        }
    }
}

fn parent_dirs(path: &str) -> Vec<&str> {
    let mut dirs = Vec::new();
    let mut cur = path;
    while let Some((parent, _)) = cur.rsplit_once('/') {
        dirs.push(parent);
        cur = parent;
    }
    dirs
}

/// 下载到临时文件再原子替换，避免留下半写入的文件；并保留远端 mtime。
fn download(dav: &WebDav, root: &Path, e: &Entry) -> Result<()> {
    let data = dav.get(&e.path)?;
    let target = root.join(&e.path);
    let dir = target.parent().context("目标文件缺少父目录")?;
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&data)?;
    tmp.persist(&target).map_err(|err| err.error)?;
    filetime::set_file_mtime(
        &target,
        filetime::FileTime::from_unix_time(e.mtime.max(0), 0),
    )?;
    Ok(())
}

fn connect_repo() -> Result<(PathBuf, WebDav)> {
    let dot = config::repo_dir()?;
    let cfg = config::load(&dot)?;
    let root = dot.parent().context("仓库目录缺少父目录")?.to_path_buf();
    Ok((root, connect(&cfg)?))
}

fn connect(cfg: &Config) -> Result<WebDav> {
    let r = &cfg.remote;
    if r.url.is_empty() {
        bail!("尚未配置远端仓库，请先运行 anysync config");
    }
    let password = if !r.password.is_empty() {
        r.password.clone()
    } else if r.username.is_empty() {
        String::new()
    } else if std::io::stdin().is_terminal() {
        // rpassword 自带的提示输出在中文 Windows 下会乱码，改由标准输出打印
        print!("请输入 {} 的密码：", r.username);
        std::io::stdout().flush()?;
        rpassword::read_password()?
    } else {
        // 非交互环境（脚本/管道）从 stdin 读一行，便于 echo pass | anysync push
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line.trim().to_string()
    };
    WebDav::new(&r.url, &r.root, &r.username, &password)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, mtime: i64, size: u64) -> Entry {
        Entry {
            path: path.to_string(),
            mtime,
            size,
            is_dir: false,
        }
    }

    #[test]
    fn missing_dest_transfers() {
        let src = vec![e("a.txt", 100, 1)];
        assert_eq!(plan(&src, &[])[0].1, Action::Transfer);
    }

    #[test]
    fn newer_source_transfers() {
        let src = vec![e("a.txt", 200, 1)];
        let dst = vec![e("a.txt", 100, 1)];
        assert_eq!(plan(&src, &dst)[0].1, Action::Transfer);
    }

    #[test]
    fn newer_dest_skips() {
        let src = vec![e("a.txt", 100, 1)];
        let dst = vec![e("a.txt", 200, 1)];
        assert_eq!(plan(&src, &dst)[0].1, Action::Skip);
    }

    #[test]
    fn same_time_same_size_skips() {
        let src = vec![e("a.txt", 100, 1)];
        let dst = vec![e("a.txt", 100, 1)];
        assert_eq!(plan(&src, &dst)[0].1, Action::Skip);
    }

    #[test]
    fn same_time_different_size_conflicts() {
        let src = vec![e("a.txt", 100, 1)];
        let dst = vec![e("a.txt", 100, 2)];
        assert_eq!(plan(&src, &dst)[0].1, Action::Conflict);
    }

    #[test]
    fn within_tolerance_treated_as_same() {
        let src = vec![e("a.txt", 100, 1)];
        let dst = vec![e("a.txt", 100 + TOLERANCE, 1)];
        assert_eq!(plan(&src, &dst)[0].1, Action::Skip);
    }

    #[test]
    fn dirs_are_ignored() {
        let src = vec![Entry {
            path: "d".into(),
            mtime: 0,
            size: 0,
            is_dir: true,
        }];
        assert!(plan(&src, &[]).is_empty());
    }

    #[test]
    fn parent_dirs_are_ordered() {
        assert_eq!(parent_dirs("a/b/c.txt"), vec!["a/b", "a"]);
    }
}
