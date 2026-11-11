use crate::App;
use crate::store::{Job, PENDING, now};
use std::sync::Arc;
use std::time::Duration;

pub fn next_run(schedule: &str, from: i64) -> Result<i64, String> {
    let s = schedule.trim().to_lowercase();
    if let Some(rest) = s.strip_prefix("every ") {
        let rest = rest.trim();
        if rest.len() < 2 || !rest.is_ascii() {
            return Err(bad(schedule));
        }
        let (num, unit) = rest.split_at(rest.len() - 1);
        let n: i64 = num.trim().parse().map_err(|_| bad(schedule))?;
        let secs = match unit {
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            _ => return Err(bad(schedule)),
        };
        if n <= 0 {
            return Err(bad(schedule));
        }
        return Ok(from + n * secs);
    }
    let cron = match s.strip_prefix("daily ") {
        Some(t) => {
            let (h, m) = t.trim().split_once(':').ok_or_else(|| bad(schedule))?;
            format!("{m} {h} * * *")
        }
        None => s,
    };
    let c = Cron::parse(&cron).ok_or_else(|| bad(schedule))?;
    c.after(from).ok_or_else(|| bad(schedule))
}

fn bad(s: &str) -> String {
    format!(
        "unsupported schedule \"{s}\": use \"every 30m\", \"every 2h\", \"daily 08:00\" or 5-field cron such as \"0 9 * * 1-5\" (all UTC)"
    )
}

struct Cron {
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
    dom_any: bool,
    dow_any: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn value(s: &str, lo: u32, names: &[&str]) -> Option<u32> {
    names
        .iter()
        .position(|n| *n == s)
        .map(|i| i as u32 + lo)
        .or_else(|| s.parse().ok())
}

fn field(f: &str, lo: u32, hi: u32, names: &[&str]) -> Option<u64> {
    let mut bits = 0u64;
    for part in f.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0)?),
            None => (part, 1),
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a, lo, names)?, value(b, lo, names)?)
        } else {
            let a = value(range, lo, names)?;
            (a, if part.contains('/') { hi } else { a })
        };
        if a < lo || b > hi || a > b {
            return None;
        }
        for v in (a..=b).step_by(step as usize) {
            bits |= 1 << v;
        }
    }
    Some(bits)
}

impl Cron {
    fn parse(s: &str) -> Option<Cron> {
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() != 5 {
            return None;
        }
        let mut dow = field(f[4], 0, 7, &DAYS)?;
        if dow & (1 << 7) != 0 {
            dow |= 1;
        }
        Some(Cron {
            minute: field(f[0], 0, 59, &[])?,
            hour: field(f[1], 0, 23, &[])?,
            dom: field(f[2], 1, 31, &[])?,
            month: field(f[3], 1, 12, &MONTHS)?,
            dow,
            dom_any: f[2] == "*",
            dow_any: f[4] == "*",
        })
    }

    fn day_matches(&self, days: i64) -> bool {
        let (_, m, d) = crate::store::civil(days);
        let wd = (days + 4).rem_euclid(7);
        let on = |bits: u64, v: i64| bits & (1 << v) != 0;
        if !on(self.month, m) {
            return false;
        }
        let (dom, dow) = (on(self.dom, d), on(self.dow, wd));
        match (self.dom_any, self.dow_any) {
            (false, false) => dom || dow,
            (true, false) => dow,
            (false, true) => dom,
            (true, true) => true,
        }
    }

    fn after(&self, from: i64) -> Option<i64> {
        let start = from - from.rem_euclid(60) + 60;
        let first_day = start.div_euclid(86_400);
        for day in first_day..first_day + 366 * 8 {
            if !self.day_matches(day) {
                continue;
            }
            for h in 0..24 {
                if self.hour & (1 << h) == 0 {
                    continue;
                }
                for m in 0..60 {
                    let at = day * 86_400 + h * 3600 + m * 60;
                    if self.minute & (1 << m) != 0 && at >= start {
                        return Some(at);
                    }
                }
            }
        }
        None
    }
}

/// Jobs to run now. A pending job never runs, even with a next run set.
pub fn due(jobs: Vec<Job>, at: i64) -> Vec<Job> {
    jobs.into_iter()
        .filter(|j| j.enabled && j.status != PENDING && j.next_run_at.is_some_and(|t| t <= at))
        .collect()
}

pub async fn run(app: Arc<App>) {
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let due: Vec<_> = match app.store.jobs(None) {
            Ok(j) => due(j, now()),
            Err(e) => {
                eprintln!("scheduler: cannot read jobs: {e}");
                continue;
            }
        };
        for job in due {
            let next = next_run(&job.schedule, now()).ok();
            if let Err(e) = app.store.finish_job(job.id, "running", next) {
                eprintln!("scheduler: job {}: {e}", job.id);
                continue;
            }
            let app = app.clone();
            tokio::task::spawn_blocking(move || {
                let status = match app.bots.get(&job.bot) {
                    None => format!("error: no bot \"{}\"", job.bot),
                    Some(bot) => {
                        let session = format!("cron-{}-{}", job.id, now());
                        let _ = app.store.ensure_session(
                            &job.user,
                            &session,
                            &bot.name,
                            &format!("{}: scheduled", job.name),
                            "cron",
                        );
                        match crate::agent::run(
                            &app,
                            bot,
                            crate::agent::Input {
                                user: &job.user,
                                session: &session,
                                text: &job.prompt,
                                context: None,
                                owner: false,
                                research: None,
                                workspace: None,
                                cancel: None,
                                progress: None,
                                on_text: None,
                            },
                        ) {
                            Ok(_) => "ok".into(),
                            Err(e) => format!("error: {e}"),
                        }
                    }
                };
                if let Err(e) = app.store.finish_job(job.id, &status, next) {
                    eprintln!("scheduler: job {}: {e}", job.id);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{due, next_run};
    use crate::store::{PENDING, Store};

    #[test]
    fn pending_jobs_never_run() {
        let s = Store::open(std::path::Path::new(":memory:")).unwrap();
        let live = s
            .add_job("a", "nexy", "live", "every 1h", "p", Some(10))
            .unwrap();
        let wait = s
            .add_pending_job("a", "nexy", "wait", "every 1h", "p", "read_page")
            .unwrap();
        let mut jobs = s.jobs(None).unwrap();
        jobs[1].next_run_at = Some(10);
        assert_eq!(jobs[1].status, PENDING);
        let ids: Vec<i64> = due(jobs, 100).iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![live], "pending job {wait} was due");
        s.confirm_job("a", wait, 50).unwrap();
        assert_eq!(due(s.jobs(None).unwrap(), 100).len(), 2);
    }

    #[test]
    fn schedules() {
        let t = 1_700_000_000;
        assert_eq!(next_run("every 30m", t).unwrap(), t + 1800);
        assert_eq!(next_run("every 2h", t).unwrap(), t + 7200);
        let d = next_run("daily 08:00", t).unwrap();
        assert_eq!(d.rem_euclid(86_400), 8 * 3600);
        assert!(d > t && d - t <= 86_400);
        assert_eq!(next_run("0 8 * * *", t).unwrap(), d);
        assert!(next_run("whenever", t).is_err());
        assert!(next_run("daily 25:00", t).is_err());
    }

    const FRI_2026_11_06_1000: i64 = 1_793_959_200;

    fn at(t: i64) -> String {
        let (y, m, d) = crate::store::civil(t.div_euclid(86_400));
        let r = t.rem_euclid(86_400);
        format!("{y}-{m:02}-{d:02} {:02}:{:02}", r / 3600, r % 3600 / 60)
    }

    #[test]
    fn standard_cron() {
        let t = FRI_2026_11_06_1000;
        assert_eq!(at(t), "2026-11-06 10:00");
        let n = |s: &str| at(next_run(s, t).unwrap());
        assert_eq!(n("0 9 * * 1-5"), "2026-11-09 09:00");
        assert_eq!(n("30 10 * * mon-fri"), "2026-11-06 10:30");
        assert_eq!(n("0 8 * * 0,6"), "2026-11-07 08:00");
        assert_eq!(n("0 8 * * 7"), "2026-11-08 08:00");
        assert_eq!(n("*/15 * * * *"), "2026-11-06 10:15");
        assert_eq!(n("5-59/20 * * * *"), "2026-11-06 10:05");
        assert_eq!(n("0 0 1 * *"), "2026-12-01 00:00");
        assert_eq!(n("0 12 25 dec *"), "2026-12-25 12:00");
        assert_eq!(n("0 6 29 2 *"), "2028-02-29 06:00");
        assert_eq!(n("0 9 13 * 5"), "2026-11-13 09:00");
        assert_eq!(n("0 9,17 * * *"), "2026-11-06 17:00");
        for bad in [
            "0 9 * *",
            "60 * * * *",
            "0 24 * * *",
            "0 9 * * 8",
            "*/0 * * * *",
            "5-1 * * * *",
            "0 9 31 2 *",
            "x * * * *",
        ] {
            assert!(next_run(bad, t).is_err(), "{bad} was accepted");
        }
    }
}
