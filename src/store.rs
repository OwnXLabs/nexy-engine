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
    pub user: String,
    pub bot: String,
    pub title: String,
    pub source: String,
    pub started_at: i64,
    pub message_count: i64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Job {
    pub id: i64,
    pub user: String,
    pub bot: String,
    pub name: String,
    pub schedule: String,
    pub prompt: String,
    pub enabled: bool,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub last_status: Option<String>,
    /// "active", or "pending" until the owner confirms it
    pub status: String,
    /// outside content that was in context when it was made, if any
    pub origin: Option<String>,
}

pub const PENDING: &str = "pending";

/// Routines one user may keep, pending ones included.
pub const MAX_JOBS_PER_USER: i64 = 50;
/// Routines one bot may hold across all users: user ids are the caller's to choose.
pub const MAX_JOBS_PER_BOT: i64 = 200;

#[derive(Debug)]
pub enum JobError {
    Full(String),
    Db(rusqlite::Error),
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            JobError::Full(m) => f.write_str(m),
            JobError::Db(e) => write!(f, "{e}"),
        }
    }
}

impl From<rusqlite::Error> for JobError {
    fn from(e: rusqlite::Error) -> Self {
        JobError::Db(e)
    }
}

pub const DEFAULT_USER: &str = "owner";
const USER_COL: &str = "TEXT NOT NULL DEFAULT 'owner'";

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn utc_now() -> String {
    utc_at(now())
}

pub fn utc_at(s: i64) -> String {
    let (days, rem) = (s.div_euclid(86_400), s.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

pub fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
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
        for (table, column, ddl) in [
            ("sessions", "user", USER_COL),
            ("memory", "user", USER_COL),
            ("jobs", "user", USER_COL),
            ("jobs", "status", "TEXT NOT NULL DEFAULT 'active'"),
            ("jobs", "origin", "TEXT"),
        ] {
            let has: bool = db
                .prepare(&format!(
                    "SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}'"
                ))?
                .exists([])?;
            if !has {
                db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"))?;
            }
        }
        Ok(Store { db: Mutex::new(db) })
    }

    pub fn ensure_session(
        &self,
        user: &str,
        id: &str,
        bot: &str,
        title: &str,
        source: &str,
    ) -> rusqlite::Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO sessions(id, user, bot, title, source, started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, user, bot, title, source, now()],
        )?;
        Ok(())
    }

    pub fn session_user(&self, id: &str) -> rusqlite::Result<Option<String>> {
        self.db
            .lock()
            .unwrap()
            .query_row(
                "SELECT user FROM sessions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
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

    pub fn sessions(&self, user: &str, limit: usize) -> rusqlite::Result<Vec<SessionMeta>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT s.id, s.user, s.bot, s.title, s.source, s.started_at, (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id)
             FROM sessions s WHERE s.user = ?1 ORDER BY s.started_at DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![user, limit as i64], |r| {
            Ok(SessionMeta {
                id: r.get(0)?,
                user: r.get(1)?,
                bot: r.get(2)?,
                title: r.get(3)?,
                source: r.get(4)?,
                started_at: r.get(5)?,
                message_count: r.get(6)?,
            })
        })?;
        rows.collect()
    }

    pub fn delete_session(&self, user: &str, id: &str) -> rusqlite::Result<usize> {
        let db = self.db.lock().unwrap();
        let n = db.execute(
            "DELETE FROM sessions WHERE id = ?1 AND user = ?2",
            params![id, user],
        )?;
        if n > 0 {
            db.execute("DELETE FROM messages WHERE session_id = ?1", params![id])?;
        }
        Ok(n)
    }

    pub fn remember(&self, user: &str, bot: &str, text: &str) -> rusqlite::Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO memory(user, bot, text, ts) VALUES (?1, ?2, ?3, ?4)",
            params![user, bot, text, now()],
        )?;
        Ok(())
    }

    pub fn recall(
        &self,
        user: &str,
        bot: &str,
        query: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<String>> {
        let db = self.db.lock().unwrap();
        let words: Vec<String> = query
            .split_whitespace()
            .filter(|w| w.len() > 2)
            .map(|w| format!("%{}%", w.to_lowercase()))
            .collect();
        let mut out = Vec::new();
        let mut st = db.prepare(
            "SELECT text FROM memory WHERE user = ?1 AND bot = ?2 ORDER BY id DESC LIMIT 500",
        )?;
        for t in st.query_map(params![user, bot], |r| r.get::<_, String>(0))? {
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
        user: &str,
        bot: &str,
        name: &str,
        schedule: &str,
        prompt: &str,
        next: Option<i64>,
    ) -> Result<i64, JobError> {
        self.insert_job(user, bot, name, schedule, prompt, next, None)
    }

    /// Saved but not scheduled: no next run until the owner confirms it.
    pub fn add_pending_job(
        &self,
        user: &str,
        bot: &str,
        name: &str,
        schedule: &str,
        prompt: &str,
        origin: &str,
    ) -> Result<i64, JobError> {
        self.insert_job(user, bot, name, schedule, prompt, None, Some(origin))
    }

    /// Counts and inserts under one lock, so parallel requests cannot pass the caps together.
    #[allow(clippy::too_many_arguments)]
    fn insert_job(
        &self,
        user: &str,
        bot: &str,
        name: &str,
        schedule: &str,
        prompt: &str,
        next: Option<i64>,
        origin: Option<&str>,
    ) -> Result<i64, JobError> {
        let db = self.db.lock().unwrap();
        let count = |sql: &str, v: &str| db.query_row(sql, params![v], |r| r.get::<_, i64>(0));
        if count("SELECT COUNT(*) FROM jobs WHERE user = ?1", user)? >= MAX_JOBS_PER_USER {
            return Err(JobError::Full(format!(
                "routine limit reached ({MAX_JOBS_PER_USER}, waiting ones included): delete one first"
            )));
        }
        if count("SELECT COUNT(*) FROM jobs WHERE bot = ?1", bot)? >= MAX_JOBS_PER_BOT {
            return Err(JobError::Full(format!(
                "this bot holds its limit of {MAX_JOBS_PER_BOT} routines: delete one first"
            )));
        }
        let status = if origin.is_some() { PENDING } else { "active" };
        db.execute(
            "INSERT INTO jobs(user, bot, name, schedule, prompt, next_run_at, status, origin)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![user, bot, name, schedule, prompt, next, status, origin],
        )?;
        Ok(db.last_insert_rowid())
    }

    pub fn job(&self, user: &str, id: i64) -> rusqlite::Result<Option<Job>> {
        Ok(self.jobs(Some(user))?.into_iter().find(|j| j.id == id))
    }

    /// Owner only, through the API: a pending job becomes active with its first run.
    pub fn confirm_job(&self, user: &str, id: i64, next: i64) -> rusqlite::Result<usize> {
        self.db.lock().unwrap().execute(
            "UPDATE jobs SET status = 'active', next_run_at = ?3 WHERE id = ?1 AND user = ?2 AND status = ?4",
            params![id, user, next, PENDING],
        )
    }

    pub fn discard_job(&self, user: &str, id: i64) -> rusqlite::Result<usize> {
        self.db.lock().unwrap().execute(
            "DELETE FROM jobs WHERE id = ?1 AND user = ?2 AND status = ?3",
            params![id, user, PENDING],
        )
    }

    /// Names of the tools that ran in a session, oldest turns included.
    pub fn tools_used(&self, session: &str) -> rusqlite::Result<Vec<String>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT DISTINCT COALESCE(tool_name, '') FROM messages WHERE session_id = ?1 AND role = 'tool'",
        )?;
        let rows = st.query_map(params![session], |r| r.get(0))?;
        rows.collect()
    }

    pub fn jobs(&self, user: Option<&str>) -> rusqlite::Result<Vec<Job>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT id, user, bot, name, schedule, prompt, enabled, next_run_at, last_run_at, last_status, status, origin FROM jobs
             WHERE ?1 IS NULL OR user = ?1 ORDER BY id",
        )?;
        let rows = st.query_map(params![user], |r| {
            Ok(Job {
                id: r.get(0)?,
                user: r.get(1)?,
                bot: r.get(2)?,
                name: r.get(3)?,
                schedule: r.get(4)?,
                prompt: r.get(5)?,
                enabled: r.get::<_, i64>(6)? != 0,
                next_run_at: r.get(7)?,
                last_run_at: r.get(8)?,
                last_status: r.get(9)?,
                status: r.get(10)?,
                origin: r.get(11)?,
            })
        })?;
        rows.collect()
    }

    pub fn finish_job(&self, id: i64, status: &str, next: Option<i64>) -> rusqlite::Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE jobs SET last_run_at = ?2, last_status = ?3, next_run_at = ?4 WHERE id = ?1 AND status = 'active'",
            params![id, now(), status, next],
        )?;
        Ok(())
    }

    pub fn delete_job(&self, user: &str, id: i64) -> rusqlite::Result<usize> {
        self.db.lock().unwrap().execute(
            "DELETE FROM jobs WHERE id = ?1 AND user = ?2",
            params![id, user],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_761), (2026, 11, 4));
    }

    #[test]
    fn users_cannot_see_each_other() {
        let s = Store::open(Path::new(":memory:")).unwrap();
        s.ensure_session("a", "s1", "nexy", "t", "api").unwrap();
        s.append("s1", "user", "hello", None).unwrap();
        s.remember("a", "nexy", "secret plan").unwrap();
        s.add_job("a", "nexy", "j", "every 1h", "p", None).unwrap();
        assert_eq!(s.session_user("s1").unwrap().as_deref(), Some("a"));
        assert!(s.sessions("b", 50).unwrap().is_empty());
        assert!(s.recall("b", "nexy", "secret", 5).unwrap().is_empty());
        assert_eq!(s.recall("a", "nexy", "secret", 5).unwrap().len(), 1);
        assert!(s.jobs(Some("b")).unwrap().is_empty());
        assert_eq!(s.delete_session("b", "s1").unwrap(), 0);
        assert_eq!(s.delete_job("b", 1).unwrap(), 0);
        assert_eq!(s.messages("s1", 10).unwrap().len(), 1);
        assert_eq!(s.delete_session("a", "s1").unwrap(), 1);
        assert!(s.messages("s1", 10).unwrap().is_empty());
    }

    #[test]
    fn pending_jobs_wait_for_confirm_or_discard() {
        let s = Store::open(Path::new(":memory:")).unwrap();
        let id = s
            .add_pending_job("a", "nexy", "j", "every 1h", "p", "read_page")
            .unwrap();
        let j = s.job("a", id).unwrap().unwrap();
        assert_eq!((j.status.as_str(), j.next_run_at), (PENDING, None));
        assert_eq!(j.origin.as_deref(), Some("read_page"));
        s.finish_job(id, "ok", Some(5)).unwrap();
        assert_eq!(s.job("a", id).unwrap().unwrap().next_run_at, None);
        assert_eq!(s.confirm_job("b", id, 99).unwrap(), 0);
        assert_eq!(s.confirm_job("a", id, 99).unwrap(), 1);
        let j = s.job("a", id).unwrap().unwrap();
        assert_eq!((j.status.as_str(), j.next_run_at), ("active", Some(99)));
        assert_eq!(s.confirm_job("a", id, 5).unwrap(), 0, "confirms once");
        assert_eq!(
            s.discard_job("a", id).unwrap(),
            0,
            "discard is for pending only"
        );
        let p = s
            .add_pending_job("a", "nexy", "k", "every 1h", "p", "x")
            .unwrap();
        assert_eq!(s.discard_job("b", p).unwrap(), 0);
        assert_eq!(s.discard_job("a", p).unwrap(), 1);
        assert!(s.job("a", p).unwrap().is_none());
    }

    #[test]
    fn routines_are_capped_per_user_and_per_bot() {
        let s = Store::open(Path::new(":memory:")).unwrap();
        for i in 0..MAX_JOBS_PER_USER - 1 {
            s.add_job("a", "nexy", &format!("j{i}"), "every 1h", "p", Some(1))
                .unwrap();
        }
        s.add_pending_job("a", "nexy", "last", "every 1h", "p", "read_page")
            .unwrap();
        let full = s.add_job("a", "nexy", "x", "every 1h", "p", Some(1));
        assert!(matches!(full, Err(JobError::Full(_))), "{full:?}");
        let full = s.add_pending_job("a", "other", "x", "every 1h", "p", "o");
        assert!(matches!(full, Err(JobError::Full(_))), "pending count too");
        s.add_job("b", "nexy", "x", "every 1h", "p", Some(1))
            .unwrap();
        for u in 0..MAX_JOBS_PER_BOT - MAX_JOBS_PER_USER - 1 {
            s.add_job(&format!("u{u}"), "nexy", "x", "every 1h", "p", Some(1))
                .unwrap();
        }
        let full = s.add_job("fresh", "nexy", "x", "every 1h", "p", Some(1));
        assert!(
            full.as_ref()
                .is_err_and(|e| e.to_string().contains("this bot")),
            "{full:?}"
        );
        assert!(
            s.add_job("fresh", "coder", "x", "every 1h", "p", Some(1))
                .is_ok()
        );
        assert_eq!(s.delete_job("a", 1).unwrap(), 1);
        assert!(
            s.add_job("a", "coder", "x", "every 1h", "p", Some(1))
                .is_ok()
        );
    }

    #[test]
    fn old_database_gains_status_and_keeps_its_jobs() {
        let path = std::env::temp_dir().join(format!("nexy-old-{}.db", std::process::id()));
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        {
            let old = Connection::open(&path).unwrap();
            old.execute_batch(
                "CREATE TABLE jobs(id INTEGER PRIMARY KEY, bot TEXT NOT NULL, name TEXT NOT NULL, schedule TEXT NOT NULL,
                   prompt TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, next_run_at INTEGER, last_run_at INTEGER, last_status TEXT);
                 INSERT INTO jobs(bot, name, schedule, prompt, next_run_at) VALUES ('nexy', 'brief', 'every 1h', 'p', 42);",
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let j = s.job(DEFAULT_USER, 1).unwrap().unwrap();
        assert_eq!(
            (j.status.as_str(), j.next_run_at, j.origin),
            ("active", Some(42), None)
        );
        drop(s);
        Store::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        for ext in ["-wal", "-shm"] {
            let side = path.with_file_name(format!(
                "{}{ext}",
                path.file_name().unwrap().to_string_lossy()
            ));
            if side.exists() {
                std::fs::remove_file(side).unwrap();
            }
        }
    }
}
