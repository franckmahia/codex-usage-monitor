use std::path::{Path, PathBuf};

/// Un fichier source de logs Codex.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: PathBuf,
    pub archived: bool,
    pub size: u64,
    pub mtime_unix: Option<i64>,
}

/// Codex home resolu : argument > $CODEX_HOME > ~/.codex.
pub fn codex_home(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(home) = std::env::var("CODEX_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Path::new(&home).join(".codex");
    }
    PathBuf::from(".codex")
}

fn collect(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("rollout-") {
                out.push(p);
            }
        }
    }
}

/// Decouvre sessions/**/*.jsonl et archived_sessions/**/*.jsonl, tries
/// par chemin pour un scan deterministe.
pub fn discover(codex_home: &Path) -> Vec<SourceFile> {
    let mut files: Vec<SourceFile> = Vec::new();
    let mut push_dir = |dir: PathBuf, archived: bool| {
        let mut paths = Vec::new();
        collect(&dir, &mut paths);
        paths.sort();
        for p in paths {
            let (size, mtime_unix) = match std::fs::metadata(&p) {
                Ok(m) => (
                    m.len(),
                    m.modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64),
                ),
                Err(_) => (0, None),
            };
            files.push(SourceFile {
                path: p,
                archived,
                size,
                mtime_unix,
            });
        }
    };
    push_dir(codex_home.join("sessions"), false);
    push_dir(codex_home.join("archived_sessions"), true);
    files
}
