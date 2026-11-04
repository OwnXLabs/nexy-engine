use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub model_url: String,
    pub model: String,
    pub model_key: Option<String>,
    pub searxng_url: Option<String>,
    pub data_dir: PathBuf,
    pub packs_dir: PathBuf,
    pub slots: usize,
    pub api_key: Option<String>,
    pub caps_file: Option<PathBuf>,
}

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn from_env() -> Self {
        Config {
            host: var("NEXY_ENGINE_HOST").unwrap_or_else(|| "127.0.0.1".into()),
            port: var("NEXY_ENGINE_PORT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(8650),
            model_url: var("NEXY_ENGINE_MODEL_URL")
                .unwrap_or_else(|| "http://127.0.0.1:11434".into())
                .trim_end_matches('/')
                .to_string(),
            model: var("NEXY_ENGINE_MODEL").unwrap_or_else(|| "qwen3.6:35b-a3b".into()),
            model_key: var("NEXY_ENGINE_MODEL_KEY"),
            searxng_url: var("NEXY_ENGINE_SEARXNG_URL")
                .map(|u| u.trim_end_matches('/').to_string()),
            data_dir: var("NEXY_ENGINE_DATA")
                .unwrap_or_else(|| "data".into())
                .into(),
            packs_dir: var("NEXY_ENGINE_PACKS")
                .unwrap_or_else(|| "packs".into())
                .into(),
            slots: var("NEXY_ENGINE_SLOTS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(2)
                .max(1),
            api_key: var("NEXY_ENGINE_KEY"),
            caps_file: var("NEXY_ENGINE_CAPS_FILE").map(PathBuf::from),
        }
    }
}
