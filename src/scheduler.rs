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

/// A schedule as it is stored: "every 30m", "daily 08:15" or 5-field cron. Also takes
/// "weekdays 07:30", "weekends 9am", "monday 18:45", and cron with the time in the hour field
/// ("0 7:30 * * 1-5" is 07:30, not 07:00). Anything else is refused, never read as another time.
pub fn canonical(schedule: &str) -> Result<String, String> {
    let s = schedule
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if let Some(rest) = s.strip_prefix("every ")
        && rest.len() >= 2
        && rest.is_ascii()
        && rest[..rest.len() - 1]
            .trim()
            .parse::<i64>()
            .is_ok_and(|n| n > 0)
        && matches!(&rest[rest.len() - 1..], "m" | "h" | "d")
    {
        return Ok(format!("every {}", rest.replace(' ', "")));
    }
    if let Some(c) = friendly(&s) {
        return c.ok_or_else(|| bad(schedule));
    }
    let f: Vec<&str> = s.split(' ').collect();
    let cron = match f.as_slice() {
        // the time written into the hour field: "0 7:30 * * 1-5"
        ["0" | "00", hm, rest @ ..] if rest.len() == 3 && hm.contains(':') => {
            let (h, m) = clock(hm).ok_or_else(|| bad(schedule))?;
            format!("{m} {h} {}", rest.join(" "))
        }
        // or in place of minute and hour: "7:30 * * 1-5"
        [hm, rest @ ..] if rest.len() == 3 && hm.contains(':') => {
            let (h, m) = clock(hm).ok_or_else(|| bad(schedule))?;
            format!("{m} {h} {}", rest.join(" "))
        }
        _ => s.clone(),
    };
    Cron::parse(&cron)
        .map(|_| cron)
        .ok_or_else(|| bad(schedule))
}

/// "daily 08:15", "weekdays 7:30 am", "every monday at 18:45": `None` when it is not of this form,
/// `Some(None)` when it is but the time is not a time.
fn friendly(s: &str) -> Option<Option<String>> {
    let s = s.strip_prefix("every ").unwrap_or(s);
    let (days, at) = s.split_once(' ')?;
    let dow = match days.trim_end_matches(',') {
        "daily" | "day" => "*".to_string(),
        "weekday" | "weekdays" => "1-5".into(),
        "weekend" | "weekends" => "0,6".into(),
        d => {
            let d = d.strip_suffix('s').filter(|x| x.len() > 2).unwrap_or(d);
            let i = DAYS
                .iter()
                .position(|n| d.starts_with(n) && DAY_NAMES.iter().any(|f| f.starts_with(d)))?;
            i.to_string()
        }
    };
    let at = at.trim().strip_prefix("at ").unwrap_or(at.trim());
    Some(clock(at).map(|(h, m)| {
        if dow == "*" {
            format!("daily {h:02}:{m:02}")
        } else {
            format!("{m} {h} * * {dow}")
        }
    }))
}

const DAY_NAMES: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

/// "7:30", "07:30", "7:30 pm", "7pm", "19:05" as (hour, minute).
pub fn clock(t: &str) -> Option<(u32, u32)> {
    let t = t.trim().replace(' ', "");
    let (t, half) = match t.strip_suffix("am").or_else(|| t.strip_suffix("a.m.")) {
        Some(x) => (x.to_string(), Some(0)),
        None => match t.strip_suffix("pm").or_else(|| t.strip_suffix("p.m.")) {
            Some(x) => (x.to_string(), Some(12)),
            None => (t, None),
        },
    };
    let num = |v: &str| {
        (!v.is_empty() && v.len() <= 2 && v.bytes().all(|b| b.is_ascii_digit()))
            .then(|| v.parse::<u32>().ok())
            .flatten()
    };
    let (h, m) = match t.split_once(':') {
        Some((h, m)) if m.len() == 2 => (num(h)?, num(m)?),
        Some(_) => return None,
        None if half.is_some() => (num(&t)?, 0),
        None => return None,
    };
    let h = match half {
        Some(add) if (1..=12).contains(&h) => h % 12 + add,
        Some(_) => return None,
        None => h,
    };
    (h <= 23 && m <= 59).then_some((h, m))
}

/// When a schedule next runs after `from`. Clock times ("daily 08:00", cron) are read in `tz`.
pub fn next_run(schedule: &str, tz: &TimeZone, from: i64) -> Result<i64, String> {
    let s = canonical(schedule)?;
    if let Some(rest) = s.strip_prefix("every ") {
        let (num, unit) = rest.split_at(rest.len() - 1);
        let n: i64 = num.parse().map_err(|_| bad(schedule))?;
        let secs = match unit {
            "m" => 60,
            "h" => 3600,
            _ => 86_400,
        };
        return Ok(from + n * secs);
    }
    let cron = match s.strip_prefix("daily ") {
        Some(t) => {
            let (h, m) = t.split_once(':').ok_or_else(|| bad(schedule))?;
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
/// Gives the schedule to store and its first run.
pub fn new_schedule(schedule: &str, tz: &TimeZone, from: i64) -> Result<(String, i64), String> {
    let s = canonical(schedule)?;
    let next = next_run(&s, tz, from)?;
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
    Ok((s, next))
}

/// The one clock time a stored schedule runs at each day it runs, if it has exactly one.
pub fn time_of_day(canonical: &str) -> Option<(u32, u32)> {
    if let Some(t) = canonical.strip_prefix("daily ") {
        return clock(t);
    }
    let f: Vec<&str> = canonical.split(' ').collect();
    match f.as_slice() {
        [m, h, _, _, _] => Some((h.parse().ok()?, m.parse().ok()?)),
        _ => None,
    }
}

pub const UNSUPPORTED: &str = "unsupported schedule";

fn bad(s: &str) -> String {
    format!(
        "{UNSUPPORTED} \"{s}\": use \"daily 08:15\", \"weekdays 07:30\", \"every 30m\", \"every 2h\" or 5-field cron \
         \"minute hour day month weekday\" such as \"30 7 * * 1-5\" (07:30 on weekdays), in the owner's time zone"
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

/// Jobs to run now. A pending job never runs, even with a next run set; nor does one an event starts.
pub fn due(jobs: Vec<Job>, at: i64) -> Vec<Job> {
    jobs.into_iter()
        .filter(|j| j.enabled && j.status != PENDING && j.trigger.is_none())
        .filter(|j| j.next_run_at.is_some_and(|t| t <= at))
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
            // paused or deleted since the read: it does not run
            match app.store.start_job(job.id, next) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(e) => {
                    eprintln!("scheduler: job {}: {e}", job.id);
                    continue;
                }
            }
            start(app.clone(), job, tz, next, None);
        }
    }
}

/// One run of a routine: the session it ran in and its reply, or why it did not finish.
pub struct Ran {
    pub session: String,
    pub reply: Result<String, String>,
}

impl Ran {
    /// What the routine list shows: "ok" or "error: …".
    pub fn status(&self) -> String {
        match &self.reply {
            Ok(_) => "ok".into(),
            Err(e) => format!("error: {e}"),
        }
    }
}

/// Runs a routine once, here and now, as every run does: its own prompt in a session of its own,
/// never as the owner. `event` is the fenced content of the event that started it: outside
/// content, so writes are refused and nothing new is scheduled from it.
pub fn execute(app: &App, job: &Job, tz: TimeZone, event: Option<&str>, title: &str) -> Ran {
    let kind = if event.is_some() { "event" } else { "cron" };
    let session = format!("{kind}-{}-{}", job.id, now());
    let Some(bot) = app.bots.get(&job.bot) else {
        return Ran {
            session,
            reply: Err(format!("no bot \"{}\"", job.bot)),
        };
    };
    if let Err(e) = app.store.ensure_session(
        &job.user,
        &session,
        &bot.name,
        &format!("{}: {title}", job.name),
        kind,
    ) {
        eprintln!("scheduler: job {}: session: {e}", job.id);
    }
    // The run is the delivery: asked "remind me to X", the model answered "I can't send a direct
    // reminder" (QA 2026-11-13). It is told it is the routine reaching the owner, and to give the result.
    let text = routine_turn(&job.name, &job.prompt);
    let reply = crate::agent::run(
        app,
        bot,
        crate::agent::Input {
            user: &job.user,
            session: &session,
            text: &text,
            context: event,
            outside: event.is_some(),
            owner: false,
            research: None,
            tz,
            workspace: None,
            cancel: None,
            progress: None,
            on_text: None,
        },
    )
    .map(|t| t.reply);
    Ran { session, reply }
}

/// What a routine run asks: its own task, framed as the routine reaching the owner now.
pub fn routine_turn(name: &str, prompt: &str) -> String {
    format!(
        "This is your routine \"{name}\" running now, as the owner set it up. Your reply is how it \
         reaches them: reply with its result itself. If the task is to remind them of something, your \
         reply is that reminder, said directly (for example \"Reminder: ...\"); never say you cannot \
         send reminders. Do not create, change or delete routines.\n\nThe routine's task: {prompt}"
    )
}

/// Runs a routine off the async threads and records how it went; see `execute`.
pub fn start(
    app: Arc<App>,
    job: Job,
    tz: TimeZone,
    next: Option<i64>,
    event: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let title = if event.is_some() {
            "on an event"
        } else {
            "scheduled"
        };
        let ran = execute(&app, &job, tz, event.as_deref(), title);
        if let Err(e) = app.store.finish_job(job.id, &ran.status(), next) {
            eprintln!("scheduler: job {}: {e}", job.id);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{due, new_schedule, next_run, routine_turn, zone};
    use crate::store::{PENDING, Store};
    use jiff::tz::TimeZone;
    const UTC: TimeZone = TimeZone::UTC;

    #[test]
    fn a_reminder_run_is_told_its_reply_is_the_reminder() {
        let t = routine_turn("rice", "remind me to count the rice and canned goods stock");
        assert!(
            t.contains("your reply is that reminder") || t.contains("reply is that reminder"),
            "{t}"
        );
        assert!(
            t.ends_with("The routine's task: remind me to count the rice and canned goods stock"),
            "{t}"
        );
        assert!(t.contains("never say you cannot send reminders"), "{t}");
    }

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
        s.confirm_job("a", wait, Some(50), "UTC").unwrap();
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

    /// QA 0.2.9: a time with minutes is read as that time, in the forms a model writes it;
    /// one that cannot be read is refused, never taken as another time.
    #[test]
    fn minutes_are_kept() {
        use super::canonical;
        for (said, kept) in [
            ("0 7:30 * * 1-5", "30 7 * * 1-5"),
            ("7:30 * * 1-5", "30 7 * * 1-5"),
            ("weekdays 07:30", "30 7 * * 1-5"),
            ("every weekday at 7:30 am", "30 7 * * 1-5"),
            ("Weekdays 7:30PM", "30 19 * * 1-5"),
            ("daily 08:15", "daily 08:15"),
            ("daily 8:15", "daily 08:15"),
            ("every day at 12am", "daily 00:00"),
            ("monday 18:45", "45 18 * * 1"),
            ("every friday at 9pm", "0 21 * * 5"),
            ("weekends 9am", "0 9 * * 0,6"),
            ("30 7 * * 1-5", "30 7 * * 1-5"),
            ("0  9 * *  MON-FRI", "0 9 * * mon-fri"),
            ("every 2 h", "every 2h"),
        ] {
            assert_eq!(canonical(said).as_deref(), Ok(kept), "{said}");
        }
        for bad in [
            "15 7:30 * * 1-5",
            "* 7:30 * * *",
            "0 7:30 * * 1-5 *",
            "weekdays 7",
            "daily 7:3",
            "daily 24:00",
            "weekdays 13pm",
            "at 7:30 on weekdays",
            "every day at half past seven",
            "every 2x",
            "every 2ü",
            "someday 07:30",
        ] {
            let e = canonical(bad).unwrap_err();
            assert!(
                e.contains("\"30 7 * * 1-5\"") && e.contains("weekdays 07:30"),
                "{bad}: {e}"
            );
        }
        // Fri 6 Nov 2026 18:00 in Manila: next weekday 7:30 is Monday's
        let from = utc("2026-11-06T10:00:00Z");
        let manila = zone("Asia/Manila").unwrap();
        for s in ["0 7:30 * * 1-5", "weekdays 07:30"] {
            let t = next_run(s, &manila, from).unwrap();
            assert_eq!(
                super::local_at(t, &manila),
                "Mon 2026-11-09 07:30 Asia/Manila",
                "{s}"
            );
        }
        assert_eq!(
            new_schedule("0 7:30 * * 1-5", &manila, from).unwrap().0,
            "30 7 * * 1-5"
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
