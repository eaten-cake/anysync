use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "anysync",
    version,
    about = "基于 WebDAV 的极简多端文件同步工具"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// 在当前目录初始化 anysync 仓库
    Init,
    /// 配置远端仓库
    Config {
        /// WebDAV 地址
        #[arg(long)]
        url: Option<String>,
        /// 远端根目录
        #[arg(long)]
        root: Option<String>,
        /// 用户名
        #[arg(long)]
        username: Option<String>,
        /// 交互式输入密码（不回显，不作为命令行参数）
        #[arg(long)]
        password: bool,
    },
    /// 拉取远端较新的文件
    Pull,
    /// 推送本地较新的文件
    Push,
}
