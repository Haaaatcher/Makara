//! 参数定义（clap derive）与子命令分发。
//!
//! 语言分层（AGENTS.md）：**帮助文本用英文**（通过显式 `about` / `help` 属性），
//! 本文件中的中文文档注释面向开发者，两者并存时 clap 以属性为准。

use crate::commands;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// 顶层命令行定义。`makara` 与 `myapikey` 共用。
#[derive(Debug, Parser)]
#[command(
    name = "makara",
    version,
    about = "Record and sync AI API credentials. Encrypted, synced via your own GitHub private repo",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// 全部子命令（docs/design.md §7）。
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 初始化：设主密码、建本地仓库，可选绑远端。
    #[command(
        about = "Initialize: set the passphrase, create the local repo, optionally bind a remote"
    )]
    /// 初始化本机（已初始化时自动转为保险合并式重绑，数据不丢）。
    ///
    /// 自动判断三件事：未授权 → 浏览器授权；仓库不存在 → 自动创建
    /// 私有仓库；远端有数据 → 下载并与本地合并。
    #[command(
        about = "Initialize: authorize GitHub if needed, create/reuse the private repo, download or upload the vault"
    )]
    Init {
        /// 仓库（可跳过交互直接指定）：`owner/name`、完整 GitHub URL，
        /// 或裸仓库名（归属当前授权账号）。缺省 = 交互式询问。
        #[arg(
            help = "GitHub repo (owner/name, full URL, or bare name); omit to be asked interactively"
        )]
        repo: Option<String>,
    },

    /// 交互式添加一条记录（缺省的参数会逐项询问）。
    #[command(about = "Add a record interactively. Omitted fields are prompted")]
    Add {
        /// 记录名称。
        #[arg(short, long, help = "Record name")]
        name: Option<String>,
        /// 协议标签。
        #[arg(
            short,
            long,
            help = "Protocol label: openai | anthropic | any custom text"
        )]
        protocol: Option<String>,
        /// base URL。
        #[arg(short, long, help = "Base URL")]
        base_url: Option<String>,
        /// API key。
        #[arg(
            short = 'k',
            long,
            help = "API key. Hidden input if omitted, recommended"
        )]
        api_key: Option<String>,
        /// 模型名（可重复；交互时也支持逗号分隔）。
        #[arg(
            long = "model",
            short = 'm',
            help = "Model name (repeatable, or comma-separated when prompted)"
        )]
        models: Vec<String>,
        /// 备注。
        #[arg(long, help = "Notes, optional")]
        notes: Option<String>,
    },

    /// 联网检查新 Release 并提示升级命令。
    #[command(about = "Check for a newer release and print the upgrade command")]
    Update,

    /// 列出全部记录（key 掩码显示）。
    #[command(about = "List all records. Keys are masked")]
    List,

    /// 模糊搜索记录（名称 / Base URL / 备注）。
    #[command(about = "Search records (fuzzy match on name, URL, notes)")]
    Search {
        /// 查询词：模糊匹配名称、Base URL、备注。
        #[arg(help = "Search query")]
        query: String,
    },

    /// 查看单条记录详情（key 永远掩码）。
    #[command(
        about = "Show one record. Key is always masked; use --copy to put it on the clipboard"
    )]
    Show {
        /// 记录的 id（前缀即可）或名称。
        #[arg(help = "Record id, prefix ok, or name")]
        target: String,
        /// 复制 key。
        #[arg(long, help = "Copy the API key to the clipboard")]
        copy: bool,
    },

    /// 复制一条记录的完整 key 到剪贴板（key 永不打印到终端）。
    #[command(about = "Copy a record's API key to the clipboard (the key is never printed)")]
    Key {
        /// 记录的 id（前缀即可）或名称。
        #[arg(help = "Record id, prefix ok, or name")]
        target: String,
    },

    /// 把一条记录的凭证写入 AI 工具的配置文件（D59 生态适配，当前支持 Claude Code）。
    // 文档注释保持单段：多段会被 clap 提升为 long_about（中文压过显式
    // 英文 about，违背语言分层）；行为细节见 commands::use_target 的函数注释
    /// 把一条记录的凭证写入 AI 工具的配置文件（D59 生态适配）。
    #[command(
        about = "Apply a record's credentials to an AI tool's config (currently Claude Code). The plan is shown first; the key lands in the config file, never on screen"
    )]
    Use {
        /// 记录的 id（前缀即可）或名称；缺省在终端里交互选择。
        #[arg(help = "Record id, prefix ok, or name; omit in a terminal to pick interactively")]
        target: Option<String>,
        /// 目标生态 id（缺省 = 唯一已接入目标）。
        #[arg(long, help = "Target ecosystem id; omit to use the only supported one")]
        into: Option<String>,
        /// 项目级配置（默认 gitignore 的个人变体）。
        #[arg(
            long,
            help = "Write the project-level config (default: the gitignored personal variant)"
        )]
        project: bool,
        /// 项目级共享变体（通常会被提交进 git，需 --project）。
        #[arg(
            long,
            help = "Use the project variant that is usually committed to git; requires --project"
        )]
        shared: bool,
        /// ANTHROPIC_API_KEY 形态（默认 ANTHROPIC_AUTH_TOKEN）。
        #[arg(
            long,
            help = "Use ANTHROPIC_API_KEY (x-api-key) instead of the default ANTHROPIC_AUTH_TOKEN (Bearer)"
        )]
        api_key: bool,
        /// 主模型（D66：Main 型目标如 Codex；Claude 等逐别名目标不支持）。
        #[arg(
            long,
            conflicts_with_all = ["model_map", "no_model"],
            help = "Main model id (for targets with a single model key, e.g. Codex); per-alias targets require --model-map instead"
        )]
        model: Option<String>,
        /// 按别名分派模型（D65 纯逐别名）：给定的别名才写；缺省在终端里逐别名交互映射。
        #[arg(
            long,
            conflicts_with_all = ["model", "no_model"],
            help = "Per-alias model mapping, e.g. opus=X,sonnet=Y,haiku=Z; aliases not mentioned stay untouched; omit in a terminal to map each alias interactively"
        )]
        model_map: Option<String>,
        /// 跳过模型映射（网关透传模型别名时用）。
        #[arg(
            long,
            conflicts_with_all = ["model", "model_map"],
            help = "Do not touch the model mapping keys"
        )]
        no_model: bool,
        /// 只展示计划，不写盘。
        #[arg(long, help = "Show what would be written; do not touch any file")]
        dry_run: bool,
        /// 跳过全部确认（非交互脚本用）。
        #[arg(long, help = "Skip all confirmations, for scripts")]
        yes: bool,
    },

    /// 编辑一条记录。
    #[command(about = "Edit a record field by field. Enter keeps the current value")]
    Edit {
        /// 记录的 id（前缀即可）或名称。
        #[arg(help = "Record id, prefix ok, or name")]
        target: String,
    },

    /// 删除一条记录（墓碑）。
    #[command(about = "Delete a record. Keeps a tombstone, purged after the grace period")]
    Delete {
        /// 记录的 id（前缀即可）或名称。
        #[arg(help = "Record id, prefix ok, or name")]
        target: String,
    },

    /// 校验 key 有效性。
    #[command(about = "Check key validity via GET /models. Checks all records if no target given")]
    Check {
        /// 记录的 id（前缀即可）或名称。
        #[arg(help = "Record id, prefix ok, or name")]
        target: Option<String>,
    },

    /// 同步。
    #[command(about = "Sync: pull + merge + push")]
    Sync,

    /// 查看仓库与同步状态。
    #[command(about = "Show repository and sync status")]
    Status,

    /// 管理远端仓库。
    #[command(about = "Manage the remote repository")]
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },

    /// 修改解锁密码（无参）。
    #[command(subcommand_required = false, about = "Change the passphrase")]
    Passwd {
        #[command(subcommand)]
        action: Option<PasswdAction>,
    },

    /// 立即锁定：清除本机记住的解锁密码（vault 不变）。
    #[command(
        about = "Lock now: forget the remembered passphrase on this machine (the vault is untouched)"
    )]
    Lock,

    /// 明文导出。
    #[command(
        about = "Export all records to a JSON file in PLAINTEXT. Asks twice before writing; no argument = the current directory"
    )]
    Export {
        /// 导出文件路径（省略 = 当前目录下 makara-export.json）。
        #[arg(help = "Output file path (default: current directory)")]
        path: Option<PathBuf>,
    },

    /// 导入。
    #[command(about = "Import records from a JSON file")]
    Import {
        /// 导入文件路径。
        #[arg(help = "Input file path")]
        path: PathBuf,
    },
}

/// `remote` 子命令。
#[derive(Debug, Subcommand)]
pub enum RemoteAction {
    /// 设置或更换远端仓库地址。
    #[command(about = "Set or replace the remote repository URL")]
    Set { url: String },
    /// 查看当前远端配置。
    #[command(about = "Show the current remote")]
    Show,
    /// 撤销绑定：解绑远端并清除本机的 GitHub 授权（回到单机；vault 不动）。
    #[command(
        about = "Unbind the remote and forget the GitHub authorization on this machine (the vault is untouched)"
    )]
    Unset,
}

/// `makara passwd` 的子命令。
#[derive(Debug, clap::Subcommand)]
pub enum PasswdAction {
    /// 修改解锁密码（`makara passwd` 无参的等价显式形式）。
    #[command(about = "Change the passphrase")]
    Set,
}

/// 入口：控制台编码守卫 → 解析 + 分发。
///
/// 守卫必须在任何输出之前建立、存活到 run 结束（Drop 时恢复代码页），
/// 因此放在函数体第一行。
///
/// # Errors
/// 子命令的任何错误原样上抛。
pub fn run() -> myapikey_core::Result<()> {
    let _console_guard = crate::console::enter_utf8();
    // 文件日志:静默动作(自动续期/迁移抢救/降级)落文件可查;目录来自
    // AppDirs(尊重 MYAPIKEY_HOME 隔离)。init 失败(目录建不了)降级无日志
    if let Ok(dirs) = myapikey_core::paths::AppDirs::load() {
        myapikey_core::log::init(&dirs.config_dir);
        if let Ok(cfg) = myapikey_core::config::AppConfig::load(&dirs) {
            myapikey_core::log::set_level(&cfg.log_level);
        }
        myapikey_core::log::info("cli", "command started");
    }
    let cli = Cli::parse();
    match cli.command {
        Command::Init { repo } => commands::init(repo.as_deref()),
        Command::Add {
            name,
            protocol,
            base_url,
            api_key,
            models,
            notes,
        } => commands::add(name, protocol, base_url, api_key, models, notes),
        Command::Update => commands::update(),
        Command::List => commands::list(None),
        Command::Search { query } => commands::list(Some(&query)),
        Command::Show { target, copy } => commands::show(&target, copy),
        Command::Key { target } => commands::key(&target),
        Command::Use {
            target,
            into,
            project,
            shared,
            api_key,
            model,
            model_map,
            no_model,
            dry_run,
            yes,
        } => {
            let scope = commands::UseScope::new(project, shared)?;
            let model = if let Some(m) = model {
                commands::ModelArg::MainModel(m)
            } else if let Some(spec) = model_map {
                commands::ModelArg::PerAlias(commands::parse_model_map(&spec)?)
            } else if no_model {
                commands::ModelArg::Skip
            } else {
                commands::ModelArg::Auto
            };
            commands::use_target(
                target.as_deref(),
                into.as_deref(),
                &commands::UseFlags {
                    scope,
                    model,
                    api_key_style: api_key,
                    dry_run,
                    yes,
                },
            )
        }
        Command::Edit { target } => commands::edit(&target),
        Command::Delete { target } => commands::delete(&target),
        Command::Check { target } => commands::check(target.as_deref()),
        Command::Sync => commands::sync(),
        Command::Status => commands::status(),
        Command::Remote { action } => commands::remote(action),
        Command::Passwd { action } => commands::passwd(action.as_ref()),
        Command::Lock => commands::lock_now(),
        Command::Export { path } => commands::export(path.as_deref()),
        Command::Import { path } => commands::import(&path),
    }
}
