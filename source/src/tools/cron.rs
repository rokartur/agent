use crate::agent::FollowUp;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Prompts that CronCreate and ScheduleWakeup queue as follow-ups; they live only as long as this process.
#[derive(Default)]
pub struct Crons {
    jobs: Vec<Job>,
    next_id: usize,
}

pub struct Job {
    pub id: String,
    when: When,
    pub prompt: String,
}

impl Job {
    pub fn when(&self) -> String {
        match &self.when {
            When::Cron { expression, recurring: true, .. } => format!("{expression} recurring"),
            When::Cron { expression, recurring: false, .. } => format!("{expression} once"),
            When::Once(at) => format!("in {}s", at.duration_since(SystemTime::now()).unwrap_or_default().as_secs()),
        }
    }
}

#[derive(Clone)]
enum When {
    Cron { expression: String, fields: [u64; 5], recurring: bool },
    Once(SystemTime),
}

const MAX_DELAY_SECONDS: u64 = 86_400;
const RANGES: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];

pub fn create(crons: &Arc<Mutex<Crons>>, input: &Value, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) -> Result<String, String> {
    let expression = input["cron"].as_str().ok_or("cron required.")?.trim();
    let prompt = required_prompt(input)?;
    let fields = parse(expression)?;
    let recurring = input["recurring"].as_bool().unwrap_or(true);
    let id = start(crons, When::Cron { expression: expression.to_string(), fields, recurring }, prompt, follow_ups);
    let repeats = if recurring { "every time it matches" } else { "once, at the next match" };
    Ok(format!("Scheduled {id} ({expression}, local time), firing {repeats}. Its prompt arrives as a message; CronDelete cancels it."))
}

pub fn wakeup(crons: &Arc<Mutex<Crons>>, input: &Value, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) -> Result<String, String> {
    let delay = input["delay_seconds"].as_u64().filter(|d| (1..=MAX_DELAY_SECONDS).contains(d)).ok_or(format!("delay_seconds must be 1 to {MAX_DELAY_SECONDS}."))?;
    let prompt = required_prompt(input)?;
    let id = start(crons, When::Once(SystemTime::now() + Duration::from_secs(delay)), prompt, follow_ups);
    Ok(format!("Wakeup {id} in {delay}s. End your turn; its prompt arrives as a message. CronDelete cancels it."))
}

impl Crons {
    pub fn delete(&mut self, input: &Value) -> Result<String, String> {
        let id = input["id"].as_str().ok_or("id required.")?;
        self.cancel(id).ok_or(format!("No scheduled task {id}."))?;
        Ok(format!("Cancelled {id}."))
    }

    pub fn cancel(&mut self, id: &str) -> Option<Job> {
        let index = self.jobs.iter().position(|job| job.id == id)?;
        Some(self.jobs.remove(index))
    }

    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    /// /reload: the jobs, for `adopt` in the server it execs.
    #[cfg(unix)]
    pub fn hand_over(&self) -> Value {
        let jobs: Vec<Value> = self
            .jobs
            .iter()
            .map(|job| match &job.when {
                When::Cron { expression, recurring, .. } => serde_json::json!({"id": job.id, "prompt": job.prompt, "cron": expression, "recurring": recurring}),
                When::Once(at) => serde_json::json!({"id": job.id, "prompt": job.prompt, "at": at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()}),
            })
            .collect();
        serde_json::json!({"next_id": self.next_id, "jobs": jobs})
    }

    pub fn list(&self) -> String {
        if self.jobs.is_empty() {
            return "No scheduled tasks.".into();
        }
        let lines: Vec<String> = self.jobs.iter().map(|job| format!("{}  {}  {}", job.id, job.when(), job.prompt)).collect();
        lines.join("\n")
    }
}

fn required_prompt(input: &Value) -> Result<String, String> {
    input["prompt"].as_str().map(str::trim).filter(|p| !p.is_empty()).map(String::from).ok_or("prompt required.".into())
}

fn start(crons: &Arc<Mutex<Crons>>, when: When, prompt: String, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) -> String {
    let id = {
        let mut crons = crons.lock().unwrap();
        crons.next_id += 1;
        format!("cr-{}", crons.next_id)
    };
    resume(crons, id.clone(), when, prompt, follow_ups);
    id
}

fn resume(crons: &Arc<Mutex<Crons>>, id: String, when: When, prompt: String, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) {
    crons.lock().unwrap().jobs.push(Job { id: id.clone(), when: when.clone(), prompt });
    let (crons, follow_ups) = (crons.clone(), follow_ups.clone());
    thread::spawn(move || watch(&crons, &id, when, &follow_ups));
}

/// The jobs `Crons::hand_over` listed before /reload, each watched again.
#[cfg(unix)]
pub fn adopt(crons: &Arc<Mutex<Crons>>, handed: &Value, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) {
    crons.lock().unwrap().next_id = handed["next_id"].as_u64().unwrap_or_default() as usize;
    for job in handed["jobs"].as_array().into_iter().flatten() {
        let when = match (job["cron"].as_str(), job["at"].as_u64()) {
            (Some(expression), _) => match parse(expression) {
                Ok(fields) => When::Cron { expression: expression.into(), fields, recurring: job["recurring"] == true },
                Err(_) => continue,
            },
            (None, Some(at)) => When::Once(UNIX_EPOCH + Duration::from_secs(at)),
            (None, None) => continue,
        };
        resume(crons, job["id"].as_str().unwrap_or_default().into(), when, job["prompt"].as_str().unwrap_or_default().into(), follow_ups);
    }
}

/// Sleeps until the job is due, queues its prompt, and returns once the job is cancelled or done.
fn watch(crons: &Mutex<Crons>, id: &str, when: When, follow_ups: &Mutex<Vec<FollowUp>>) {
    loop {
        let pause = match &when {
            When::Once(at) => at.duration_since(SystemTime::now()).unwrap_or_default(),
            When::Cron { .. } => Duration::from_secs(60 - unix_seconds() % 60),
        };
        thread::sleep(pause);
        let mut crons = crons.lock().unwrap();
        let Some(index) = crons.jobs.iter().position(|job| job.id == id) else { return };
        let (due, once) = match &when {
            When::Once(_) => (true, true),
            When::Cron { fields, recurring, .. } => (matches(fields, local_fields(unix_seconds() / 60 * 60)), !recurring),
        };
        if !due {
            continue;
        }
        let prompt = if once { crons.jobs.remove(index).prompt } else { crons.jobs[index].prompt.clone() };
        follow_ups.lock().unwrap().push(FollowUp { source: id.to_string(), text: format!("Scheduled task {id} is due: {prompt}") });
        if once {
            return;
        }
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after 1970").as_secs()
}

/// Minute, hour, day of month, month and weekday (Sunday 0) of `unix` in the local time zone.
fn local_fields(unix: u64) -> [u32; 5] {
    let tm = crate::process::local_time(unix);
    [tm.tm_min, tm.tm_hour, tm.tm_mday, tm.tm_mon + 1, tm.tm_wday].map(|value| value as u32)
}

/// A 5-field cron expression as one bit set per field; supports `*`, numbers, `a-b`, `/step` and `,` lists.
fn parse(expression: &str) -> Result<[u64; 5], String> {
    let parts: Vec<&str> = expression.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(format!("cron needs 5 fields (minute hour day-of-month month day-of-week), got {expression:?}."));
    }
    let mut fields = [0; 5];
    for (index, part) in parts.iter().enumerate() {
        fields[index] = field(part, RANGES[index]).ok_or(format!("Bad cron field {part:?} in {expression:?}."))?;
    }
    // Sunday is 0 or 7.
    if fields[4] & 1 << 7 != 0 {
        fields[4] = fields[4] & !(1 << 7) | 1;
    }
    Ok(fields)
}

fn field(text: &str, (min, max): (u32, u32)) -> Option<u64> {
    let mut bits = 0;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => (range, step.parse().ok().filter(|step| *step > 0)?),
            None => (item, 1),
        };
        let (from, to) = match range.split_once('-') {
            _ if range == "*" => (min, max),
            Some((from, to)) => (from.parse().ok()?, to.parse().ok()?),
            // `5/15` runs from 5 to the end of the range.
            None if step > 1 => (range.parse().ok()?, max),
            None => (range.parse().ok()?, range.parse().ok()?),
        };
        if from < min || to > max || from > to {
            return None;
        }
        for value in (from..=to).step_by(step) {
            bits |= 1 << value;
        }
    }
    Some(bits)
}

fn matches(fields: &[u64; 5], [minute, hour, day, month, weekday]: [u32; 5]) -> bool {
    let has = |index: usize, value: u32| fields[index] & 1 << value != 0;
    let every_day = Some(fields[2]) == field("*", RANGES[2]);
    let every_weekday = Some(fields[4]) == field("*", (0, 6));
    // Like Vixie cron: when both day fields are restricted, either one matching is enough.
    let day_matches = if every_day || every_weekday { has(2, day) && has(4, weekday) } else { has(2, day) || has(4, weekday) };
    has(0, minute) && has(1, hour) && has(3, month) && day_matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_cron_fields_and_matches_local_minutes() {
        let every_five = parse("*/5 * * * *").unwrap();
        assert!(matches(&every_five, [10, 3, 1, 1, 0]));
        assert!(!matches(&every_five, [11, 3, 1, 1, 0]));
        let weekdays_at_nine = parse("0 9 * * 1-5").unwrap();
        assert!(matches(&weekdays_at_nine, [0, 9, 14, 6, 3]));
        assert!(!matches(&weekdays_at_nine, [0, 9, 15, 6, 6]));
        let sunday = parse("30 8 * * 7").unwrap();
        assert!(matches(&sunday, [30, 8, 2, 3, 0]));
        let first_or_monday = parse("0 0 1 * 1").unwrap();
        assert!(matches(&first_or_monday, [0, 0, 1, 5, 4]));
        assert!(matches(&first_or_monday, [0, 0, 9, 5, 1]));
        assert!(!matches(&first_or_monday, [0, 0, 9, 5, 2]));
        assert!(matches(&parse("5/20 * * * *").unwrap(), [45, 0, 1, 1, 0]));
        assert!(matches(&parse("7 7 7 7 *").unwrap(), [7, 7, 7, 7, 3]));
        for bad in ["* * * *", "60 * * * *", "*/0 * * * *", "5-1 * * * *", "a * * * *"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn wakeup_queues_its_prompt_once_and_delete_cancels() {
        let crons = Arc::new(Mutex::new(Crons::default()));
        let follow_ups = Arc::new(Mutex::new(Vec::new()));
        wakeup(&crons, &json!({"delay_seconds": 1, "prompt": "check CI"}), &follow_ups).unwrap();
        wakeup(&crons, &json!({"delay_seconds": 1, "prompt": "cancelled"}), &follow_ups).unwrap();
        assert!(crons.lock().unwrap().list().contains("cr-2  in "));
        crons.lock().unwrap().delete(&json!({"id": "cr-2"})).unwrap();
        thread::sleep(Duration::from_millis(1500));
        let queued: Vec<String> = follow_ups.lock().unwrap().iter().map(|f| f.text.clone()).collect();
        assert_eq!(queued, ["Scheduled task cr-1 is due: check CI"]);
        assert_eq!(crons.lock().unwrap().list(), "No scheduled tasks.");
        assert!(wakeup(&crons, &json!({"delay_seconds": 0, "prompt": "x"}), &follow_ups).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn handed_over_jobs_keep_their_ids_and_still_fire() {
        let (crons, follow_ups) = (Arc::new(Mutex::new(Crons::default())), Arc::new(Mutex::new(Vec::new())));
        create(&crons, &json!({"cron": "0 9 * * 1-5", "prompt": "standup", "recurring": false}), &follow_ups).unwrap();
        wakeup(&crons, &json!({"delay_seconds": 2, "prompt": "check CI"}), &follow_ups).unwrap();
        let adopted = Arc::new(Mutex::new(Crons::default()));
        adopt(&adopted, &crons.lock().unwrap().hand_over(), &follow_ups);
        crons.lock().unwrap().jobs.clear();
        let list = adopted.lock().unwrap().list();
        assert!(list.starts_with("cr-1  0 9 * * 1-5 once  standup\ncr-2  in "), "{list}");
        wakeup(&adopted, &json!({"delay_seconds": 60, "prompt": "later"}), &follow_ups).unwrap();
        assert!(adopted.lock().unwrap().list().contains("cr-3  in "));
        thread::sleep(Duration::from_millis(3000));
        let queued: Vec<String> = follow_ups.lock().unwrap().iter().map(|f| f.text.clone()).collect();
        assert_eq!(queued, ["Scheduled task cr-2 is due: check CI"]);
    }
}
