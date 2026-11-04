use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    db: Mutex<Connection>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Msg {
    pub role: String,
    pub content: String,
    pub tool_name: Option<String>,
    pub timestamp: i64,
}

#[derive(Serialize, Clone, Debug)]
pub struct SessionMeta {
    pub id: String,
    pub bot: String,
    pub title: String,
    pub source: String,
    pub started_at: i64,
    pub message_count: i64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Job {
    pub id: i64,
    pub bot: String,
    pub name: String,
    pub schedule: String,
    pub prompt: String,
    pub enabled: bool,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub last_status: Option<String>,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-2000;
             CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, bot TEXT NOT NULL, title TEXT NOT NULL,
               source TEXT NOT NULL, started_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS messages(session_id TEXT NOT NULL, seq INTEGER NOT NULL, role TEXT NOT NULL,
               content TEXT NOT NULL, tool_name TEXT, ts INTEGER NOT NULL, PRIMARY KEY(session_id, seq)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS memory(id INTEGER PRIMARY KEY, bot TEXT NOT NULL, text TEXT NOT NULL, ts INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS jobs(id INTEGER PRIMARY KEY, bot TEXT NOT NULL, name TEXT NOT NULL, schedule TEXT NOT NULL,
               prompt TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, next_run_at INTEGER, last_run_at INTEGER, last_status TEXT);",
        )?;
        Ok(Store { db: Mutex::new(db) })
    }

    pub fn ensure_session(
        &self,
        id: &str,
        bot: &str,
        title: &str,
        source: &str,
    ) -> rusqlite::Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO sessions(id, bot, title, source, started_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, bot, title, source, now()],
        )?;
        Ok(())
    }

    pub fn append(
        &self,
        session: &str,
        role: &str,
        content: &str,
        tool_name: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO messages(session_id, seq, role, content, tool_name, ts)
             VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_id = ?1), ?2, ?3, ?4, ?5)",
            params![session, role, content, tool_name, now()],
        )?;
        Ok(())
    }

    pub fn messages(&self, session: &str, limit: usize) -> rusqlite::Result<Vec<Msg>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT role, content, tool_name, ts FROM
               (SELECT * FROM messages WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2) ORDER BY seq",
        )?;
        let rows = st.query_map(params![session, limit as i64], |r| {
            Ok(Msg {
                role: r.get(0)?,
                content: r.get(1)?,
                tool_name: r.get(2)?,
                timestamp: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn sessions(&self, limit: usize) -> rusqlite::Result<Vec<SessionMeta>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT s.id, s.bot, s.title, s.source, s.started_at, (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id)
             FROM sessions s ORDER BY s.started_at DESC LIMIT ?1",
        )?;
        let rows = st.query_map(params![limit as i64], |r| {
            Ok(SessionMeta {
                id: r.get(0)?,
                bot: r.get(1)?,
                title: r.get(2)?,
                source: r.get(3)?,
                started_at: r.get(4)?,
                message_count: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn session_exists(&self, id: &str) -> rusqlite::Result<bool> {
        let db = self.db.lock().unwrap();
        db.query_row("SELECT 1 FROM sessions WHERE id = ?1", params![id], |_| {
            Ok(())
        })
        .optional()
        .map(|o| o.is_some())
    }

    pub fn delete_session(&self, id: &str) -> rusqlite::Result<usize> {
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM messages WHERE session_id = ?1", params![id])?;
        db.execute("DELETE FROM sessions WHERE id = ?1", params![id])
    }

    pub fn remember(&self, bot: &str, text: &str) -> rusqlite::Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO memory(bot, text, ts) VALUES (?1, ?2, ?3)",
            params![bot, text, now()],
        )?;
        Ok(())
    }

    pub fn recall(&self, bot: &str, query: &str, limit: usize) -> rusqlite::Result<Vec<String>> {
        let db = self.db.lock().unwrap();
        let words: Vec<String> = query
            .split_whitespace()
            .filter(|w| w.len() > 2)
            .map(|w| format!("%{}%", w.to_lowercase()))
            .collect();
        let mut out = Vec::new();
        let mut st =
            db.prepare("SELECT text FROM memory WHERE bot = ?1 ORDER BY id DESC LIMIT 500")?;
        for t in st.query_map(params![bot], |r| r.get::<_, String>(0))? {
            let t = t?;
            let low = t.to_lowercase();
            if words.is_empty() || words.iter().any(|w| low.contains(w.trim_matches('%'))) {
                out.push(t);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    pub fn add_job(
        &self,
        bot: &str,
        name: &str,
        schedule: &str,
        prompt: &str,
        next: Option<i64>,
    ) -> rusqlite::Result<i64> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO jobs(bot, name, schedule, prompt, next_run_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![bot, name, schedule, prompt, next],
        )?;
        Ok(db.last_insert_rowid())
    }

    pub fn jobs(&self) -> rusqlite::Result<Vec<Job>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT id, bot, name, schedule, prompt, enabled, next_run_at, last_run_at, last_status FROM jobs ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Job {
                id: r.get(0)?,
                bot: r.get(1)?,
                name: r.get(2)?,
                schedule: r.get(3)?,
                prompt: r.get(4)?,
                enabled: r.get::<_, i64>(5)? != 0,
                next_run_at: r.get(6)?,
                last_run_at: r.get(7)?,
                last_status: r.get(8)?,
            })
        })?;
        rows.collect()
    }

    pub fn finish_job(&self, id: i64, status: &str, next: Option<i64>) -> rusqlite::Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE jobs SET last_run_at = ?2, last_status = ?3, next_run_at = ?4 WHERE id = ?1",
            params![id, now(), status, next],
        )?;
        Ok(())
    }

    pub fn delete_job(&self, id: i64) -> rusqlite::Result<usize> {
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM jobs WHERE id = ?1", params![id])
    }
}
