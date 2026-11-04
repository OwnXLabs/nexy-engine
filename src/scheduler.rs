use crate::App;
use crate::store::now;
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
    let (minute, hour) = if let Some(t) = s.strip_prefix("daily ") {
        let (h, m) = t.trim().split_once(':').ok_or_else(|| bad(schedule))?;
        (
            m.parse::<i64>().map_err(|_| bad(schedule))?,
            h.parse::<i64>().map_err(|_| bad(schedule))?,
        )
    } else {
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() != 5 || f[2..] != ["*", "*", "*"] {
            return Err(bad(schedule));
        }
        (
            f[0].parse().map_err(|_| bad(schedule))?,
            f[1].parse().map_err(|_| bad(schedule))?,
        )
    };
    if !(0..60).contains(&minute) || !(0..24).contains(&hour) {
        return Err(bad(schedule));
    }
    let day = from - from.rem_euclid(86_400);
    let at = day + hour * 3600 + minute * 60;
    Ok(if at > from { at } else { at + 86_400 })
}

fn bad(s: &str) -> String {
    format!(
        "unsupported schedule \"{s}\": use \"every 30m\", \"every 2h\", \"daily 08:00\" (UTC) or \"M H * * *\""
    )
}

pub async fn run(app: Arc<App>) {
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let due: Vec<_> = match app.store.jobs() {
            Ok(j) => j
                .into_iter()
                .filter(|j| j.enabled && j.next_run_at.is_some_and(|t| t <= now()))
                .collect(),
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
                            &session,
                            &bot.name,
                            &format!("{}: scheduled", job.name),
                            "cron",
                        );
                        match crate::agent::run(
                            &app,
                            bot,
                            crate::agent::Input {
                                session: &session,
                                text: &job.prompt,
                                context: None,
                                owner: false,
                                workspace: None,
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
    use super::next_run;

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
}
