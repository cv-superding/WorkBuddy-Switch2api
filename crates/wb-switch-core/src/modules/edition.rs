//! 版本（国内版 / 国际版）定义。
//!
//! 两个版本的差异全部实测确认（2026-09-24）：
//!
//! | 项 | 国内版 | 国际版 |
//! |---|---|---|
//! | 官方认证文件 | `workbuddy-desktop.info` | `workbuddy-desktop-ai.info` |
//! | 客户端进程 | `WorkBuddy(.exe)` | `WorkBuddyAI(.exe)` |
//! | 应用数据目录 | `~/.workbuddy` | `~/.workbuddy-ai` |
//! | API 域名 | `www.codebuddy.cn` | `www.workbuddy.ai` |
//! | `auth.scope` | 无 | `openid profile offline_access email` |
//!
//! 两个认证文件同目录、结构几乎一致，只有上面几处差异。
//! **登录身份由认证文件决定**（里面有 access/refresh token）；
//! 应用数据目录下的 `storage/skeleton/account-snapshot.json` 是客户端自己记的
//! 「当前 uid」，国际版有、国内版当前版本没有 —— 切换时两个都写更稳。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edition {
    #[default]
    Domestic,
    International,
}

impl Edition {
    pub const ALL: [Edition; 2] = [Edition::Domestic, Edition::International];

    /// 持久化用的稳定标识（写进 `accounts.json`）。
    pub fn key(self) -> &'static str {
        match self {
            Edition::Domestic => "domestic",
            Edition::International => "international",
        }
    }

    /// 界面展示名。
    pub fn label(self) -> &'static str {
        match self {
            Edition::Domestic => "国内版",
            Edition::International => "国际版",
        }
    }

    /// 官方认证文件名。
    pub fn auth_file_name(self) -> &'static str {
        match self {
            Edition::Domestic => "workbuddy-desktop.info",
            Edition::International => "workbuddy-desktop-ai.info",
        }
    }

    /// 应用数据目录名（home 下）。
    pub fn data_dir_name(self) -> &'static str {
        match self {
            Edition::Domestic => ".workbuddy",
            Edition::International => ".workbuddy-ai",
        }
    }

    /// 客户端进程名（不含扩展名）。
    pub fn process_name(self) -> &'static str {
        match self {
            Edition::Domestic => "WorkBuddy",
            Edition::International => "WorkBuddyAI",
        }
    }

    /// API 域名（写进 auth 文件的 `auth.domain`）。
    pub fn api_domain(self) -> &'static str {
        match self {
            Edition::Domestic => "www.codebuddy.cn",
            Edition::International => "www.workbuddy.ai",
        }
    }

    /// 应用数据目录 `~/.workbuddy` 或 `~/.workbuddy-ai`。
    pub fn data_dir(self) -> PathBuf {
        super::config::home_dir().join(self.data_dir_name())
    }

    /// 官方认证文件完整路径。
    pub fn auth_file_path(self) -> PathBuf {
        let home = super::config::home_dir();
        #[cfg(target_os = "macos")]
        let dir = home.join("Library/Application Support/CodeBuddyExtension/Data/Public/auth");
        #[cfg(target_os = "windows")]
        let dir = home.join("AppData/Local/CodeBuddyExtension/Data/Public/auth");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let dir = home.join(".local/share/CodeBuddyExtension/Data/Public/auth");
        dir.join(self.auth_file_name())
    }

    /// 切换前备份认证文件时的文件名前缀。
    pub fn backup_prefix(self) -> &'static str {
        match self {
            Edition::Domestic => "workbuddy-desktop",
            Edition::International => "workbuddy-desktop-ai",
        }
    }

    /// 客户端自己记的「当前账号」快照。
    ///
    /// 参考 Harvey-Will/workbuddy-tools（MIT）的 `core/editions.py::account_snapshot_path`。
    /// ⚠️ 本机实测：**国际版存在、国内版当前版本没有该文件** —— 不存在时跳过即可。
    pub fn account_snapshot_path(self) -> PathBuf {
        self.data_dir()
            .join("storage")
            .join("skeleton")
            .join("account-snapshot.json")
    }

    /// 客户端安装目录的候选父目录（供扫描 exe 用）。
    pub fn install_parent_dirs(self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            out.push(PathBuf::from(local).join("Programs"));
        }
        // 本机实测两个客户端都装在 F:\AdobeAll\ 下，其它盘符也可能。
        for drive in ["C:", "D:", "E:", "F:", "G:"] {
            out.push(PathBuf::from(format!("{drive}/AdobeAll")));
            out.push(PathBuf::from(format!("{drive}/")));
        }
        out
    }
}

/// 宽松解析：兼容 `cn` / `intl` / `国内版` / `international` 等写法。
/// 未知值回落到国内版（保持向后兼容）。
pub fn parse_lenient(raw: &str) -> Edition {
    match raw.trim().to_ascii_lowercase().as_str() {
        "international" | "intl" | "global" | "ai" | "workbuddyai" | "国际版" | "国际"
        | "workbuddy-ai" => Edition::International,
        _ => Edition::Domestic,
    }
}

/// 从账号记录（`accounts.json` 的条目）取版本，缺省视为国内版。
pub fn edition_of(acc: &serde_json::Value) -> Edition {
    acc.get("edition")
        .and_then(|v| v.as_str())
        .map(parse_lenient)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_match_measured_values() {
        assert_eq!(Edition::Domestic.auth_file_name(), "workbuddy-desktop.info");
        assert_eq!(
            Edition::International.auth_file_name(),
            "workbuddy-desktop-ai.info"
        );
    }

    #[test]
    fn data_dirs_and_processes_are_distinct() {
        assert_ne!(Edition::Domestic.data_dir_name(), Edition::International.data_dir_name());
        assert_ne!(Edition::Domestic.process_name(), Edition::International.process_name());
        assert_eq!(Edition::International.api_domain(), "www.workbuddy.ai");
    }

    #[test]
    fn parse_lenient_defaults_to_domestic() {
        assert_eq!(parse_lenient("international"), Edition::International);
        assert_eq!(parse_lenient("国际版"), Edition::International);
        assert_eq!(parse_lenient("intl"), Edition::International);
        assert_eq!(parse_lenient("domestic"), Edition::Domestic);
        assert_eq!(parse_lenient(""), Edition::Domestic);
        assert_eq!(parse_lenient("随便什么"), Edition::Domestic);
    }

    #[test]
    fn edition_of_reads_json_field() {
        assert_eq!(edition_of(&serde_json::json!({"edition": "international"})), Edition::International);
        assert_eq!(edition_of(&serde_json::json!({})), Edition::Domestic);
    }
}
