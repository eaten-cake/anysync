use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub remote: Remote,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Remote {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

/// 从当前目录向上查找 .anysync 目录，类似 git。
pub fn repo_dir() -> Result<PathBuf> {
    let mut dir = std::env::current_dir().context("无法获取当前目录")?;
    loop {
        let dot = dir.join(".anysync");
        if dot.is_dir() {
            return Ok(dot);
        }
        if !dir.pop() {
            bail!("当前目录不在 anysync 仓库内，请先运行 anysync init");
        }
    }
}

pub fn init() -> Result<()> {
    let dot = PathBuf::from(".anysync");
    if dot.join("config.toml").exists() {
        println!("anysync 仓库已存在");
        return Ok(());
    }
    fs::create_dir_all(&dot)?;
    save(&dot, &Config::default())?;
    println!(
        "已初始化 anysync 仓库：{}",
        dot.join("config.toml").display()
    );
    Ok(())
}

pub fn load(dot: &Path) -> Result<Config> {
    let text = fs::read_to_string(dot.join("config.toml")).context("读取配置失败")?;
    toml::from_str(&text).context("解析配置失败")
}

fn save(dot: &Path, cfg: &Config) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(dot)?;
    write!(file, "{}", toml::to_string_pretty(cfg)?)?;
    file.persist(dot.join("config.toml"))
        .map_err(|err| err.error)?;
    Ok(())
}

/// remote config：指定参数时只更新对应项，无参数时逐项交互询问。
pub fn remote_config(
    url: Option<String>,
    root: Option<String>,
    username: Option<String>,
    ask_password: bool,
) -> Result<()> {
    let dot = repo_dir()?;
    let mut remote = load(&dot)?.remote;
    let interactive = url.is_none() && root.is_none() && username.is_none() && !ask_password;

    if interactive {
        remote.url = ask("远程仓库地址", &remote.url)?;
        remote.root = ask("根目录（追加到远程地址，如 test）", &remote.root)?;
        remote.username = ask("用户名", &remote.username)?;
    } else {
        if let Some(v) = url {
            remote.url = v;
        }
        if let Some(v) = root {
            remote.root = v;
        }
        if let Some(v) = username {
            remote.username = v;
        }
    }
    if interactive || ask_password {
        let password = prompt_password("密码（回车保持不变）: ")?;
        if !password.is_empty() {
            remote.password = password;
        }
    }
    save(&dot, &Config { remote })?;
    println!("配置已保存：{}", dot.join("config.toml").display());
    Ok(())
}

/// 交互终端下无回显读取；非交互环境从 stdin 读一行。
fn prompt_password(label: &str) -> Result<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        // rpassword 自带的提示输出在中文 Windows 下会乱码，改由标准输出打印
        print!("{label}");
        io::stdout().flush()?;
        Ok(rpassword::read_password()?)
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line.trim().to_string())
    }
}

fn ask(label: &str, current: &str) -> Result<String> {
    match current {
        "" => print!("{label}: "),
        _ => print!("{label} [{current}]: "),
    }
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let value = line.trim();
    Ok(if value.is_empty() {
        current.to_string()
    } else {
        value.to_string()
    })
}
