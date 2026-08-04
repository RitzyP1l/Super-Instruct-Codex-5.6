use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const API_KEY_ENV: &str = "SUPER_INSTRUCT_API_KEY";

#[derive(Clone, Serialize)]
pub struct IsolatedProfile {
    pub name: String,
    pub root: PathBuf,
    pub codex_home: PathBuf,
    pub app_data: PathBuf,
}

#[derive(Serialize)]
pub struct LaunchResult {
    pub pid: u32,
    pub profile: String,
    pub codex_home: String,
    pub app_data: String,
    pub proxy_url: String,
}

#[derive(Clone, Serialize)]
pub struct ModelListResult {
    pub base_url: String,
    pub models: Vec<String>,
}

/// 兼容常见 OpenAI 中转站：优先尝试 /v1/models，再尝试原路径 /models。
pub fn model_endpoint_candidates(input: &str) -> Vec<(String, String)> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        return vec![(trimmed.to_string(), format!("{trimmed}/models"))];
    }
    vec![
        (format!("{trimmed}/v1"), format!("{trimmed}/v1/models")),
        (trimmed.to_string(), format!("{trimmed}/models")),
    ]
}

pub fn extract_model_ids(value: &serde_json::Value) -> Vec<String> {
    let list = value
        .get("data")
        .and_then(|v| v.as_array())
        .or_else(|| value.get("models").and_then(|v| v.as_array()))
        .or_else(|| value.as_array());
    let mut models = list
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.as_str().map(str::to_string).or_else(|| {
                item.get("id")
                    .or_else(|| item.get("model"))
                    .or_else(|| item.get("name"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
        })
        .filter(|id| !id.trim().is_empty())
        .collect::<Vec<_>>();
    models.sort_by_key(|id| id.to_ascii_lowercase());
    models.dedup();
    models
}

pub fn profiles_root() -> Result<PathBuf, String> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA is not available".to_string())?;
    Ok(local.join("Super-Instruct").join("profiles"))
}

pub fn validate_profile_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 48 {
        return Err("配置名称长度应为 1-48 个字符".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("配置名称仅支持字母、数字、短横线和下划线".into());
    }
    Ok(name.to_string())
}

pub fn profile(name: &str) -> Result<IsolatedProfile, String> {
    let name = validate_profile_name(name)?;
    let root = profiles_root()?.join(&name);
    Ok(IsolatedProfile {
        name,
        codex_home: root.join("codex-home"),
        app_data: root.join("app-data"),
        root,
    })
}

pub fn prepare_profile(
    name: &str,
    model: &str,
    proxy_port: u16,
    upstream_url: &str,
    bridge_md: &str,
    skills_dir: Option<&Path>,
) -> Result<IsolatedProfile, String> {
    let profile = profile(name)?;
    let model = model.trim();
    if model.is_empty() {
        return Err("模型名称不能为空".into());
    }

    fs::create_dir_all(&profile.codex_home)
        .map_err(|e| format!("创建独立 CODEX_HOME 失败: {e}"))?;
    fs::create_dir_all(&profile.app_data).map_err(|e| format!("创建独立应用数据目录失败: {e}"))?;

    let config = render_config(model, proxy_port)?;
    fs::write(profile.codex_home.join("config.toml"), config)
        .map_err(|e| format!("写入独立 config.toml 失败: {e}"))?;
    fs::write(profile.codex_home.join("bridge.md"), bridge_md)
        .map_err(|e| format!("写入 bridge.md 失败: {e}"))?;
    fs::write(profile.root.join("upstream_url.txt"), upstream_url.trim())
        .map_err(|e| format!("保存上游地址失败: {e}"))?;

    if let Some(src) = skills_dir {
        let dst = profile.codex_home.join("skills");
        if dst.exists() {
            fs::remove_dir_all(&dst).map_err(|e| format!("清理旧 Skills 失败: {e}"))?;
        }
        copy_dir_recursive(src, &dst).map_err(|e| format!("复制 Skills 失败: {e}"))?;
    }

    Ok(profile)
}

fn render_config(model: &str, proxy_port: u16) -> Result<String, String> {
    let model = toml_string(model)?;
    Ok(format!(
        "model = {model}\nmodel_provider = \"super_instruct\"\nmodel_instructions_file = \"./bridge.md\"\ncli_auth_credentials_store = \"file\"\n\n[model_providers.super_instruct]\nname = \"Super Instruct Isolated\"\nbase_url = \"http://127.0.0.1:{proxy_port}/v1\"\nenv_key = \"{API_KEY_ENV}\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n\n[shell_environment_policy]\nexclude = [\"CODEX_HOME\", \"{API_KEY_ENV}\"]\n"
    ))
}

fn toml_string(value: &str) -> Result<String, String> {
    Ok(toml::Value::String(value.to_string()).to_string())
}

pub fn find_codex_executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CODEX_APP_EXE").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
    }

    let program_files = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .ok_or_else(|| "ProgramFiles is not available".to_string())?;
    let windows_apps = program_files.join("WindowsApps");
    let entries = fs::read_dir(&windows_apps).map_err(|e| format!("读取 WindowsApps 失败: {e}"))?;
    let mut candidates = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("OpenAI.Codex_") || !name.contains("_x64__") {
                return None;
            }
            let exe = entry.path().join("app").join("ChatGPT.exe");
            exe.is_file().then_some(exe)
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates
        .pop()
        .ok_or_else(|| "未找到 Codex Windows 客户端 ChatGPT.exe".to_string())
}

pub fn launch_profile(
    profile: &IsolatedProfile,
    api_key: &str,
    proxy_port: u16,
) -> Result<LaunchResult, String> {
    if api_key.trim().is_empty() {
        return Err("API Key 不能为空".into());
    }
    let exe = find_codex_executable()?;
    let child = Command::new(&exe)
        .env("CODEX_HOME", &profile.codex_home)
        .env("CODEX_PROFILE_NAME", &profile.name)
        .env(API_KEY_ENV, api_key.trim())
        .arg(format!("--user-data-dir={}", profile.app_data.display()))
        .spawn()
        .map_err(|e| format!("启动 Codex 独立窗口失败: {e}"))?;

    Ok(LaunchResult {
        pid: child.id(),
        profile: profile.name.clone(),
        codex_home: profile.codex_home.display().to_string(),
        app_data: profile.app_data.display().to_string(),
        proxy_url: format!("http://127.0.0.1:{proxy_port}/v1"),
    })
}

pub fn remove_profile(name: &str) -> Result<String, String> {
    let profile = profile(name)?;
    if profile.root.exists() {
        fs::remove_dir_all(&profile.root).map_err(|e| format!("删除独立配置失败: {e}"))?;
        Ok(format!("已删除独立配置 {}", profile.name))
    } else {
        Ok(format!("独立配置 {} 不存在", profile.name))
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let dest = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_name_rejects_path_traversal() {
        assert!(validate_profile_name("../default").is_err());
        assert!(validate_profile_name("account-b").is_ok());
    }

    #[test]
    fn config_uses_env_key_and_contains_no_secret() {
        let config = render_config("gpt-5.2-codex", 12345).unwrap();
        assert!(config.contains("env_key = \"SUPER_INSTRUCT_API_KEY\""));
        assert!(config.contains("127.0.0.1:12345/v1"));
        assert!(!config.contains("sk-test-secret"));
        toml::from_str::<toml::Value>(&config).unwrap();
    }

    #[test]
    fn extracts_and_sorts_openai_model_list() {
        let value = serde_json::json!({"data": [
            {"id": "gpt-5.2-codex"},
            {"id": "gpt-4.1"},
            {"id": "gpt-5.2-codex"}
        ]});
        assert_eq!(extract_model_ids(&value), vec!["gpt-4.1", "gpt-5.2-codex"]);
    }

    #[test]
    fn builds_v1_model_endpoint_candidates() {
        let candidates = model_endpoint_candidates("https://relay.example/api");
        assert_eq!(candidates[0].0, "https://relay.example/api/v1");
        assert_eq!(candidates[0].1, "https://relay.example/api/v1/models");
        assert_eq!(model_endpoint_candidates("https://relay.example/v1").len(), 1);
    }
}
