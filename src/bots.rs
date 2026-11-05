use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize, Clone, Debug)]
pub struct Bot {
    pub name: String,
    #[serde(default)]
    pub persona: String,
    #[serde(default)]
    pub persona_file: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default = "default_steps")]
    pub max_steps: usize,
    #[serde(default = "default_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_secs")]
    pub max_secs: u64,
    #[serde(default)]
    pub search_first: bool,
}

fn default_secs() -> u64 {
    600
}

fn default_steps() -> usize {
    6
}
fn default_tokens() -> u32 {
    1024
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct IdleRec {
    pub id: u32,
    pub flags: u32,
    pub wake_at: u64,
    pub cursor: u64,
}

pub fn load(dir: &Path) -> Result<HashMap<String, Bot>, String> {
    let mut bots = HashMap::new();
    let rd = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read packs at {}: {e}", dir.display()))?;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("toml")
            || p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let mut b: Bot = toml::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))?;
        if let Some(f) = &b.persona_file {
            b.persona = std::fs::read_to_string(dir.join(f))
                .map_err(|e| format!("{}: {e}", dir.join(f).display()))?;
        }
        for t in &b.tools {
            if crate::tools::find(t).is_none() {
                return Err(format!("{}: unknown tool \"{t}\"", p.display()));
            }
        }
        if b.tools.len() > 8 {
            return Err(format!("{}: at most 8 tools per bot", p.display()));
        }
        bots.insert(b.name.clone(), b);
    }
    if bots.is_empty() {
        return Err(format!("no bots in {}", dir.display()));
    }
    Ok(bots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_record_fits_the_budget() {
        assert!(std::mem::size_of::<IdleRec>() <= 256);
        assert_eq!(std::mem::size_of::<IdleRec>(), 24);
    }
}
