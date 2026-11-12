use crate::App;
use crate::store::{Job, PENDING, now};
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use std::sync::Arc;
use std::time::Duration;

/// The owner's zone by IANA name, such as "Asia/Manila". An empty name is UTC; an unknown one is refused.
pub fn zone(name: &str) -> Result<TimeZone, String> {
    let n = name.trim();
    if n.is_empty() || n.eq_ignore_ascii_case("utc") {
        return Ok(TimeZone::UTC);
    }
    TimeZone::get(n).map_err(|e| format!("unknown time zone \"{n}\": {e}"))
}

/// The zone's name as stored and shown: "UTC" for UTC.
pub fn zone_name(tz: &TimeZone) -> &str {
    tz.iana_name().unwrap_or("UTC")
}

/// A moment in the owner's zone, e.g. "Fri 2026-11-13 08:00 Asia/Manila".
pub fn local_at(s: i64, tz: &TimeZone) -> String {
    match Timestamp::from_second(s) {
        Ok(t) => {
            let d = tz.to_datetime(t);
            let wd = DAYS[d.weekday().to_sunday_zero_offset() as usize];
            format!(
                "{}{} {:04}-{:02}-{:02} {:02}:{:02} {}",
                wd[..1].to_uppercase(),
                &wd[1..],
                d.year(),
                d.month(),
                d.day(),
                d.hour(),
                d.minute(),
                zone_name(tz)
            )
        }
        Err(e) => format!("{} ({e})", crate::store::utc_at(s)),
    }
}

/// "Fri 2026-11-13 (the user's time zone, Asia/Manila)", for the system prompt.
pub fn today(tz: &TimeZone) -> String {
    let now = local_at(now(), tz);
    format!("{} (the user's time zone, {})", &now[..14], zone_name(tz))
}

/// "Fri 2026-11-13 08:00 Asia/Manila (the user's local time)", for each turn.
pub fn now_line(tz: &TimeZone) -> String {
    format!("{} (the user's local time)", local_at(now(), tz))
}

/// When a schedule next runs after `from`. Clock times ("daily 08:00", cron) are read in `tz`.
pub fn next_run(schedule: &str, tz: &TimeZone, from: i64) -> Result<i64, String> {
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
    c.after(from, tz).ok_or_else(|| bad(schedule))
}

/// New routines run no more often than this.
pub const MIN_GAP_MINS: i64 = 5;

/// A schedule for a new routine: valid, and no more often than every MIN_GAP_MINS minutes.
pub fn new_schedule(schedule: &str, tz: &TimeZone, from: i64) -> Result<i64, String> {
    let next = next_run(schedule, tz, from)?;
    let s = schedule.trim().to_lowercase();
    let gap = if s.starts_with("every ") {
        (next - from) / 60
    } else if s.starts_with("daily ") {
        24 * 60
    } else {
        Cron::parse(&s).map_or(0, |c| c.min_gap())
    };
    if gap < MIN_GAP_MINS {
        return Err(format!(
            "\"{schedule}\" runs too often: routines run at most every {MIN_GAP_MINS} minutes"
        ));
    }
    Ok(next)
}

fn bad(s: &str) -> String {
    format!(
        "unsupported schedule \"{s}\": use \"every 30m\", \"every 2h\", \"daily 08:00\" or 5-field cron such as \"0 9 * * 1-5\" (in the owner's time zone)"
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

    /// Fewest minutes between two runs, counting across the hour.
    fn min_gap(&self) -> i64 {
        let mins: Vec<i64> = (0..60).filter(|m| self.minute & (1 << m) != 0).collect();
        match (mins.first(), mins.last()) {
            (Some(a), Some(z)) if mins.len() > 1 => mins
                .windows(2)
                .map(|w| w[1] - w[0])
                .chain([60 - z + a])
                .min()
                .unwrap_or(60),
            _ => 60,
        }
    }

    fn day_matches(&self, day: Date) -> bool {
        let on = |bits: u64, v: i64| bits & (1 << v) != 0;
        if !on(self.month, i64::from(day.month())) {
            return false;
        }
        let (dom, dow) = (
            on(self.dom, i64::from(day.day())),
            on(self.dow, i64::from(day.weekday().to_sunday_zero_offset())),
        );
        match (self.dom_any, self.dow_any) {
            (false, false) => dom || dow,
            (true, false) => dow,
            (false, true) => dom,
            (true, true) => true,
        }
    }

    /// The first matching minute after `from`, on the wall clock in `tz`.
    /// A time skipped by a clock change runs just after it; a repeated one runs once, the first time.
    fn after(&self, from: i64, tz: &TimeZone) -> Option<i64> {
        let start = from - from.rem_euclid(60) + 60;
        let mut day = tz.to_datetime(Timestamp::from_second(start).ok()?).date();
        for _ in 0..366 * 8 {
            if self.day_matches(day) {
                // the earliest of the day: in a skipped hour, wall-clock order is not time order
                let mut best: Option<i64> = None;
                for h in (0..24).filter(|h| self.hour & (1 << h) != 0) {
                    for m in (0..60).filter(|m| self.minute & (1 << m) != 0) {
                        let at = tz
                            .to_ambiguous_timestamp(day.at(h, m, 0, 0))
                            .compatible()
                            .ok()?
                            .as_second();
                        if at >= start && best.is_none_or(|b| at < b) {
                            best = Some(at);
                        }
                    }
                }
                if best.is_some() {
                    return best;
                }
            }
            day = day.tomorrow().ok()?;
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
            // a zone the bundled database no longer knows stops the job and says why
            let tz = match zone(&job.tz) {
                Ok(z) => z,
                Err(e) => {
                    eprintln!("scheduler: job {}: {e}", job.id);
                    if let Err(e) = app.store.finish_job(job.id, &format!("error: {e}"), None) {
                        eprintln!("scheduler: job {}: {e}", job.id);
                    }
                    continue;
                }
            };
            let next = next_run(&job.schedule, &tz, now()).ok();
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
                                outside: false,
                                owner: false,
                                research: None,
                                tz: tz.clone(),
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
    use super::{due, new_schedule, next_run, zone};
    use crate::store::{PENDING, Store};
    use jiff::tz::TimeZone;
    const UTC: TimeZone = TimeZone::UTC;

    #[test]
    fn pending_jobs_never_run() {
        let s = Store::open(std::path::Path::new(":memory:")).unwrap();
        let live = s
            .add_job("a", "nexy", "live", "every 1h", "UTC", "p", Some(10))
            .unwrap();
        let wait = s
            .add_pending_job("a", "nexy", "wait", "every 1h", "UTC", "p", "read_page")
            .unwrap();
        let mut jobs = s.jobs(None).unwrap();
        jobs[1].next_run_at = Some(10);
        assert_eq!(jobs[1].status, PENDING);
        let ids: Vec<i64> = due(jobs, 100).iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![live], "pending job {wait} was due");
        s.confirm_job("a", wait, 50, "UTC").unwrap();
        assert_eq!(due(s.jobs(None).unwrap(), 100).len(), 2);
    }

    #[test]
    fn schedules() {
        let t = 1_700_000_000;
        assert_eq!(next_run("every 30m", &UTC, t).unwrap(), t + 1800);
        assert_eq!(next_run("every 2h", &UTC, t).unwrap(), t + 7200);
        let d = next_run("daily 08:00", &UTC, t).unwrap();
        assert_eq!(d.rem_euclid(86_400), 8 * 3600);
        assert!(d > t && d - t <= 86_400);
        assert_eq!(next_run("0 8 * * *", &UTC, t).unwrap(), d);
        assert!(next_run("whenever", &UTC, t).is_err());
        assert!(next_run("daily 25:00", &UTC, t).is_err());
    }

    #[test]
    fn new_routines_run_at_most_every_five_minutes() {
        let t = 1_700_000_000;
        for ok in [
            "every 5m",
            "every 1h",
            "daily 08:00",
            "*/5 * * * *",
            "0 9 * * 1-5",
            "0,30 * * * *",
        ] {
            assert!(new_schedule(ok, &UTC, t).is_ok(), "{ok} was refused");
        }
        for fast in [
            "every 1m",
            "every 4m",
            "* * * * *",
            "*/2 * * * *",
            "0,3 * * * *",
            "0,58 * * * *",
            "* 9 1 1 *",
        ] {
            let e = new_schedule(fast, &UTC, t).unwrap_err();
            assert!(e.contains("too often"), "{fast}: {e}");
        }
        assert!(
            new_schedule("whenever", &UTC, t)
                .unwrap_err()
                .contains("unsupported")
        );
        assert_eq!(
            next_run("every 1m", &UTC, t).unwrap(),
            t + 60,
            "existing routines still run"
        );
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
        let n = |s: &str| at(next_run(s, &UTC, t).unwrap());
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
            assert!(next_run(bad, &UTC, t).is_err(), "{bad} was accepted");
        }
    }

    fn utc(s: &str) -> i64 {
        s.parse::<jiff::Timestamp>().unwrap().as_second()
    }

    fn runs(schedule: &str, tz: &str, from: &str, n: usize) -> Vec<String> {
        let tz = zone(tz).unwrap();
        let mut t = utc(from);
        (0..n)
            .map(|_| {
                t = next_run(schedule, &tz, t).unwrap();
                at(t)
            })
            .collect()
    }

    #[test]
    fn clock_times_are_the_owners() {
        // Manila is UTC+8 all year: 8 am there is midnight UTC
        assert_eq!(
            runs("daily 08:00", "Asia/Manila", "2026-11-06T10:00:00Z", 2),
            ["2026-11-07 00:00", "2026-11-08 00:00"]
        );
        assert_eq!(
            runs("0 9 * * 1-5", "Asia/Manila", "2026-11-06T10:00:00Z", 1),
            ["2026-11-09 01:00"],
            "Monday 9 am in Manila"
        );
        // a weekday is the owner's weekday: Monday 7 am in Manila is still Sunday in UTC
        assert_eq!(
            runs("0 7 * * mon", "Asia/Manila", "2026-11-06T10:00:00Z", 1),
            ["2026-11-08 23:00"]
        );
        assert_eq!(
            runs("daily 08:00", "UTC", "2026-11-06T10:00:00Z", 1),
            ["2026-11-07 08:00"]
        );
        assert_eq!(
            runs("every 2h", "Asia/Manila", "2026-11-06T10:00:00Z", 1),
            ["2026-11-06 12:00"],
            "intervals do not depend on the zone"
        );
        let e = zone("Mars/Olympus").unwrap_err();
        assert!(e.contains("unknown time zone"), "{e}");
        assert_eq!(zone("").unwrap(), UTC);
    }

    #[test]
    fn clock_changes_keep_the_wall_clock() {
        // London: GMT to BST on Sun 28 Mar 2027 at 01:00 UTC, back on Sun 31 Oct 2027
        assert_eq!(
            runs("daily 08:00", "Europe/London", "2027-03-26T12:00:00Z", 3),
            ["2027-03-27 08:00", "2027-03-28 07:00", "2027-03-29 07:00"]
        );
        assert_eq!(
            runs("daily 08:00", "Europe/London", "2027-10-29T12:00:00Z", 3),
            ["2027-10-30 07:00", "2027-10-31 08:00", "2027-11-01 08:00"]
        );
        // 01:30 does not exist on the spring day: it runs once, just after the jump
        assert_eq!(
            runs("30 1 * * *", "Europe/London", "2027-03-27T12:00:00Z", 2),
            ["2027-03-28 01:30", "2027-03-29 00:30"]
        );
        // 01:30 happens twice on the autumn day: it runs once, the first time
        assert_eq!(
            runs("30 1 * * *", "Europe/London", "2027-10-30T12:00:00Z", 2),
            ["2027-10-31 00:30", "2027-11-01 01:30"]
        );
        // hourly through the jump: no hour twice, none lost but the one that never was
        assert_eq!(
            runs("0 * * * *", "Europe/London", "2027-03-28T00:30:00Z", 3),
            ["2027-03-28 01:00", "2027-03-28 02:00", "2027-03-28 03:00"]
        );
    }

    #[test]
    fn a_job_from_before_zones_runs_in_utc() {
        let path = std::env::temp_dir().join(format!("nexy-tz-{}.db", std::process::id()));
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        {
            let old = rusqlite::Connection::open(&path).unwrap();
            old.execute_batch(
                "CREATE TABLE jobs(id INTEGER PRIMARY KEY, bot TEXT NOT NULL, name TEXT NOT NULL, schedule TEXT NOT NULL,
                   prompt TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, next_run_at INTEGER, last_run_at INTEGER, last_status TEXT);
                 INSERT INTO jobs(bot, name, schedule, prompt) VALUES ('nexy', 'brief', 'daily 08:00', 'p');",
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let j = &s.jobs(None).unwrap()[0];
        let next = next_run(
            &j.schedule,
            &zone(&j.tz).unwrap(),
            utc("2026-11-06T10:00:00Z"),
        )
        .unwrap();
        assert_eq!(at(next), "2026-11-07 08:00");
        drop(s);
        for ext in ["", "-wal", "-shm"] {
            let f = std::path::PathBuf::from(format!("{}{ext}", path.display()));
            if f.exists() {
                std::fs::remove_file(f).unwrap();
            }
        }
    }
}
