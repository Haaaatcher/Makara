//! 生态适配（D59，docs/design.md §2 ADR）：把 vault 记录供给到各 AI 工具的配置文件。
//!
//! 职责与边界：
//! - 每个"生态"（Claude Code、Codex……）是一个 [`Target`] 实现单元，职责收敛为
//!   三件事：判定记录协议是否适用（[`Fit`]）、给出目标配置文件路径（[`Scope`]）、
//!   把凭证写入配置（备份 + 原子写）。
//! - 这是**定向适配**而非通用字段映射引擎（旧配方引擎已随 D17–D23 删除，不翻案）：
//!   每个 target 写死自己认识的字段路径，上游 schema 变更由 fixture 快照测试先行
//!   暴露（测试红 = 上游漂移，补适配后更新快照并在此登记核对日期）。
//! - core 不感知任何 UI：本模块只提供纯逻辑，`use` 命令（CLI）与 GUI 按钮共用。
//!
//! 安全约束（D59 + AGENTS.md 铁律）：
//! - 明文 key 只落**用户显式发起**的写入所指向的目标配置文件，绝不进日志、
//!   错误信息或任何终端输出（展示一律走 [`FieldWrite::display`] / [`mask_secret`] 掩码）；
//! - 写前备份原文件（`<file>.bak`，仅在不存在时创建——保住"接入 makara 之前"
//!   的原始状态，后续覆盖式 apply 不抹掉它）；
//! - 原子写（临时文件 + rename，unix 0600），任一步失败都不破坏既有配置；
//! - 项目级共享变体（通常会被提交进 git）必须由调用方显式选择（[`Scope::ProjectShared`]），
//!   本模块不做"猜一个写进去"。

use crate::error::{Error, Result};
use crate::model::{Protocol, Record};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// 目标抽象
// ---------------------------------------------------------------------------

/// 协议适配判定结果（D59 协议族门控）。
///
/// 依据是**用户手填的协议标签**（[`Protocol`]），不做任何 key 前缀 / URL
/// 形态推断——标签之外的证据一概不看。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fit {
    /// 直接适用，无需确认。
    Yes,
    /// 可以写入，但需要用户确认（legacy 协议 / 自定义标签——与目标实际
    /// 期望的 API 形态存在合理怀疑空间）。`key` = 稳定提示键（GUI 双语
    /// 字典按它翻译，D70），`message` = 英文文案（CLI）。
    Confirm { key: &'static str, message: String },
    /// 不适用（协议族不对，写入必然不可用）。硬拒绝，`--yes` 也不能越过。
    No { key: &'static str, message: String },
}

/// [`Fit`] 的稳定提示键（D70）：GUI 双语字典的查表键；改文案不改键。
pub mod fit_key {
    /// Claude × OpenAI 族（含 chat）。
    pub const CLAUDE_OPENAI_FAMILY: &str = "fitClaudeOpenAiFamily";
    /// Claude × legacy Text Completions。
    pub const CLAUDE_LEGACY: &str = "fitClaudeLegacy";
    /// Claude × 自定义标签。
    pub const CLAUDE_CUSTOM: &str = "fitClaudeCustom";
    /// Codex × chat API 移除（OpenAiChat 标签）。
    pub const CODEX_CHAT_REMOVED: &str = "fitCodexChatRemoved";
    /// Codex × Anthropic 族。
    pub const CODEX_ANTHROPIC_FAMILY: &str = "fitCodexAnthropicFamily";
    /// Codex × 自定义标签。
    pub const CODEX_CUSTOM: &str = "fitCodexCustom";
}

/// 供给目标的配置面警示（D70）：key = 稳定提示键（GUI 双语查表），
/// message = 英文文案（CLI）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupplyWarning {
    /// 稳定提示键。
    pub key: &'static str,
    /// 英文文案。
    pub message: String,
}

/// 供给范围（D59）：决定写入哪个配置文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 用户级配置（该工具全局生效）。
    User,
    /// 项目级**个人**变体——工具约定被 gitignore、不进版本库的文件
    /// （Claude Code：`.claude/settings.local.json`）。`--project` 的默认形态。
    ProjectLocal,
    /// 项目级**共享**变体——工具约定会被提交进 git 的文件
    /// （Claude Code：`.claude/settings.json`）。明文 key 有随仓库泄露风险，
    /// 必须由用户显式指名（CLI `--shared`），本项目铁律"明文不进会被提交的
    /// 文件"由此把关：把关动作在调用方（要求显式 flag + 确认），本模块只认 scope。
    ProjectShared,
}

/// 模型映射（D64）：claude 别名 → 模型 ID 的写入集合；`None` 的别名
/// 本次**不动**。三个别名同指一个模型（网关单模型场景，D60）或按别名
/// 分派（多模型网关，如后台 haiku 指到便宜档）皆可表达。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelMapping {
    /// `opus` 别名（opusplan 的 Plan 模式也用它）。
    pub opus: Option<String>,
    /// `sonnet` 别名（opusplan 的非 Plan 模式也用它）。
    pub sonnet: Option<String>,
    /// `haiku` 别名（兼任后台任务模型）。
    pub haiku: Option<String>,
}

impl ModelMapping {
    /// 三别名同指一个模型的便捷构造（D60 主流场景）。
    pub fn all(model: &str) -> Self {
        Self {
            opus: Some(model.to_string()),
            sonnet: Some(model.to_string()),
            haiku: Some(model.to_string()),
        }
    }

    /// 是否没有任何映射（本次完全不碰映射键）。
    pub fn is_empty(&self) -> bool {
        self.opus.is_none() && self.sonnet.is_none() && self.haiku.is_none()
    }

    /// 按别名遍历（写入计划与落盘共用同一顺序）。
    fn roles(&self) -> [(&'static str, Option<&String>); 3] {
        [
            ("OPUS", self.opus.as_ref()),
            ("SONNET", self.sonnet.as_ref()),
            ("HAIKU", self.haiku.as_ref()),
        ]
    }
}

/// `apply` 的可选项。各 target 自行解释；未接目标的字段被忽略。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyOptions {
    /// Claude Code：`true` = 写 `env.ANTHROPIC_API_KEY`（x-api-key 形态，
    /// 官方 key 的常规形态）；默认 `false` = 写 `env.ANTHROPIC_AUTH_TOKEN`
    /// （Bearer 形态，三方网关/中转的主流形态）。
    ///
    /// 切换形态时另一形态的旧值会被**移除**（两种 token 并存时 Claude Code
    /// 的取舍不可控，留旧值等于埋雷）。
    pub api_key_style: bool,
    /// 模型映射（D60/D64）：写入 `ANTHROPIC_DEFAULT_OPUS_MODEL` /
    /// `_SONNET_MODEL` / `_HAIKU_MODEL`（别名解析层，haiku 兼任后台任务
    /// 模型）。**刻意不碰** settings `model` 键与 `ANTHROPIC_MODEL`——
    /// 两者优先级压过用户会话内 `/model` 手动切换，接管即打架。键名
    /// 2026-11-18 经官方文档核对（`ANTHROPIC_SMALL_FAST_MODEL` 已废弃，
    /// 不使用）。仅 [`ModelKind::Aliases`] 型目标消费。
    pub model_mapping: ModelMapping,
    /// 主模型（D66）：写入目标的主模型键（Codex：顶层 `model`）。仅
    /// [`ModelKind::Main`] 型目标消费；`None` = 不动该键。
    pub model: Option<String>,
}

/// 一次写入计划中的单个字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldWrite {
    /// 字段路径（展示用，如 `env.ANTHROPIC_BASE_URL`）。
    pub path: String,
    /// 新值。敏感字段这里是明文 key——只允许进内存与目标配置文件。
    pub value: String,
    /// 是否敏感（决定 [`FieldWrite::display`] 是否掩码）。
    pub secret: bool,
}

impl FieldWrite {
    /// 展示值：敏感字段掩码，普通字段原样。终端输出一律走本方法。
    pub fn display(&self) -> String {
        if self.secret {
            mask_secret(&self.value)
        } else {
            self.value.clone()
        }
    }
}

/// 掩码任意敏感串：保留前 4 后 4，中间 `...`；≤ 8 字符只露前 2。
/// 与 [`crate::model::Record::masked_key`] 同语义（独立实现是为了让
/// 非 Record 载体的敏感值——如配置文件里读到的旧 token——也能掩码）。
pub fn mask_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 8 {
        let head: String = chars.iter().take(2).collect();
        format!("{head}...")
    } else {
        let head: String = chars.iter().take(4).collect();
        let tail: String = chars.iter().skip(chars.len() - 4).collect();
        format!("{head}...{tail}")
    }
}

/// 目标的模型写入语义（D66）：CLI/GUI 据此渲染对应的模型选择交互。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// Claude 的三别名解析键（消费 [`ApplyOptions::model_mapping`]）。
    Aliases,
    /// 单一主模型键（消费 [`ApplyOptions::model`]），如 Codex 顶层 `model`。
    Main,
}

/// 目标配置文件中"当前已供给的凭证"（供写入前展示被替换的旧值）。
///
/// 字段按目标各自填充（同一结构承载多生态，D62 泛化在第二生态接入时
/// 落地）；配置文件不存在时 [`Target::current`] 直接返回 `None`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuppliedCreds {
    /// 当前生效的 base URL（供给来源字段）。
    pub base_url: Option<String>,
    /// Bearer 形态令牌（Claude `ANTHROPIC_AUTH_TOKEN`）。展示时须掩码。
    pub auth_token: Option<String>,
    /// x-api-key 形态令牌（Claude `ANTHROPIC_API_KEY`）。展示时须掩码。
    pub api_key: Option<String>,
    /// Codex `[model_providers.makara]` 表内的 bearer token（D66）。
    /// 展示时须掩码。
    pub bearer_token: Option<String>,
    /// `opus` 别名当前解析到的模型（D60 映射键）。
    pub opus_model: Option<String>,
    /// `sonnet` 别名当前解析到的模型（D60 映射键）。
    pub sonnet_model: Option<String>,
    /// `haiku` 别名当前解析到的模型（D60 映射键，兼任后台任务模型）。
    pub haiku_model: Option<String>,
    /// Codex 顶层 `model`（D66 主模型键）。
    pub model: Option<String>,
    /// Codex 顶层 `model_provider`（D66）——非 `makara` 时提示用户将
    /// 被覆盖。
    pub model_provider: Option<String>,
}

/// 一次 apply 的计划 / 结果（dry-run 与实际写共用同一结构，靠 `dry_run` 区分）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// 目标配置文件路径。
    pub path: PathBuf,
    /// 守护"接入 makara 前原貌"的备份路径。原文件不存在（首次创建配置）
    /// 时为 `None`；已存在时无论本次是否新写备份都回传路径。
    pub backup: Option<PathBuf>,
    /// 本次是否新建备份（`false` = 备份已存在，沿用首次接入时的原貌，
    /// 调用方可据此调整展示与提示）。
    pub backup_new: bool,
    /// 本次的字段写入。
    pub writes: Vec<FieldWrite>,
    /// 本次被移除的字段路径（如切换 token 形态时清掉的另一形态旧值）。
    pub removes: Vec<String>,
    /// 需要随计划一并展示的提示（如 Codex：顶层 `model_provider` 由其他
    /// 值被覆盖为 makara）。
    pub note: Option<String>,
    /// `true` = 仅计划未落盘。
    pub dry_run: bool,
}

/// 一个生态目标的抽象（D59）。实现单元 = 该生态配置面的全部知识。
pub trait Target {
    /// 稳定标识（CLI `--into` 的取值），如 `"claude"`。
    fn id(&self) -> &'static str;

    /// 人读名称，如 `"Claude Code"`。
    fn display_name(&self) -> &'static str;

    /// 协议适配判定（见 [`Fit`]）。
    fn fit(&self, protocol: &Protocol) -> Fit;

    /// 本机是否检测到该工具（粗粒度：其配置目录是否存在）。
    /// 未检测到时调用方应向用户确认后再写（避免给未安装的工具凭空造目录）。
    fn detect(&self, user_home: &Path) -> bool;

    /// 给定范围下的目标配置文件路径。
    ///
    /// - `user_home`：用户主目录（[`Scope::User`] 用）；
    /// - `project_root`：项目根目录（项目级 scope 用，CLI 取当前工作目录）。
    fn settings_path(&self, scope: Scope, user_home: &Path, project_root: &Path) -> PathBuf;

    /// 该目标支持的供给范围及面向用户的描述（D62，CLI 交互菜单按返回
    /// 顺序渲染，**首项即默认**）。
    ///
    /// "gitignored / committed" 这类语义是每个生态自己的约定——CLI 只负责
    /// 问，不硬编码任何生态的解释。目标不支持的范围根本不出现（交互选不
    /// 到；调用方对显式 flag 给出的不支持范围应报错）。
    fn supported_scopes(&self) -> Vec<(Scope, String)>;

    /// 凭证形态选项（D62，CLI 交互菜单文案）。`None` = 该目标无形态之分，
    /// 不问。布尔值即 [`ApplyOptions::api_key_style`] 的取值——当前唯一的
    /// 形态维度；未来生态需要更多维度时扩展 [`ApplyOptions`]。
    fn credential_styles(&self) -> Option<Vec<(bool, String)>>;

    /// 目标配置面的环境警示（D70）：与本目标供给相关的环境隐患，返回空
    /// = 无警示。`SupplyWarning::key` 为稳定提示键（GUI 双语字典查表），
    /// `message` 为英文文案（CLI）。顺序即展示顺序。
    fn config_warnings(&self, user_home: &Path) -> Vec<SupplyWarning>;

    /// 模型写入语义（D66）：决定 CLI/GUI 的模型选择交互形态与消费的
    /// [`ApplyOptions`] 字段。`None` = 该目标不消费模型。
    fn model_kind(&self) -> Option<ModelKind>;

    /// 读取目标配置文件中当前已供给的凭证。
    ///
    /// # Errors
    /// 文件存在但不是合法 JSON 时返回 [`Error::Format`]——配置已损坏时
    /// 严禁继续写入覆盖。
    fn current(&self, path: &Path) -> Result<Option<SuppliedCreds>>;

    /// 生成写入计划；`dry_run = false` 时执行（备份 + 原子写）。
    ///
    /// # Errors
    /// - [`Error::Format`]：既有配置不是合法 JSON（拒绝覆盖损坏文件）；
    /// - [`Error::Io`]：读 / 写 / 备份任一步失败（原子写保证此时原文件完好）。
    fn apply(
        &self,
        record: &Record,
        path: &Path,
        opts: ApplyOptions,
        dry_run: bool,
    ) -> Result<ApplyOutcome>;
}

/// 全部已接入的生态目标（D59 接入顺序：Claude Code → Codex（D66）→
/// Gemini CLI 后续逐个追加——追加 = 新增一个 [`Target`] 实现 + 在此
/// 登记 + fixture 测试）。
#[must_use]
pub fn all_targets() -> Vec<Box<dyn Target>> {
    vec![Box::new(ClaudeCode), Box::new(Codex)]
}

/// 按稳定标识查找目标。
#[must_use]
pub fn find_target(id: &str) -> Option<Box<dyn Target>> {
    all_targets().into_iter().find(|t| t.id() == id)
}

/// 当前用户主目录（目标配置多位于 `<home>/.claude` 等处）。
///
/// # Errors
/// 平台主目录环境变量（unix `HOME` / Windows `USERPROFILE`）不可得时返回
/// [`Error::Usage`]。
pub fn user_home_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let key = "USERPROFILE";
    #[cfg(not(windows))]
    let key = "HOME";
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error::Usage(format!("cannot determine the home directory (${key})")))
}

// ---------------------------------------------------------------------------
// Claude Code（D59 首个接入的生态）
// ---------------------------------------------------------------------------

/// Claude Code 适配器。
///
/// 配置面（字段路径 **2026-11-16 核对**，schema 漂移由本模块 fixture 测试锁定）：
///
/// | Scope | 文件 | 说明 |
/// |---|---|---|
/// | [`Scope::User`] | `<home>/.claude/settings.json` | 全局生效 |
/// | [`Scope::ProjectLocal`] | `<root>/.claude/settings.local.json` | 个人、gitignore |
/// | [`Scope::ProjectShared`] | `<root>/.claude/settings.json` | 共享、通常被提交 |
///
/// 写入字段（合入既有 JSON 的 `env` 对象，其余键原样保留）：
/// - `env.ANTHROPIC_BASE_URL` ← `record.base_url`（**原样写入，不归一化**——铁律）；
/// - `env.ANTHROPIC_AUTH_TOKEN`（默认）或 `env.ANTHROPIC_API_KEY`
///   （[`ApplyOptions::api_key_style`]）← `record.api_key`。
pub struct ClaudeCode;

/// Claude Code 配置文件名（各 scope 共用文件名、目录不同）。
const CLAUDE_SETTINGS_FILE: &str = "settings.json";

impl Target for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn fit(&self, protocol: &Protocol) -> Fit {
        match protocol {
            // Messages API 记录：Claude Code 的原生形态
            Protocol::Anthropic => Fit::Yes,
            // legacy Text Completions：同属 Anthropic 生态、凭证通常通用，
            // 但 API 形态与 Claude Code 期望不一致，写前确认一次
            Protocol::AnthropicComplete => Fit::Confirm {
                key: fit_key::CLAUDE_LEGACY,
                message: "legacy Anthropic protocol (Text Completions); Claude Code speaks the Messages API"
                    .to_string(),
            },
            // 自定义标签：无证据判断端点形态，写前确认一次
            Protocol::Custom { .. } => Fit::Confirm {
                key: fit_key::CLAUDE_CUSTOM,
                message: "custom protocol label; confirm this endpoint serves the Anthropic API"
                    .to_string(),
            },
            // OpenAI 族：Claude Code 不消费，硬拒绝
            Protocol::OpenAi | Protocol::OpenAiChat => Fit::No {
                key: fit_key::CLAUDE_OPENAI_FAMILY,
                message: "Claude Code speaks the Anthropic API; this record is OpenAI-family"
                    .to_string(),
            },
        }
    }

    fn detect(&self, user_home: &Path) -> bool {
        user_home.join(".claude").is_dir()
    }

    fn settings_path(&self, scope: Scope, user_home: &Path, project_root: &Path) -> PathBuf {
        match scope {
            Scope::User => user_home.join(".claude").join(CLAUDE_SETTINGS_FILE),
            Scope::ProjectLocal => project_root.join(".claude").join("settings.local.json"),
            Scope::ProjectShared => project_root.join(".claude").join(CLAUDE_SETTINGS_FILE),
        }
    }

    fn supported_scopes(&self) -> Vec<(Scope, String)> {
        vec![
            (
                Scope::User,
                "User config — all projects on this machine (recommended)".to_string(),
            ),
            (
                Scope::ProjectLocal,
                "Project config, personal — this project only, never committed to git".to_string(),
            ),
            (
                Scope::ProjectShared,
                "Project config, shared — this project only, committed to git; teammates get the key"
                    .to_string(),
            ),
        ]
    }

    fn credential_styles(&self) -> Option<Vec<(bool, String)>> {
        Some(vec![
            (
                false,
                "AUTH_TOKEN (recommended) — Bearer auth header, what most gateways / relays use"
                    .to_string(),
            ),
            (
                true,
                "API_KEY — x-api-key header (official api.anthropic.com or strict mirrors)"
                    .to_string(),
            ),
        ])
    }

    fn model_kind(&self) -> Option<ModelKind> {
        Some(ModelKind::Aliases)
    }

    fn config_warnings(&self, _user_home: &Path) -> Vec<SupplyWarning> {
        Vec::new()
    }

    fn current(&self, path: &Path) -> Result<Option<SuppliedCreds>> {
        let Some(value) = read_settings(path)? else {
            return Ok(None);
        };
        let env = value.get("env").and_then(Value::as_object);
        let get = |key: &str| {
            env.and_then(|e| e.get(key))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Ok(Some(SuppliedCreds {
            base_url: get("ANTHROPIC_BASE_URL"),
            auth_token: get("ANTHROPIC_AUTH_TOKEN"),
            api_key: get("ANTHROPIC_API_KEY"),
            opus_model: get("ANTHROPIC_DEFAULT_OPUS_MODEL"),
            sonnet_model: get("ANTHROPIC_DEFAULT_SONNET_MODEL"),
            haiku_model: get("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            ..SuppliedCreds::default()
        }))
    }

    fn apply(
        &self,
        record: &Record,
        path: &Path,
        opts: ApplyOptions,
        dry_run: bool,
    ) -> Result<ApplyOutcome> {
        // 既有配置原样载入（缺失 = 从空对象起步）；损坏即中止——覆盖坏
        // 文件等于销毁用户手写的全部其他设置
        let (existing, existed) = match read_settings(path)? {
            Some(v) => (v, true),
            None => (Value::Object(Map::new()), false),
        };

        // 备份语义：只在"接入 makara 前的原始文件"尚无备份时创建一次，
        // 后续 apply 不覆盖备份（保住最早的原貌，见模块文档）。
        // backup 恒回传路径（文件存在即有备份守护），backup_new 告知
        // 调用方本次是否会真的落备份
        let backup = existed.then(|| backup_path(path));
        let backup_new = backup.as_ref().is_some_and(|bak| !bak.exists());

        let (writes, removes) = plan_writes(&existing, record, &opts);

        let outcome = ApplyOutcome {
            path: path.to_path_buf(),
            backup,
            backup_new,
            writes,
            removes,
            note: None,
            dry_run: true,
        };
        if dry_run {
            return Ok(outcome);
        }

        // ---- 实际写盘（以下步骤任一失败都不破坏原文件）----
        let mut root = existing;
        merge_env(&mut root, record, &opts, path)?;
        let serialized = format!(
            "{}\n",
            serde_json::to_string_pretty(&root)
                .map_err(|e| Error::Serde(format!("settings serialization failed: {e}")))?
        );

        if outcome.backup_new {
            if let Some(bak) = &outcome.backup {
                std::fs::copy(path, bak)?;
            }
        }
        std::fs::create_dir_all(parent_of(path))?;
        // 原子写：同目录临时文件 + rename（config.rs 同款）；token 落盘
        // 的文件收 0600（unix），Windows 上 mode 无意义、属主已隔离
        let tmp = tmp_path(path);
        std::fs::write(&tmp, serialized.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)?;

        Ok(ApplyOutcome {
            dry_run: false,
            ..outcome
        })
    }
}

/// 构造写入计划：凭证两字段 + 模型映射三键（D60）+ 另一形态旧 token 的
/// 移除预告。与 [`merge_env`] 是同一份语义的"计划 / 执行"两面，改动须同步。
fn plan_writes(
    existing: &Value,
    record: &Record,
    opts: &ApplyOptions,
) -> (Vec<FieldWrite>, Vec<String>) {
    let (token_key, other_token_key) = token_keys(opts.api_key_style);
    let mut writes = vec![
        FieldWrite {
            path: "env.ANTHROPIC_BASE_URL".to_string(),
            value: record.base_url.clone(),
            secret: false,
        },
        FieldWrite {
            path: format!("env.{token_key}"),
            value: record.api_key.clone(),
            secret: true,
        },
    ];
    // 模型映射（D60/D64）：按别名写入（None 别名不动）；只动别名解析
    // 层，不写主模型层
    for (role, m) in opts.model_mapping.roles() {
        if let Some(m) = m {
            writes.push(FieldWrite {
                path: format!("env.ANTHROPIC_DEFAULT_{role}_MODEL"),
                value: m.clone(),
                secret: false,
            });
        }
    }
    let mut removes = Vec::new();
    let has_other = existing
        .get("env")
        .and_then(Value::as_object)
        .is_some_and(|e| e.contains_key(other_token_key));
    if has_other {
        removes.push(format!("env.{other_token_key}"));
    }
    (writes, removes)
}

/// 把凭证与模型映射合入根对象的 `env` 子树：插入/替换两个凭证字段、
/// 映射三键，并移除另一形态旧 token（键不存在时 remove 是无害空操作，
/// 与 [`plan_writes`] 预告的 `removes` 语义一致）。
///
/// # Errors
/// `env` 键被非对象值占用时返回 [`Error::Format`]（[`bad_shape`]）。
fn merge_env(root: &mut Value, record: &Record, opts: &ApplyOptions, path: &Path) -> Result<()> {
    let env_entry = root
        .as_object_mut()
        .ok_or_else(|| bad_shape(path))?
        .entry("env")
        .or_insert_with(|| Value::Object(Map::new()));
    let env = env_entry.as_object_mut().ok_or_else(|| bad_shape(path))?;
    env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        Value::String(record.base_url.clone()),
    );
    let (token_key, other_token_key) = token_keys(opts.api_key_style);
    env.insert(token_key.to_string(), Value::String(record.api_key.clone()));
    for (role, m) in opts.model_mapping.roles() {
        if let Some(m) = m {
            env.insert(
                format!("ANTHROPIC_DEFAULT_{role}_MODEL"),
                Value::String(m.clone()),
            );
        }
    }
    env.remove(other_token_key);
    Ok(())
}

/// 依据 token 形态开关给出（要写的键, 要移除的另一形态键）。
fn token_keys(api_key_style: bool) -> (&'static str, &'static str) {
    if api_key_style {
        ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN")
    } else {
        ("ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY")
    }
}

/// 备份文件路径：`<file>.bak`（同目录）。
fn backup_path(path: &Path) -> PathBuf {
    append_ext(path, "bak")
}

/// 临时文件路径：`<file>.tmp`（同目录，保证 rename 同盘原子）。
fn tmp_path(path: &Path) -> PathBuf {
    append_ext(path, "tmp")
}

/// 在文件全名后追加后缀（`settings.json` → `settings.json.bak`）——
/// 用 `with_extension` 会替换 `.json`，这里要保留原扩展名。
fn append_ext(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// 父目录：无父目录的退化输入（如裸文件名）按当前目录处理。
fn parent_of(path: &Path) -> PathBuf {
    path.parent().map_or_default(Path::to_path_buf)
}

/// 读取并解析目标配置文件；不存在返回 `None`。
///
/// # Errors
/// 文件存在但不是合法 JSON 对象时返回 [`Error::Format`]。
fn read_settings(path: &Path) -> Result<Option<Value>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Format(format!("{} is not valid JSON: {e}", path.display())))?;
    if !value.is_object() {
        return Err(Error::Format(format!(
            "{} must contain a JSON object",
            path.display()
        )));
    }
    Ok(Some(value))
}

/// `env` 之外的根级结构不是对象（`read_settings` 已保证根是对象，
/// 此处仅防御 `env` 键被占用为非对象值——如用户手写 `"env": []`）。
fn bad_shape(path: &Path) -> Error {
    Error::Format(format!(
        "{} has a non-object `env` key; fix it by hand before applying",
        path.display()
    ))
}

// ---------------------------------------------------------------------------
// Codex CLI（D66 第二个接入的生态）
// ---------------------------------------------------------------------------

/// Codex CLI 适配器。
///
/// 配置面（字段路径 **2026-11-19 经官方文档与多方来源核对**，漂移由
/// fixture 测试锁定）：
///
/// - 配置文件：`<CODEX_HOME 或 ~/.codex>/config.toml`，**仅用户级**（Codex
///   无项目级配置 → [`Target::supported_scopes`] 只返回 [`Scope::User`]；
///   `CODEX_HOME` 环境变量重定向已支持，D70）。
/// - 写入（TOML 树级合并，其余键原样保留）：
///   - 顶层 `model_provider = "makara"`（指向我们的 provider 表）；
///   - 顶层 `model` ← [`ApplyOptions::model`]（用户选定的主模型，None 不动）；
///   - `[model_providers.makara]` 表：`name = "Makara supplied"`、
///     `base_url` ← 记录原样（不归一化）、`wire_api = "responses"`（D69：
///     0.170.0 起 responses-only，OpenAiChat 记录在 fit 门控即拒）、
///     `experimental_bearer_token` ← key（见下）。
///
/// **key 落盘路径取舍（D66）**：Codex 自定义 provider 的 key 只有
/// `env_key`（环境变量——本产品不做 shell env 供给）/ `[auth]` command
/// 表（不合场景）/ `experimental_bearer_token`（明文落 config.toml）
/// 三条路，选后者：唯一不依赖 shell env 的"写完即生效"路径，文件收
/// 0600。`experimental` 键名是登记在案的漂移风险点，上游出正式机制即
/// 迁移；`auth.json` 的 `OPENAI_API_KEY` 只对 openai 官方 provider 生效，
/// 本适配器不写 auth.json。
pub struct Codex;

/// Codex 供给使用的 provider 表 id 与顶层 `model_provider` 取值。
const CODEX_PROVIDER_ID: &str = "makara";

/// Codex 配置目录：官方支持 `CODEX_HOME` 环境变量重定向（默认
/// `<home>/.codex`）——检测 / 路径 / 警示全线经由本函数，保证
/// 重定向场景行为一致（D70）。
fn codex_home(user_home: &Path) -> PathBuf {
    match std::env::var_os("CODEX_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => user_home.join(".codex"),
    }
}

/// 写入 provider 表的 `wire_api` 取值（D69）：Codex 0.170.0 起已移除
/// `wire_api = "chat"`（官方 discussion #7782，2026-02 完成移除），只支持
/// Responses API——chat 网关记录在 fit 门控即被拒绝，不产生必败配置。
const CODEX_WIRE_API: &str = "responses";

impl Target for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex CLI"
    }

    fn fit(&self, protocol: &Protocol) -> Fit {
        match protocol {
            // Responses API：0.170.0 起 Codex 的唯一形态（D69）
            Protocol::OpenAi => Fit::Yes,
            // Chat Completions 网关：写入必败（Codex 已移除 chat 支持）——
            // 硬拒并给出路：端点若实际服务 Responses API，改标签为 openai
            Protocol::OpenAiChat => Fit::No {
                key: fit_key::CODEX_CHAT_REMOVED,
                message:
                    "Codex now requires the Responses API (wire_api = \"chat\" was removed in \
                 recent versions); this record is labeled OpenAI Chat — relabel it as \
                 \"openai\" if the endpoint actually serves the Responses API"
                        .to_string(),
            },
            // 自定义标签：无从判断端点形态，按 responses 写入、确认时说明
            Protocol::Custom { .. } => Fit::Confirm {
                key: fit_key::CODEX_CUSTOM,
                message:
                    "custom protocol label; the provider will be written for the Responses API \
                 (wire_api = \"responses\")"
                        .to_string(),
            },
            // Anthropic 族：Codex 不消费，硬拒绝
            Protocol::Anthropic | Protocol::AnthropicComplete => Fit::No {
                key: fit_key::CODEX_ANTHROPIC_FAMILY,
                message: "Codex speaks the OpenAI API; this record is Anthropic-family".to_string(),
            },
        }
    }

    fn detect(&self, user_home: &Path) -> bool {
        codex_home(user_home).is_dir()
    }

    fn settings_path(&self, scope: Scope, user_home: &Path, _project_root: &Path) -> PathBuf {
        // Codex 只有用户级配置；项目级 scope 不会由 supported_scopes 给出
        let _ = scope;
        codex_home(user_home).join("config.toml")
    }

    fn supported_scopes(&self) -> Vec<(Scope, String)> {
        vec![(
            Scope::User,
            "User config — all projects on this machine (recommended)".to_string(),
        )]
    }

    // Codex 无 token 形态之分（bearer token 单一路径）——D62 None 语义
    fn credential_styles(&self) -> Option<Vec<(bool, String)>> {
        None
    }

    fn model_kind(&self) -> Option<ModelKind> {
        Some(ModelKind::Main)
    }

    /// Codex 的配置面警示（D70）：`auth.json` 中的 **ChatGPT 登录态**
    /// （`tokens` 字段）会在认证层压过自定义 provider 的 bearer token
    /// （0.170.0 实测；社区同款症状，标准解法 = `codex logout`）——
    /// 检测到即警示，修复动作留给用户执行。
    fn config_warnings(&self, user_home: &Path) -> Vec<SupplyWarning> {
        let mut v = Vec::new();
        let auth = codex_home(user_home).join("auth.json");
        if let Ok(raw) = std::fs::read_to_string(&auth) {
            let chatgpt_login = serde_json::from_str::<serde_json::Value>(&raw)
                .is_ok_and(|v| v.get("tokens").is_some());
            if chatgpt_login {
                v.push(SupplyWarning {
                    key: "warnCodexChatgptLogin",
                    message: "ChatGPT login detected — it takes precedence over the gateway \
                              configured by Makara. Run `codex logout` to use the gateway"
                        .to_string(),
                });
            }
        }
        v
    }

    fn current(&self, path: &Path) -> Result<Option<SuppliedCreds>> {
        let Some(value) = read_toml(path)? else {
            return Ok(None);
        };
        let provider = value
            .get("model_providers")
            .and_then(|p| p.get(CODEX_PROVIDER_ID));
        let get = |v: &toml::Value, key: &str| {
            v.get(key).and_then(toml::Value::as_str).map(str::to_string)
        };
        Ok(Some(SuppliedCreds {
            base_url: provider.and_then(|p| get(p, "base_url")),
            bearer_token: provider.and_then(|p| get(p, "experimental_bearer_token")),
            model: get(&value, "model"),
            model_provider: get(&value, "model_provider"),
            ..SuppliedCreds::default()
        }))
    }

    fn apply(
        &self,
        record: &Record,
        path: &Path,
        opts: ApplyOptions,
        dry_run: bool,
    ) -> Result<ApplyOutcome> {
        // 既有配置原样载入（缺失 = 从空表起步）；损坏即中止——覆盖坏
        // 文件等于销毁用户手写的全部其他设置
        let (existing, existed) = match read_toml(path)? {
            Some(v) => (v, true),
            None => (toml::Value::Table(toml::map::Map::default()), false),
        };

        // 备份语义与 Claude 适配器一致：守护"接入 makara 前的原貌"，只建一次
        let backup = existed.then(|| backup_path(path));
        let backup_new = backup.as_ref().is_some_and(|bak| !bak.exists());

        let (writes, old_provider) = codex_plan(&existing, record, &opts);
        // 旧 model_provider 非 makara：如实提示将被覆盖（用户手配的其他
        // provider 表不删，但顶层指向会被改走）
        let note = old_provider.filter(|p| p != CODEX_PROVIDER_ID).map(|p| {
            format!("top-level model_provider '{p}' will be replaced by '{CODEX_PROVIDER_ID}'")
        });
        let outcome = ApplyOutcome {
            path: path.to_path_buf(),
            backup,
            backup_new,
            writes,
            removes: Vec::new(),
            note,
            dry_run: true,
        };
        if dry_run {
            return Ok(outcome);
        }

        // ---- 实际写盘（以下步骤任一失败都不破坏原文件）----
        let mut root = existing;
        codex_merge(&mut root, record, &opts, path)?;
        let serialized = toml::to_string_pretty(&root)
            .map_err(|e| Error::Serde(format!("config serialization failed: {e}")))?;

        if outcome.backup_new {
            if let Some(bak) = &outcome.backup {
                std::fs::copy(path, bak)?;
            }
        }
        std::fs::create_dir_all(parent_of(path))?;
        // 原子写（config.rs 同款）；token 落盘的文件收 0600（unix）
        let tmp = tmp_path(path);
        std::fs::write(&tmp, serialized.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)?;

        Ok(ApplyOutcome {
            dry_run: false,
            ..outcome
        })
    }
}

/// Codex 的 wire_api 取值（D69）：恒为 responses——上游已移除 chat 支持，
/// 本适配器以生态最新版行为为准（见 [`Codex`] 文档与 design.md §10.1 适配登记）。
fn codex_wire_api() -> &'static str {
    CODEX_WIRE_API
}

/// 构造 Codex 写入计划；返回（写入集，顶层旧 model_provider 供覆盖提示）。
/// 旧值展示走 [`Target::current`]（SuppliedCreds），此处不重复承载。
fn codex_plan(
    existing: &toml::Value,
    record: &Record,
    opts: &ApplyOptions,
) -> (Vec<FieldWrite>, Option<String>) {
    let old_provider = existing
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .map(str::to_string);

    let mut writes = vec![
        FieldWrite {
            path: "model_provider".to_string(),
            value: CODEX_PROVIDER_ID.to_string(),
            secret: false,
        },
        FieldWrite {
            path: format!("model_providers.{CODEX_PROVIDER_ID}.base_url"),
            value: record.base_url.clone(),
            secret: false,
        },
        FieldWrite {
            path: format!("model_providers.{CODEX_PROVIDER_ID}.wire_api"),
            value: codex_wire_api().to_string(),
            secret: false,
        },
        FieldWrite {
            path: format!("model_providers.{CODEX_PROVIDER_ID}.experimental_bearer_token"),
            value: record.api_key.clone(),
            secret: true,
        },
    ];
    if let Some(m) = &opts.model {
        writes.push(FieldWrite {
            path: "model".to_string(),
            value: m.clone(),
            secret: false,
        });
    }
    (writes, old_provider)
}

/// 把 Codex 供给字段合入根表：顶层 model_provider/model + makara provider
/// 表（用户手写的其他键与其他 provider 表原样保留）。
///
/// # Errors
/// 根不是表 / `model_providers` 被占用为非表值时返回 [`Error::Format`]。
fn codex_merge(
    root: &mut toml::Value,
    record: &Record,
    opts: &ApplyOptions,
    path: &Path,
) -> Result<()> {
    let table = root.as_table_mut().ok_or_else(|| bad_shape(path))?;
    table.insert(
        "model_provider".to_string(),
        toml::Value::String(CODEX_PROVIDER_ID.to_string()),
    );
    if let Some(m) = &opts.model {
        table.insert("model".to_string(), toml::Value::String(m.clone()));
    }
    let providers = table
        .entry("model_providers")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::default()));
    let providers_table = providers.as_table_mut().ok_or_else(|| bad_shape(path))?;
    let makara = providers_table
        .entry(CODEX_PROVIDER_ID.to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::default()));
    let makara_table = makara.as_table_mut().ok_or_else(|| bad_shape(path))?;
    makara_table.insert(
        "name".to_string(),
        toml::Value::String("Makara supplied".to_string()),
    );
    makara_table.insert(
        "base_url".to_string(),
        toml::Value::String(record.base_url.clone()),
    );
    makara_table.insert(
        "wire_api".to_string(),
        toml::Value::String(codex_wire_api().to_string()),
    );
    makara_table.insert(
        "experimental_bearer_token".to_string(),
        toml::Value::String(record.api_key.clone()),
    );
    Ok(())
}

/// 读取并解析 TOML 配置；不存在返回 `None`。
///
/// # Errors
/// 文件存在但不是合法 TOML 表时返回 [`Error::Format`]。
fn read_toml(path: &Path) -> Result<Option<toml::Value>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let value: toml::Value = toml::from_str(&raw)
        .map_err(|e| Error::Format(format!("{} is not valid TOML: {e}", path.display())))?;
    if !value.is_table() {
        return Err(Error::Format(format!(
            "{} must contain a TOML table",
            path.display()
        )));
    }
    Ok(Some(value))
}

// ---------------------------------------------------------------------------
// 测试：fixture 快照锁定上游 schema（D59 漂移防线）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! fixture 快照测试：断言产出配置的完整结构。上游（Claude Code）
    //! 改 schema 时这里的断言先红——修适配、更新快照、在模块文档登记
    //! 新的核对日期，三步缺一不可。

    use super::*;
    use crate::model::Protocol;

    /// 每个测试独享的临时目录（uuid 隔离，不碰真实 `~/.claude`）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p =
                std::env::temp_dir().join(format!("mak-supply-{}-{}", tag, uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&p).expect("create temp dir");
            Self(p)
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }

        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.path(rel);
            std::fs::write(&p, content).expect("write fixture");
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 一条标准 Anthropic 测试记录（key 足够长，掩码/写入分支都走到）。
    fn anthropic_record() -> Record {
        Record::new(
            "relay".into(),
            Protocol::Anthropic,
            "https://relay.example.com".into(),
            "sk-ant-api03-abcdefghijklmnop".into(),
        )
    }

    fn settings_json(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).expect("settings file must exist"))
            .expect("settings must be valid JSON")
    }

    // ---- 协议族门控 ----

    #[test]
    fn fit_gates_by_protocol_family() {
        let t = ClaudeCode;
        assert_eq!(t.fit(&Protocol::Anthropic), Fit::Yes);
        assert!(matches!(
            t.fit(&Protocol::AnthropicComplete),
            Fit::Confirm { .. }
        ));
        assert!(matches!(
            t.fit(&Protocol::Custom {
                name: "gateway".into()
            }),
            Fit::Confirm { .. }
        ));
        // OpenAI 族硬拒绝：写进去也跑不通
        assert!(matches!(
            t.fit(&Protocol::OpenAi),
            Fit::No { message, .. }
                if message == "Claude Code speaks the Anthropic API; this record is OpenAI-family"
        ));
        assert!(matches!(t.fit(&Protocol::OpenAiChat), Fit::No { .. }));
    }

    // ---- 路径 ----

    #[test]
    fn settings_paths_per_scope() {
        let t = ClaudeCode;
        let home = Path::new("/home/u");
        let root = Path::new("/work/app");
        assert_eq!(
            t.settings_path(Scope::User, home, root),
            Path::new("/home/u/.claude/settings.json")
        );
        assert_eq!(
            t.settings_path(Scope::ProjectLocal, home, root),
            Path::new("/work/app/.claude/settings.local.json")
        );
        // 共享变体与用户级同文件名、不同目录——目录才是语义
        assert_eq!(
            t.settings_path(Scope::ProjectShared, home, root),
            Path::new("/work/app/.claude/settings.json")
        );
    }

    // ---- apply：fixture 快照 ----

    #[test]
    fn apply_user_creates_minimal_settings() {
        let dir = TempDir::new("create");
        let target = ClaudeCode.settings_path(Scope::User, &dir.0, Path::new("/unused"));
        let r = anthropic_record();

        let outcome = ClaudeCode
            .apply(&r, &target, ApplyOptions::default(), false)
            .expect("apply must succeed");
        assert!(!outcome.dry_run);
        assert!(outcome.backup.is_none(), "首次写入无备份");

        // 快照：完整结构（键排序由 serde_json 保证），上游 schema 漂移在此先红
        let v = settings_json(&target);
        let expect = serde_json::json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://relay.example.com",
                "ANTHROPIC_AUTH_TOKEN": "sk-ant-api03-abcdefghijklmnop",
            }
        });
        assert_eq!(v, expect);

        // 写入 2 空格缩进 + 末尾换行（人可读的常规形态）
        let raw = std::fs::read_to_string(&target).unwrap();
        assert!(raw.starts_with('{'));
        assert!(raw.ends_with("}\n"));
        assert!(raw.contains("\n  \"env\""), "应为 2 空格缩进");
    }

    #[test]
    fn apply_preserves_unrelated_keys() {
        let dir = TempDir::new("preserve");
        let target = dir.write(
            ".x.json",
            r#"{
  "permissions": { "allow": ["Bash(ls:*)"] },
  "model": "sonnet",
  "env": { "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "ANTHROPIC_AUTH_TOKEN": "old-token-value-1" }
}
"#,
        );
        let r = anthropic_record();

        let outcome = ClaudeCode
            .apply(&r, &target, ApplyOptions::default(), false)
            .expect("apply must succeed");

        // 快照：无关键原样、env 内其他键原样、两个目标字段被替换
        let v = settings_json(&target);
        let expect = serde_json::json!({
            "permissions": { "allow": ["Bash(ls:*)"] },
            "model": "sonnet",
            "env": {
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                "ANTHROPIC_BASE_URL": "https://relay.example.com",
                "ANTHROPIC_AUTH_TOKEN": "sk-ant-api03-abcdefghijklmnop",
            }
        });
        assert_eq!(v, expect);

        // 备份内容 = 接入前的原始全文；本次 apply 新落备份
        assert!(outcome.backup_new);
        let bak = outcome.backup.expect("已存在的文件必须有备份");
        assert_eq!(
            std::fs::read_to_string(&bak).unwrap(),
            r#"{
  "permissions": { "allow": ["Bash(ls:*)"] },
  "model": "sonnet",
  "env": { "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "ANTHROPIC_AUTH_TOKEN": "old-token-value-1" }
}
"#
        );
    }

    #[test]
    fn apply_is_idempotent_and_backup_survives_reapply() {
        let dir = TempDir::new("idem");
        let original = r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"first-original-key"},"model":"opus"}
"#;
        let target = dir.write(".x.json", original);
        let r = anthropic_record();

        ClaudeCode
            .apply(&r, &target, ApplyOptions::default(), false)
            .expect("first apply");
        let after_first = std::fs::read_to_string(&target).unwrap();
        ClaudeCode
            .apply(&r, &target, ApplyOptions::default(), false)
            .expect("second apply");
        let after_second = std::fs::read_to_string(&target).unwrap();

        assert_eq!(after_first, after_second, "重复 apply 结果必须一致");
        // 备份始终是接入前原貌，不被第二次 apply 覆盖
        let bak = backup_path(&target);
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), original);

        // backup_new 语义：首次 apply 新建备份，第二次沿用（不重写）
        let again = dir.write(".again.json", r#"{"model":"sonnet"}"#);
        let first = ClaudeCode
            .apply(&r, &again, ApplyOptions::default(), false)
            .expect("apply on another file");
        assert!(first.backup_new, "首次 apply 应新建备份");
        let second = ClaudeCode
            .apply(&r, &again, ApplyOptions::default(), false)
            .expect("reapply");
        assert!(!second.backup_new, "重复 apply 应沿用既有备份");
        assert_eq!(second.backup, first.backup, "备份路径稳定");
    }

    #[test]
    fn malformed_settings_aborts_without_touching_file() {
        let dir = TempDir::new("malformed");
        let broken = dir.write(".x.json", "{ not json !!!");
        let r = anthropic_record();

        let err = ClaudeCode
            .apply(&r, &broken, ApplyOptions::default(), false)
            .expect_err("损坏配置必须中止");
        assert!(err.to_string().contains("not valid JSON"));
        // 原文件一字不动
        assert_eq!(std::fs::read_to_string(&broken).unwrap(), "{ not json !!!");
        // current 同样拒绝，绝不静默当作空配置
        assert!(ClaudeCode.current(&broken).is_err());
    }

    #[test]
    fn style_switch_removes_the_other_token() {
        let dir = TempDir::new("style");
        let target = dir.write(
            ".x.json",
            r#"{"env":{"ANTHROPIC_API_KEY":"old-api-key-value","ANTHROPIC_AUTH_TOKEN":"keep-me"}}"#,
        );
        let r = anthropic_record();

        // 默认 AUTH_TOKEN 形态：残留的 API_KEY 被移除
        let outcome = ClaudeCode
            .apply(&r, &target, ApplyOptions::default(), false)
            .expect("apply must succeed");
        assert_eq!(outcome.removes, vec!["env.ANTHROPIC_API_KEY"]);
        let v = settings_json(&target);
        assert!(v["env"].get("ANTHROPIC_API_KEY").is_none());
        assert_eq!(
            v["env"]["ANTHROPIC_AUTH_TOKEN"],
            "sk-ant-api03-abcdefghijklmnop"
        );
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "https://relay.example.com");

        // 反向切换：API_KEY 形态移除 AUTH_TOKEN
        let outcome = ClaudeCode
            .apply(
                &r,
                &target,
                ApplyOptions {
                    api_key_style: true,
                    model_mapping: ModelMapping::default(),
                    model: None,
                },
                false,
            )
            .expect("apply must succeed");
        assert_eq!(outcome.removes, vec!["env.ANTHROPIC_AUTH_TOKEN"]);
        let v = settings_json(&target);
        assert!(v["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
        assert_eq!(
            v["env"]["ANTHROPIC_API_KEY"],
            "sk-ant-api03-abcdefghijklmnop"
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = TempDir::new("dry");
        let absent = dir.path("absent/settings.json");

        // 目标不存在：dry-run 给出完整计划但不建目录不建文件
        let outcome = ClaudeCode
            .apply(&anthropic_record(), &absent, ApplyOptions::default(), true)
            .expect("dry run must succeed");
        assert!(outcome.dry_run);
        assert!(outcome.backup.is_none());
        assert_eq!(outcome.writes.len(), 2);
        assert!(!absent.exists(), "dry-run 不得创建目标文件");
        assert!(!absent.parent().unwrap().exists(), "dry-run 不得创建目录");

        // 目标存在：内容原样
        let existing = dir.write(".x.json", r#"{"model":"sonnet"}"#);
        let before = std::fs::read_to_string(&existing).unwrap();
        let outcome = ClaudeCode
            .apply(
                &anthropic_record(),
                &existing,
                ApplyOptions::default(),
                true,
            )
            .expect("dry run must succeed");
        assert!(outcome.backup.is_some(), "计划里如实预告备份");
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            before,
            "dry-run 不得改动既有文件"
        );
        assert!(!backup_path(&existing).exists(), "dry-run 不得创建备份");
    }

    #[test]
    fn non_object_env_key_refused() {
        let dir = TempDir::new("envshape");
        let target = dir.write(".x.json", r#"{"env": [1, 2]}"#);
        let r = anthropic_record();
        let err = ClaudeCode
            .apply(&r, &target, ApplyOptions::default(), false)
            .expect_err("env 非对象必须中止");
        assert!(err.to_string().contains("`env`"));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            r#"{"env": [1, 2]}"#
        );
    }

    // ---- 菜单元数据（D62：谁的知识谁保管）----

    #[test]
    fn menu_metadata_is_target_owned() {
        // 契约：范围选项集合与顺序归 target 所有，首项即默认范围；
        // CLI 据此渲染菜单与非交互默认值，不硬编码任何生态的解释
        let scopes = ClaudeCode.supported_scopes();
        assert_eq!(
            scopes.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            [Scope::User, Scope::ProjectLocal, Scope::ProjectShared]
        );
        assert!(scopes.iter().all(|(_, l)| !l.is_empty()), "文案不为空");

        // 形态选项：默认（false，AUTH_TOKEN）必须在列
        let styles = ClaudeCode.credential_styles().expect("claude 有形态之分");
        assert!(styles.iter().any(|(v, _)| !*v), "默认形态必须在选项里");

        // Codex（D66）：单范围（无项目级配置）、无形态之分、主模型语义
        assert_eq!(
            Codex
                .supported_scopes()
                .iter()
                .map(|(s, _)| *s)
                .collect::<Vec<_>>(),
            [Scope::User]
        );
        assert!(Codex.credential_styles().is_none());
        assert_eq!(Codex.model_kind(), Some(ModelKind::Main));
        assert_eq!(ClaudeCode.model_kind(), Some(ModelKind::Aliases));
    }

    // ---- Codex（D66）----

    /// 一条标准 OpenAI（Responses）测试记录。
    fn openai_record() -> Record {
        Record::new(
            "gateway".into(),
            Protocol::OpenAi,
            "https://gw.example.com/v1".into(),
            "sk-openai-abcdefghijklmnop".into(),
        )
    }

    fn codex_config(path: &Path) -> toml::Value {
        toml::from_str(&std::fs::read_to_string(path).expect("config file must exist"))
            .expect("config must be valid TOML")
    }

    #[test]
    fn codex_fit_gates_by_protocol_family() {
        assert_eq!(Codex.fit(&Protocol::OpenAi), Fit::Yes);
        let chat = Codex.fit(&Protocol::OpenAiChat);
        assert!(
            matches!(chat, Fit::No { message: ref m, .. } if m.contains("Responses API")),
            "OpenAiChat 必须硬拒并说明 responses-only，实际 {chat:?}"
        );
        assert!(matches!(
            Codex.fit(&Protocol::Custom { name: "gw".into() }),
            Fit::Confirm { .. }
        ));
        assert!(matches!(Codex.fit(&Protocol::Anthropic), Fit::No { .. }));
        assert!(matches!(
            Codex.fit(&Protocol::AnthropicComplete),
            Fit::No { .. }
        ));
    }

    #[test]
    fn codex_apply_creates_minimal_config() {
        let dir = TempDir::new("codex-create");
        let target = Codex.settings_path(Scope::User, &dir.0, Path::new("/unused"));
        let r = openai_record();
        let opts = ApplyOptions {
            model: Some("gpt-5.2".into()),
            ..ApplyOptions::default()
        };

        let outcome = Codex
            .apply(&r, &target, opts.clone(), true)
            .expect("dry run");
        assert!(outcome.note.is_none(), "全新配置无覆盖提示");
        assert!(outcome.dry_run);
        assert!(!target.exists(), "dry-run 不得落盘");

        let outcome = Codex
            .apply(&r, &target, opts, false)
            .expect("apply must succeed");
        assert!(outcome.backup.is_none(), "首次写入无备份");

        // 快照：完整结构（上游 schema 漂移在此先红）。键序由 toml crate
        // 保证（顶层键先、表后置）
        let v = codex_config(&target);
        let expected = [("model_provider", "makara"), ("model", "gpt-5.2")];
        for (k, want) in expected {
            assert_eq!(
                v.get(k).and_then(toml::Value::as_str),
                Some(want),
                "顶层 {k} 不符"
            );
        }
        let makara = v
            .get("model_providers")
            .and_then(|p| p.get("makara"))
            .expect("makara provider 表必须存在");
        assert_eq!(
            makara.get("name").and_then(toml::Value::as_str),
            Some("Makara supplied")
        );
        assert_eq!(
            makara.get("base_url").and_then(toml::Value::as_str),
            Some("https://gw.example.com/v1")
        );
        // wire_api ← 协议标签映射（D28）：OpenAi → responses
        assert_eq!(
            makara.get("wire_api").and_then(toml::Value::as_str),
            Some("responses")
        );
        assert_eq!(
            makara
                .get("experimental_bearer_token")
                .and_then(toml::Value::as_str),
            Some("sk-openai-abcdefghijklmnop")
        );

        // current 读回（旧值展示链路）
        let creds = Codex.current(&target).unwrap().expect("exists");
        assert_eq!(creds.base_url.as_deref(), Some("https://gw.example.com/v1"));
        assert_eq!(
            creds.bearer_token.as_deref(),
            Some("sk-openai-abcdefghijklmnop")
        );
        assert_eq!(creds.model.as_deref(), Some("gpt-5.2"));
        assert_eq!(creds.model_provider.as_deref(), Some("makara"));
    }

    #[test]
    fn codex_wire_api_is_always_responses() {
        // D69：0.170.0 起 responses-only——wire_api 恒写 responses；
        // OpenAiChat 记录在 fit 门控即被硬拒（不再产出必败配置）
        let dir = TempDir::new("codex-wire");
        let target = dir.path(".x.toml");
        let mut r = openai_record();
        Codex
            .apply(&r, &target, ApplyOptions::default(), false)
            .expect("apply must succeed");
        let v = codex_config(&target);
        assert_eq!(
            v["model_providers"]["makara"]["wire_api"].as_str(),
            Some("responses")
        );
        let before = std::fs::read_to_string(&target).unwrap();

        // OpenAiChat：fit 即硬拒，什么都不写
        let mut chat = openai_record();
        chat.protocol = Protocol::OpenAiChat;
        assert!(matches!(
            Codex.fit(&Protocol::OpenAiChat),
            Fit::No { message: ref m, .. } if m.contains("Responses API")
        ));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            before,
            "被拒记录不得触碰文件"
        );

        // Custom 标签（经 Confirm 放行后）恒 responses
        r.protocol = Protocol::Custom { name: "gw".into() };
        Codex
            .apply(&r, &target, ApplyOptions::default(), false)
            .expect("apply must succeed");
        let v = codex_config(&target);
        assert_eq!(
            v["model_providers"]["makara"]["wire_api"].as_str(),
            Some("responses")
        );
    }

    #[test]
    fn codex_preserves_unrelated_keys_and_other_providers() {
        let dir = TempDir::new("codex-preserve");
        let target = dir.write(
            ".x.toml",
            r#"approval_policy = "on-request"
model = "gpt-5.1"
model_provider = "openai"

[model_providers.openai]
name = "OpenAI"
base_url = "https://api.openai.com/v1"

[model_providers.other_gw]
name = "Other"
base_url = "https://other.example.com/v1"
wire_api = "chat"
"#,
        );
        let before_other = codex_config(&target)["model_providers"]["other_gw"].clone();
        let r = openai_record();

        let outcome = Codex
            .apply(&r, &target, ApplyOptions::default(), false)
            .expect("apply must succeed");

        // 快照：approval_policy 原样；openai / other_gw 表原样；顶层
        // model_provider 被改指 makara（带提示）；makara 表写入
        let v = codex_config(&target);
        assert_eq!(
            v.get("approval_policy").and_then(toml::Value::as_str),
            Some("on-request")
        );
        assert!(v["model_providers"].get("openai").is_some());
        assert_eq!(v["model_providers"]["other_gw"], before_other);
        assert_eq!(
            v.get("model_provider").and_then(toml::Value::as_str),
            Some("makara")
        );
        assert!(v["model_providers"]["makara"].is_table());

        // note 提示覆盖行为；备份 = 接入前原貌
        assert!(
            outcome
                .note
                .as_deref()
                .is_some_and(|n| n.contains("model_provider") && n.contains("openai")),
            "覆盖 model_provider 必须有提示，实际 {:?}",
            outcome.note
        );
        let bak = outcome.backup.expect("已存在文件必须有备份");
        let bak_text = std::fs::read_to_string(&bak).unwrap();
        assert!(bak_text.contains("approval_policy"), "备份必须是原貌");
        assert!(!bak_text.contains("makara"), "备份不得含写入后内容");

        // current：旧 model_provider 展示（本次已变为 makara）
        let creds = Codex.current(&target).unwrap().expect("exists");
        assert_eq!(creds.model_provider.as_deref(), Some("makara"));
    }

    #[test]
    fn codex_model_none_leaves_model_key_untouched() {
        let dir = TempDir::new("codex-model");
        let target = dir.write(".x.toml", "model = \"user-picked\"\n");
        let r = openai_record();
        Codex
            .apply(&r, &target, ApplyOptions::default(), false)
            .expect("apply must succeed");
        let v = codex_config(&target);
        assert_eq!(
            v.get("model").and_then(toml::Value::as_str),
            Some("user-picked"),
            "opts.model = None 时顶层 model 键原样"
        );
    }

    #[test]
    fn codex_malformed_config_aborts_and_is_idempotent() {
        let dir = TempDir::new("codex-bad");
        let broken = dir.write(".x.toml", "not [valid toml ===");
        let r = openai_record();
        let err = Codex
            .apply(&r, &broken, ApplyOptions::default(), false)
            .expect_err("损坏配置必须中止");
        assert!(err.to_string().contains("not valid TOML"));
        assert_eq!(
            std::fs::read_to_string(&broken).unwrap(),
            "not [valid toml ==="
        );
        assert!(Codex.current(&broken).is_err());

        // 幂等 + 备份只建一次
        let good = dir.write(".y.toml", "approval_policy = \"never\"\n");
        let opts = ApplyOptions {
            model: Some("gpt-5.2".into()),
            ..ApplyOptions::default()
        };
        Codex.apply(&r, &good, opts.clone(), false).expect("first");
        let after_first = std::fs::read_to_string(&good).unwrap();
        Codex.apply(&r, &good, opts, false).expect("second");
        assert_eq!(
            std::fs::read_to_string(&good).unwrap(),
            after_first,
            "重复 apply 结果一致"
        );
        assert!(backup_path(&good).exists());
        assert!(
            backup_path(&good)
                .to_string_lossy()
                .ends_with(".y.toml.bak"),
            "备份 = 原文件全名 + .bak"
        );
    }

    // ---- 模型映射（D60）----

    #[test]
    fn model_mapping_writes_three_aliases_and_overwrites() {
        let dir = TempDir::new("model");
        // 预置用户手写的旧映射 + 无关键：验证覆盖范围恰好三键、无关键不动
        let target = dir.write(
            ".x.json",
            r#"{"model":"sonnet","env":{"ANTHROPIC_DEFAULT_SONNET_MODEL":"claude-3-5-sonnet-20241022","ANTHROPIC_SMALL_FAST_MODEL":"legacy-haiku"}}"#,
        );
        let r = anthropic_record();
        let opts = ApplyOptions {
            model_mapping: ModelMapping::all("glm-5.2"),
            ..ApplyOptions::default()
        };

        let outcome = ClaudeCode
            .apply(&r, &target, opts.clone(), true)
            .expect("dry run");
        assert_eq!(
            outcome
                .writes
                .iter()
                .map(|w| w.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "env.ANTHROPIC_BASE_URL",
                "env.ANTHROPIC_AUTH_TOKEN",
                "env.ANTHROPIC_DEFAULT_OPUS_MODEL",
                "env.ANTHROPIC_DEFAULT_SONNET_MODEL",
                "env.ANTHROPIC_DEFAULT_HAIKU_MODEL",
            ],
            "三键同指，别名解析层完整"
        );
        let outcome = ClaudeCode.apply(&r, &target, opts, false).expect("apply");

        // 快照：三键同指 glm-5.2；sonnet 旧映射被覆盖；废弃键 SMALL_FAST
        // 不归我们管（原样保留）；主模型 model 键不碰（D60 只动别名层）
        let v = settings_json(&target);
        let expect = serde_json::json!({
            "model": "sonnet",
            "env": {
                "ANTHROPIC_SMALL_FAST_MODEL": "legacy-haiku",
                "ANTHROPIC_BASE_URL": "https://relay.example.com",
                "ANTHROPIC_AUTH_TOKEN": "sk-ant-api03-abcdefghijklmnop",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-5.2",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.2",
            }
        });
        assert_eq!(v, expect);

        // current 读回三键（供 Old 行展示）
        let creds = ClaudeCode.current(&target).unwrap().expect("exists");
        assert_eq!(creds.opus_model.as_deref(), Some("glm-5.2"));
        assert_eq!(creds.sonnet_model.as_deref(), Some("glm-5.2"));
        assert_eq!(creds.haiku_model.as_deref(), Some("glm-5.2"));
        let _ = outcome;
    }

    #[test]
    fn per_alias_mapping_touches_only_given_aliases() {
        let dir = TempDir::new("peralias");
        // 预置三键旧值：分派只写给定的别名（None 别名原样保留）
        let target = dir.write(
            ".x.json",
            r#"{"env":{"ANTHROPIC_DEFAULT_OPUS_MODEL":"old-opus","ANTHROPIC_DEFAULT_SONNET_MODEL":"old-sonnet","ANTHROPIC_DEFAULT_HAIKU_MODEL":"old-haiku"}}"#,
        );
        let r = anthropic_record();
        let opts = ApplyOptions {
            model_mapping: ModelMapping {
                sonnet: Some("glm-5-turbo".into()),
                ..ModelMapping::default()
            },
            ..ApplyOptions::default()
        };

        let outcome = ClaudeCode
            .apply(&r, &target, opts, false)
            .expect("apply must succeed");
        // 计划只含 sonnet 一个映射写入（+ 凭证两字段）
        let paths: Vec<_> = outcome.writes.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "env.ANTHROPIC_BASE_URL",
                "env.ANTHROPIC_AUTH_TOKEN",
                "env.ANTHROPIC_DEFAULT_SONNET_MODEL"
            ]
        );

        let v = settings_json(&target);
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "old-opus");
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "glm-5-turbo");
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "old-haiku");
    }

    #[test]
    fn model_none_leaves_mapping_untouched() {
        let dir = TempDir::new("nomodel");
        let target = dir.write(
            ".x.json",
            r#"{"env":{"ANTHROPIC_DEFAULT_SONNET_MODEL":"user-picked"}}"#,
        );
        let r = anthropic_record();

        // model = None：完全不碰映射键（--no-model 语义）
        ClaudeCode
            .apply(&r, &target, ApplyOptions::default(), false)
            .expect("apply must succeed");
        let v = settings_json(&target);
        assert_eq!(
            v["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "user-picked",
            "None = 映射键原样"
        );
        assert!(v["env"].get("ANTHROPIC_DEFAULT_OPUS_MODEL").is_none());
    }

    // ---- current ----

    #[test]
    fn current_reads_fields_or_none() {
        let dir = TempDir::new("current");
        let absent = dir.path("none.json");
        assert_eq!(ClaudeCode.current(&absent).unwrap(), None);

        let target = dir.write(
            ".x.json",
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://a.b","ANTHROPIC_API_KEY":"abcd1234efgh"}}"#,
        );
        let creds = ClaudeCode.current(&target).unwrap().expect("file exists");
        assert_eq!(creds.base_url.as_deref(), Some("https://a.b"));
        assert_eq!(creds.auth_token, None);
        assert_eq!(creds.api_key.as_deref(), Some("abcd1234efgh"));
    }

    // ---- 掩码 ----

    #[test]
    fn mask_secret_matches_record_semantics() {
        assert_eq!(mask_secret("sk-ant-api03-abcdefghijklmnop"), "sk-a...mnop");
        assert_eq!(mask_secret("abcd"), "ab...");
        assert_eq!(mask_secret("12345678"), "12...");
        // 多字节安全（8 字符 = 短串分支，同 Record::masked_key）
        assert_eq!(mask_secret("密钥密钥密钥密钥"), "密钥...");
    }

    #[test]
    fn field_write_display_masks_secret_only() {
        let secret = FieldWrite {
            path: "env.ANTHROPIC_AUTH_TOKEN".into(),
            value: "sk-ant-api03-abcdefghijklmnop".into(),
            secret: true,
        };
        assert_eq!(secret.display(), "sk-a...mnop");
        let plain = FieldWrite {
            path: "env.ANTHROPIC_BASE_URL".into(),
            value: "https://relay.example.com".into(),
            secret: false,
        };
        assert_eq!(plain.display(), "https://relay.example.com");
    }

    #[test]
    fn detect_and_apply_never_touch_real_home() {
        // 本模块一切操作都基于显式路径；这里验证临时目录里的"伪 home"
        // 全流程可用，从机制上杜绝隐式依赖真实 `~` 的回归
        let dir = TempDir::new("home");
        assert!(!ClaudeCode.detect(&dir.0));
        std::fs::create_dir_all(dir.path(".claude")).unwrap();
        assert!(ClaudeCode.detect(&dir.0));

        let target = ClaudeCode.settings_path(Scope::User, &dir.0, Path::new("/unused"));
        ClaudeCode
            .apply(&anthropic_record(), &target, ApplyOptions::default(), false)
            .expect("apply must succeed");
        assert!(ClaudeCode.current(&target).unwrap().is_some());
    }
}
