use crate::backend::{self, Backend, Entry};
use crate::config::{self, Config};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use walkdir::WalkDir;

/// 比较修改时间时的容差（秒），用于吸收不同系统间的时钟与精度差异。
const TOLERANCE: i64 = 2;
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

fn is_ignored_path(path: &str) -> bool {
    path.split('/')
        .any(|part| part == ".anysync" || part == ".git")
}

/// 对比 src 与 dst 的文件列表，生成以 src 为源的执行计划。
pub fn plan<'a>(src: &'a [Entry], dst: &'a [Entry]) -> Vec<(&'a Entry, Action)> {
    let dst: BTreeMap<&str, &Entry> = dst.iter().map(|e| (e.path.as_str(), e)).collect();
    src.iter()
        .filter(|e| !e.is_dir && !is_ignored_path(&e.path))
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

pub fn status() -> Result<()> {
    let (root, dav) = connect_repo()?;
    let local = scan_local(&root)?;
    let remote = dav.list_recursive()?;
    let push_plan = plan(&local, &remote);
    let pull_plan = plan(&remote, &local);

    print_status_group("待推送", &push_plan, Action::Transfer);
    print_status_group("待拉取", &pull_plan, Action::Transfer);
    print_status_group("冲突", &push_plan, Action::Conflict);
    println!(
        "汇总：待推送 {}，待拉取 {}，冲突 {}",
        action_count(&push_plan, Action::Transfer),
        action_count(&pull_plan, Action::Transfer),
        action_count(&push_plan, Action::Conflict),
    );
    Ok(())
}

fn print_status_group(label: &str, plan: &[(&Entry, Action)], action: Action) {
    println!("{label}：");
    let paths: BTreeSet<&str> = plan
        .iter()
        .filter(|(_, candidate)| *candidate == action)
        .map(|(entry, _)| entry.path.as_str())
        .collect();
    if paths.is_empty() {
        println!("  （无）");
        return;
    }
    for path in paths {
        println!("  {path}");
    }
}

fn action_count(plan: &[(&Entry, Action)], action: Action) -> usize {
    plan.iter()
        .filter(|(_, candidate)| *candidate == action)
        .count()
}

pub fn pull() -> Result<()> {
    let (root, dav) = connect_repo()?;
    let local = scan_local(&root)?;
    let remote = dav.list_recursive()?;
    let mut updated = 0;
    for (e, action) in plan(&remote, &local) {
        match action {
            Action::Transfer => {
                download(dav.as_ref(), &root, e)?;
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
        .filter(|(_, action)| *action == Action::Transfer)
        .collect();
    if todo.is_empty() {
        println!("push 完成：远端已是最新");
        return Ok(());
    }

    // 先递归创建配置的远端根目录，再创建文件所在目录。
    dav.ensure_root()?;
    let mut dirs: Vec<&str> = todo
        .iter()
        .flat_map(|(entry, _)| parent_dirs(&entry.path))
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    for dir in dirs {
        dav.create_dir(dir)?;
    }

    let display = Arc::new(Mutex::new(ProgressDisplay::new()));
    let total = todo.len();
    for (index, (entry, _)) in todo.iter().enumerate() {
        upload_file(dav.as_ref(), &root, &entry.path, index + 1, total, &display)?;
    }
    finalize_push(dav.as_ref(), &root, &todo, total)
}

/// 上传完成后回读一次远端列表，仅对已可见文件执行 mtime 对齐。
/// 远端暂时不可见的文件不重传、不视为 push 失败，后续同步可继续处理。
fn finalize_push(
    dav: &dyn Backend,
    root: &Path,
    todo: &[(&Entry, Action)],
    total: usize,
) -> Result<()> {
    let remote: BTreeMap<String, Entry> = dav
        .list_recursive()?
        .into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| (e.path.clone(), e))
        .collect();

    for (entry, _) in todo {
        if let Some(remote) = remote.get(&entry.path)
            && let Err(err) = align_local_mtime(root, entry, remote)
        {
            println!(
                "告警：对齐 {} 的修改时间失败：{err}；下次 status 会显示该文件待拉取",
                entry.path
            );
        }
    }
    println!("push 完成：更新 {total} 个文件");
    Ok(())
}

/// 用回读的远端 mtime 对齐本地，避免 push 后立刻被判为「待拉取」。
/// 三条守卫下静默跳过对齐，只有写入本身失败才返回 Err。
fn align_local_mtime(root: &Path, local: &Entry, remote: &Entry) -> Result<()> {
    // 远端没给可用时间（解析失败会降级成 0），对齐会把本地时间戳写成 1970
    if remote.mtime <= 0 {
        return Ok(());
    }
    // 远端的字节未必是我们刚传的那份（第三方并发覆盖、服务端截断）。保持本地较旧是对的：
    // 此时的「待拉取」是真实状态，强行对齐会让对方的新内容再也拉不下来。
    if remote.size != local.size {
        return Ok(());
    }
    let target = root.join(&local.path);
    let meta = fs::metadata(&target)
        .with_context(|| format!("读取本地文件属性失败：{}", target.display()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // push 期间本地文件被改过：对齐会把新内容盖章成已同步，导致下次 push 跳过它
    if meta.len() != local.size || mtime != local.mtime {
        return Ok(());
    }
    set_local_mtime(&target, remote.mtime)
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

/// 上传单个文件。
fn upload_file(
    dav: &dyn Backend,
    root: &Path,
    path: &str,
    index: usize,
    total_files: usize,
    display: &Arc<Mutex<ProgressDisplay>>,
) -> Result<()> {
    let data = fs::read(root.join(path)).with_context(|| format!("读取本地文件失败：{path}"))?;
    let progress_display = Arc::clone(display);
    let progress_path = path.to_string();
    let result = dav.upload(
        path,
        &data,
        Arc::new(move |attempt, bytes, total| {
            if let Ok(mut display) = progress_display.lock() {
                display.update(index, total_files, &progress_path, attempt, bytes, total);
            }
        }),
    );
    match result {
        Ok(attempt) => {
            if let Ok(mut display) = display.lock() {
                display.finish(index, total_files, path, attempt, data.len() as u64);
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
fn download(dav: &dyn Backend, root: &Path, e: &Entry) -> Result<()> {
    let data = if e.size == 0 {
        Vec::new()
    } else {
        dav.download(&e.path)?
    };
    let target = root.join(&e.path);
    let dir = target.parent().context("目标文件缺少父目录")?;
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&data)?;
    tmp.persist(&target).map_err(|err| err.error)?;
    set_local_mtime(&target, e.mtime)
}

/// 把本地文件 mtime 写成给定的 Unix 秒。纳秒固定取 0，与 scan_local 的整秒口径
/// 一致，两端因此严格相等，下次 plan() 必然判 Skip，而不是靠 TOLERANCE 兜。
fn set_local_mtime(path: &Path, mtime: i64) -> Result<()> {
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(mtime.max(0), 0))
        .with_context(|| format!("写入文件修改时间失败：{}", path.display()))
}

fn connect_repo() -> Result<(PathBuf, Box<dyn Backend>)> {
    let dot = config::repo_dir()?;
    let cfg = config::load(&dot)?;
    let root = dot.parent().context("仓库目录缺少父目录")?.to_path_buf();
    Ok((root, connect(&cfg)?))
}

fn connect(cfg: &Config) -> Result<Box<dyn Backend>> {
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
    backend::connect(r, &password)
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
    fn newer_empty_source_transfers() {
        let src = vec![e("a.txt", 200, 0)];
        let dst = vec![e("a.txt", 100, 0)];
        assert_eq!(plan(&src, &dst)[0].1, Action::Transfer);
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
    fn status_classifies_one_sided_and_conflicting_files() {
        let local = vec![e("local.txt", 100, 1), e("conflict.txt", 100, 1)];
        let remote = vec![e("remote.txt", 100, 1), e("conflict.txt", 100, 2)];

        let push = plan(&local, &remote);
        let pull = plan(&remote, &local);

        assert_eq!(
            push.iter()
                .find(|(entry, _)| entry.path == "local.txt")
                .map(|(_, action)| *action),
            Some(Action::Transfer)
        );
        assert_eq!(
            pull.iter()
                .find(|(entry, _)| entry.path == "remote.txt")
                .map(|(_, action)| *action),
            Some(Action::Transfer)
        );
        assert_eq!(
            push.iter()
                .find(|(entry, _)| entry.path == "conflict.txt")
                .map(|(_, action)| *action),
            Some(Action::Conflict)
        );
    }

    #[test]
    fn parent_dirs_are_ordered() {
        assert_eq!(parent_dirs("a/b/c.txt"), vec!["a/b", "a"]);
    }

    #[test]
    fn mtime_roundtrip_closes_the_loop() {
        // 钉住 push 后对齐的正确性依据：远端整秒 -> 写回 -> scan_local 读回，
        // 三处口径一致则两值严格相等，plan() 双向都判 Skip。
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        fs::write(&file, b"hi").unwrap();

        let remote_mtime = 1_758_551_527;
        set_local_mtime(&file, remote_mtime).unwrap();

        let local = scan_local(dir.path()).unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].mtime, remote_mtime);

        let remote = vec![e("a.txt", remote_mtime, local[0].size)];
        assert_eq!(plan(&local, &remote)[0].1, Action::Skip);
        assert_eq!(plan(&remote, &local)[0].1, Action::Skip);
    }

    #[test]
    fn align_writes_remote_mtime() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let local = scan_local(dir.path()).unwrap().remove(0);

        align_local_mtime(dir.path(), &local, &e("a.txt", 1_700_000_000, 2)).unwrap();

        assert_eq!(scan_local(dir.path()).unwrap()[0].mtime, 1_700_000_000);
    }

    #[test]
    fn align_skips_on_guards() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let local = scan_local(dir.path()).unwrap().remove(0);
        let original = local.mtime;

        // G1：远端时间不可用，不能把本地写成 1970
        align_local_mtime(dir.path(), &local, &e("a.txt", 0, 2)).unwrap();
        assert_eq!(scan_local(dir.path()).unwrap()[0].mtime, original);

        // G2：远端字节不是我们传的那份，对齐会让对方的新内容再也拉不下来
        align_local_mtime(dir.path(), &local, &e("a.txt", 1_700_000_000, 99)).unwrap();
        assert_eq!(scan_local(dir.path()).unwrap()[0].mtime, original);

        // G3：push 期间本地被改过，对齐会把新内容盖章成已同步
        let stale = Entry {
            mtime: original - 3600,
            ..local.clone()
        };
        align_local_mtime(dir.path(), &stale, &e("a.txt", 1_700_000_000, 2)).unwrap();
        assert_eq!(scan_local(dir.path()).unwrap()[0].mtime, original);
    }
}
