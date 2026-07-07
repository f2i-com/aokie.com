//! Appointment calendar — services, business hours, bookings, and the
//! free-slot calculator the bot uses to quote times.
//!
//! Schema lives in `database::migrate_v5`. Settings (enabled flag,
//! per-weekday hours, timezone, buffer) ride in the existing `config`
//! key/value table — they're read here through `load_config()` rather
//! than threaded through every callsite.
//!
//! All times persisted in SQLite are unix epoch **seconds in UTC**.
//! Conversion to local wall-clock happens at the boundary using the
//! configured `timezone` (`chrono_tz::Tz`); empty string means "use
//! whatever the host says is local," which is correct for single-
//! machine deployments.
//!
//! The bot integration (phase 5) calls `list_services`, `available_slots`,
//! and `book_appointment` and never touches SQL directly — the goal is
//! that the same call shape works from SMS auto-reply, the live call
//! loop, and the manual UI without each adding its own conflict logic.

#![allow(dead_code)] // wired in across phases 3–7

pub mod prompt;
pub mod session;
pub mod tools;
pub mod verbal;

// Same Windows-only consumer pattern as the verbal re-exports below
// — these names are referenced from the BT call body that doesn't
// compile on Linux. Keep them surfaced; suppress the unused-import
// warning on the Linux developer-preview build.
#[allow(unused_imports)]
pub use session::{CallSession, OfferedSlots, SessionMode};
#[allow(unused_imports)]
pub use tools::ToolCall;

use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, NaiveDate, NaiveTime, TimeZone, Utc, Weekday,
};
use chrono_tz::Tz;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

// ============================================================================
// Settings (mirror the JSON shape the frontend writes via save_config_to_file)
// ============================================================================

/// Calendar slice of `AppConfig`. Optional in the parent struct so an
/// older config.json without a calendar section round-trips cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarConfig {
    #[serde(default)]
    pub enabled: bool,
    /// IANA timezone name (e.g. "Australia/Sydney"). Empty string =
    /// host-local time, which is what most single-business deployments
    /// want.
    #[serde(default)]
    pub timezone: String,
    /// Minutes of dead time between back-to-back appointments. Used
    /// for travel/setup; subtracted from each candidate slot's tail
    /// when checking conflicts.
    #[serde(default, rename = "bufferMinutes")]
    pub buffer_minutes: u32,
    #[serde(default = "default_business_hours", rename = "businessHours")]
    pub business_hours: BusinessHours,
}

impl Default for CalendarConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timezone: String::new(),
            buffer_minutes: 0,
            business_hours: default_business_hours(),
        }
    }
}

/// Per-weekday hours. Field names match what the React Settings tab
/// will emit so deserialization is direct.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BusinessHours {
    pub monday: DayHours,
    pub tuesday: DayHours,
    pub wednesday: DayHours,
    pub thursday: DayHours,
    pub friday: DayHours,
    pub saturday: DayHours,
    pub sunday: DayHours,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DayHours {
    /// `false` = closed on this weekday; `start`/`end`/`lunch` are
    /// ignored.
    pub open: bool,
    /// Local-time wall-clock "HH:MM" (24h). E.g. "09:00".
    pub start: String,
    pub end: String,
    /// Optional carve-out where bookings are not allowed, e.g.
    /// 12:00–13:00 lunch. None = no break.
    #[serde(default)]
    pub lunch: Option<TimeRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimeRange {
    pub start: String,
    pub end: String,
}

fn default_business_hours() -> BusinessHours {
    let weekday = DayHours {
        open: true,
        start: "09:00".to_string(),
        end: "17:00".to_string(),
        lunch: Some(TimeRange {
            start: "12:00".to_string(),
            end: "13:00".to_string(),
        }),
    };
    let weekend = DayHours {
        open: false,
        start: "09:00".to_string(),
        end: "17:00".to_string(),
        lunch: None,
    };
    BusinessHours {
        monday: weekday.clone(),
        tuesday: weekday.clone(),
        wednesday: weekday.clone(),
        thursday: weekday.clone(),
        friday: weekday,
        saturday: weekend.clone(),
        sunday: weekend,
    }
}

impl BusinessHours {
    fn for_weekday(&self, w: Weekday) -> &DayHours {
        match w {
            Weekday::Mon => &self.monday,
            Weekday::Tue => &self.tuesday,
            Weekday::Wed => &self.wednesday,
            Weekday::Thu => &self.thursday,
            Weekday::Fri => &self.friday,
            Weekday::Sat => &self.saturday,
            Weekday::Sun => &self.sunday,
        }
    }
}

fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M").ok()
}

// ============================================================================
// Services CRUD
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Service {
    pub id: i64,
    pub name: String,
    pub duration_minutes: u32,
    pub description: Option<String>,
    pub active: bool,
    pub sort_order: i64,
    pub created_at: i64,
    /// When true, the bot must collect a street address from the
    /// customer before booking; the address goes into the appointment
    /// notes. Off-site services (lawn mowing, plumbing) flip this on;
    /// in-shop services (haircuts, phone consults) leave it off.
    pub requires_address: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInput {
    pub name: String,
    pub duration_minutes: u32,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub sort_order: i64,
    #[serde(default)]
    pub requires_address: bool,
}

fn default_true() -> bool {
    true
}

pub fn list_services(conn: &Connection, only_active: bool) -> rusqlite::Result<Vec<Service>> {
    let sql = if only_active {
        "SELECT id, name, duration_minutes, description, active, sort_order, created_at, requires_address \
         FROM services WHERE active = 1 ORDER BY sort_order, name"
    } else {
        "SELECT id, name, duration_minutes, description, active, sort_order, created_at, requires_address \
         FROM services ORDER BY sort_order, name"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], row_to_service)?;
    rows.collect()
}

pub fn get_service(conn: &Connection, id: i64) -> rusqlite::Result<Option<Service>> {
    conn.query_row(
        "SELECT id, name, duration_minutes, description, active, sort_order, created_at, requires_address \
         FROM services WHERE id = ?1",
        params![id],
        row_to_service,
    )
    .optional()
}

pub fn create_service(conn: &Connection, input: ServiceInput) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO services (name, duration_minutes, description, active, sort_order, created_at, requires_address) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            input.name,
            input.duration_minutes,
            input.description,
            input.active as i64,
            input.sort_order,
            now_unix(),
            input.requires_address as i64,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_service(conn: &Connection, id: i64, input: ServiceInput) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE services SET name = ?1, duration_minutes = ?2, description = ?3, \
         active = ?4, sort_order = ?5, requires_address = ?6 WHERE id = ?7",
        params![
            input.name,
            input.duration_minutes,
            input.description,
            input.active as i64,
            input.sort_order,
            input.requires_address as i64,
            id,
        ],
    )?;
    Ok(())
}

/// Soft-delete: set active=0. Use `delete_service_hard` only when you
/// know no historical appointments reference this id.
pub fn deactivate_service(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE services SET active = 0 WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn delete_service_hard(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    // FK is ON DELETE SET NULL on appointments.service_id, so historical
    // bookings keep the denormalized service_name and just lose the id link.
    conn.execute("DELETE FROM services WHERE id = ?1", params![id])?;
    Ok(())
}

fn row_to_service(row: &rusqlite::Row<'_>) -> rusqlite::Result<Service> {
    Ok(Service {
        id: row.get(0)?,
        name: row.get(1)?,
        duration_minutes: row.get::<_, i64>(2)? as u32,
        description: row.get(3)?,
        active: row.get::<_, i64>(4)? != 0,
        sort_order: row.get(5)?,
        created_at: row.get(6)?,
        requires_address: row.get::<_, i64>(7)? != 0,
    })
}

// ============================================================================
// Appointments CRUD
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Appointment {
    pub id: i64,
    pub service_id: Option<i64>,
    pub service_name: String,
    /// Unix seconds, UTC.
    pub start_time: i64,
    pub duration_minutes: u32,
    pub customer_phone: Option<String>,
    pub customer_name: Option<String>,
    pub notes: Option<String>,
    pub status: String,
    pub source: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppointmentInput {
    /// `None` = ad-hoc booking (no catalog service). Bot bookings always
    /// pass a service_id.
    pub service_id: Option<i64>,
    /// Required so the row stays readable even if `service_id` is later
    /// soft- or hard-deleted.
    pub service_name: String,
    pub start_time: i64,
    pub duration_minutes: u32,
    #[serde(default)]
    pub customer_phone: Option<String>,
    #[serde(default)]
    pub customer_name: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// "sms" | "call" | "manual". Defaults to "manual".
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_source() -> String {
    "manual".to_string()
}

/// Check for time overlap with any existing booked appointment, plus a
/// `buffer_minutes` shoulder on each side. Returns the first conflicting
/// row if any. Use this before INSERT to avoid double-bookings.
pub fn find_conflict(
    conn: &Connection,
    start_time: i64,
    duration_minutes: u32,
    buffer_minutes: u32,
    ignore_id: Option<i64>,
) -> rusqlite::Result<Option<Appointment>> {
    let buf = buffer_minutes as i64 * 60;
    let candidate_start = start_time - buf;
    let candidate_end = start_time + (duration_minutes as i64 * 60) + buf;

    // Two ranges overlap iff: start_a < end_b AND start_b < end_a.
    // We translate that into SQL by computing each existing row's
    // [start, start + duration*60) and checking against the candidate.
    let sql = "SELECT id, service_id, service_name, start_time, duration_minutes, \
               customer_phone, customer_name, notes, status, source, created_at \
               FROM appointments \
               WHERE status = 'booked' \
                 AND (?2 IS NULL OR id != ?2) \
                 AND start_time < ?1 \
                 AND (start_time + duration_minutes * 60) > ?3 \
               ORDER BY start_time LIMIT 1";
    conn.query_row(
        sql,
        params![candidate_end, ignore_id, candidate_start],
        row_to_appointment,
    )
    .optional()
}

pub fn book_appointment(
    conn: &Connection,
    input: AppointmentInput,
    buffer_minutes: u32,
) -> Result<i64, BookingError> {
    if input.duration_minutes == 0 {
        return Err(BookingError::Invalid("duration must be > 0".into()));
    }
    // Refuse bookings in the past (with a small grace window for clock
    // skew between the LLM-emitted local-time and our wall-clock). The
    // bot occasionally invents a date that's already gone; without this
    // we'd persist it and confuse the operator.
    let now = now_unix();
    if input.start_time + 5 * 60 < now {
        return Err(BookingError::Invalid(format!(
            "start_time {} is in the past (now = {})",
            input.start_time, now
        )));
    }
    // BEGIN IMMEDIATE upgrades the connection to a write lock right
    // away, so two simultaneous booking attempts don't both pass the
    // conflict check and then both INSERT. SQLite serializes writes
    // anyway, but without this the second caller sees stale data
    // during its conflict-check and only the INSERT collides at write
    // time — which on the default rolled-back-on-error journal looks
    // like a generic SQL error rather than a typed conflict.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(BookingError::Sql)?;
    if let Some(conflict) = find_conflict(
        conn,
        input.start_time,
        input.duration_minutes,
        buffer_minutes,
        None,
    )
    .map_err(BookingError::Sql)?
    {
        return Err(BookingError::Conflict(conflict));
    }
    conn.execute(
        "INSERT INTO appointments \
         (service_id, service_name, start_time, duration_minutes, customer_phone, \
          customer_name, notes, status, source, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'booked', ?8, ?9)",
        params![
            input.service_id,
            input.service_name,
            input.start_time,
            input.duration_minutes,
            input.customer_phone,
            input.customer_name,
            input.notes,
            input.source,
            now,
        ],
    )
    .map_err(BookingError::Sql)?;
    let id = conn.last_insert_rowid();
    tx.commit().map_err(BookingError::Sql)?;
    Ok(id)
}

/// Reschedule / edit an existing booked appointment. Atomic — runs
/// inside a transaction with a conflict re-check using `ignore_id`, so
/// two operators editing simultaneously can't both win, and a moved
/// slot can't quietly collide with another booking.
///
/// Status must be `booked`; cancelled/completed rows are immutable.
pub fn update_appointment(
    conn: &Connection,
    id: i64,
    input: AppointmentInput,
    buffer_minutes: u32,
) -> Result<(), BookingError> {
    if input.duration_minutes == 0 {
        return Err(BookingError::Invalid("duration must be > 0".into()));
    }
    // BEGIN IMMEDIATE — same anti-double-edit rationale as
    // `book_appointment`. If two operators edit overlapping appointments
    // at the same time we want the second to see the first's commit
    // before re-running the conflict check, not race ahead on a snapshot.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(BookingError::Sql)?;
    let existing = get_appointment(conn, id).map_err(BookingError::Sql)?;
    let Some(existing) = existing else {
        return Err(BookingError::Invalid(format!("no appointment #{}", id)));
    };
    if existing.status != "booked" {
        return Err(BookingError::Invalid(format!(
            "appointment #{} is {}, can't edit",
            id, existing.status
        )));
    }
    // Past-date guard only applies if the operator is *moving* the
    // appointment to a new past date. Editing notes/customer info
    // on a past booking that hasn't been marked complete yet is a
    // legitimate operation — without this carve-out the operator
    // gets a confusing error when fixing a typo on yesterday's row.
    if input.start_time != existing.start_time {
        let now = now_unix();
        if input.start_time + 5 * 60 < now {
            return Err(BookingError::Invalid(format!(
                "start_time {} is in the past (now = {})",
                input.start_time, now
            )));
        }
    }
    if let Some(conflict) = find_conflict(
        conn,
        input.start_time,
        input.duration_minutes,
        buffer_minutes,
        Some(id),
    )
    .map_err(BookingError::Sql)?
    {
        return Err(BookingError::Conflict(conflict));
    }
    conn.execute(
        "UPDATE appointments SET \
            service_id = ?1, service_name = ?2, start_time = ?3, \
            duration_minutes = ?4, customer_phone = ?5, customer_name = ?6, \
            notes = ?7, source = ?8 \
         WHERE id = ?9",
        params![
            input.service_id,
            input.service_name,
            input.start_time,
            input.duration_minutes,
            input.customer_phone,
            input.customer_name,
            input.notes,
            input.source,
            id,
        ],
    )
    .map_err(BookingError::Sql)?;
    tx.commit().map_err(BookingError::Sql)?;
    Ok(())
}

#[derive(Debug)]
pub enum BookingError {
    /// Slot collides with an existing booked appointment (after buffer).
    Conflict(Appointment),
    /// Validation failure — past date, zero duration, missing row, etc.
    /// Distinguished from `Sql` so the bot can phrase a "that won't work"
    /// reply rather than apologising for a database error.
    Invalid(String),
    Sql(rusqlite::Error),
}

impl std::fmt::Display for BookingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BookingError::Conflict(a) => write!(
                f,
                "slot conflicts with existing appointment #{} ({} at {})",
                a.id, a.service_name, a.start_time
            ),
            BookingError::Invalid(msg) => write!(f, "{}", msg),
            BookingError::Sql(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for BookingError {}

pub fn cancel_appointment(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE appointments SET status = 'cancelled' WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn complete_appointment(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE appointments SET status = 'completed' WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn get_appointment(conn: &Connection, id: i64) -> rusqlite::Result<Option<Appointment>> {
    conn.query_row(
        "SELECT id, service_id, service_name, start_time, duration_minutes, \
         customer_phone, customer_name, notes, status, source, created_at \
         FROM appointments WHERE id = ?1",
        params![id],
        row_to_appointment,
    )
    .optional()
}

pub fn list_appointments_in_range(
    conn: &Connection,
    range_start_unix: i64,
    range_end_unix: i64,
) -> rusqlite::Result<Vec<Appointment>> {
    let mut stmt = conn.prepare(
        "SELECT id, service_id, service_name, start_time, duration_minutes, \
         customer_phone, customer_name, notes, status, source, created_at \
         FROM appointments \
         WHERE start_time >= ?1 AND start_time < ?2 \
         ORDER BY start_time",
    )?;
    let rows = stmt.query_map(
        params![range_start_unix, range_end_unix],
        row_to_appointment,
    )?;
    rows.collect()
}

fn row_to_appointment(row: &rusqlite::Row<'_>) -> rusqlite::Result<Appointment> {
    Ok(Appointment {
        id: row.get(0)?,
        service_id: row.get(1)?,
        service_name: row.get(2)?,
        start_time: row.get(3)?,
        duration_minutes: row.get::<_, i64>(4)? as u32,
        customer_phone: row.get(5)?,
        customer_name: row.get(6)?,
        notes: row.get(7)?,
        status: row.get(8)?,
        source: row.get(9)?,
        created_at: row.get(10)?,
    })
}

// ============================================================================
// Availability — the core function the bot calls to quote slots
// ============================================================================

/// Resolved time zone for the calendar.
///
/// Empty string = "host local time" — we ask the OS for its IANA name
/// (Windows: registry → mapped to IANA, Unix: `TZ` env / `/etc/localtime`).
/// If detection fails OR the configured zone name doesn't parse, we
/// fall back to UTC and log so the operator can see why the bot is
/// quoting times that look off.
///
/// The previous implementation silently returned UTC for empty zone,
/// which produced a 10h drift on this Australian box: the bot offered
/// "Tuesday 10:00" (interpreted as UTC), the UI rendered it as 20:00
/// AEST. Detecting the host TZ closes that gap for the default case.
pub(crate) fn resolve_tz(timezone: &str) -> Tz {
    let trimmed = timezone.trim();
    if trimmed.is_empty() {
        return host_tz();
    }
    match trimmed.parse::<Tz>() {
        Ok(tz) => tz,
        Err(_) => {
            eprintln!(
                "[calendar] configured timezone {:?} did not parse; falling back to host TZ",
                trimmed
            );
            host_tz()
        }
    }
}

/// Best-effort detection of the host's IANA timezone, e.g.
/// "Australia/Sydney". Returns `Tz::UTC` if detection fails.
fn host_tz() -> Tz {
    match iana_time_zone::get_timezone() {
        Ok(name) => match name.parse::<Tz>() {
            Ok(tz) => tz,
            Err(_) => {
                eprintln!(
                    "[calendar] host timezone {:?} not in chrono-tz database; using UTC",
                    name
                );
                Tz::UTC
            }
        },
        Err(e) => {
            eprintln!(
                "[calendar] could not detect host timezone ({}); using UTC",
                e
            );
            Tz::UTC
        }
    }
}

/// One bookable interval [start_utc, end_utc) in unix seconds. Output of
/// `available_slots`. The bot quotes these to the caller; UI renders
/// them in the configured timezone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Slot {
    pub start_unix: i64,
    pub end_unix: i64,
}

/// Compute every free slot of `duration_minutes` on the given calendar
/// date, snapped to a `step_minutes` grid (typically 15 or 30) and
/// respecting business hours, lunch breaks, the buffer setting, and any
/// existing appointments.
///
/// `date` is interpreted in the calendar's configured timezone.
pub fn available_slots(
    conn: &Connection,
    config: &CalendarConfig,
    date: NaiveDate,
    duration_minutes: u32,
    step_minutes: u32,
) -> rusqlite::Result<Vec<Slot>> {
    if duration_minutes == 0 || step_minutes == 0 {
        return Ok(Vec::new());
    }
    let tz = resolve_tz(&config.timezone);
    let day_hours = config.business_hours.for_weekday(date.weekday());
    if !day_hours.open {
        return Ok(Vec::new());
    }
    let (Some(open_t), Some(close_t)) = (parse_hhmm(&day_hours.start), parse_hhmm(&day_hours.end))
    else {
        return Ok(Vec::new());
    };
    if close_t <= open_t {
        return Ok(Vec::new());
    }

    // Build the (at most two) working windows by carving the lunch
    // break out of [open, close).
    let lunch = day_hours.lunch.as_ref().and_then(|r| {
        let s = parse_hhmm(&r.start)?;
        let e = parse_hhmm(&r.end)?;
        if e > s && s >= open_t && e <= close_t {
            Some((s, e))
        } else {
            None
        }
    });
    let windows: Vec<(NaiveTime, NaiveTime)> = match lunch {
        Some((ls, le)) => vec![(open_t, ls), (le, close_t)],
        None => vec![(open_t, close_t)],
    };

    // Pull existing appointments for the day, plus a 24h shoulder so
    // an appointment starting late the previous day still factors into
    // morning conflicts. We treat status='booked' as the only thing
    // that consumes a slot — cancelled/completed are inert.
    let day_start_utc = local_date_to_utc(tz, date, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    let day_end_utc = day_start_utc + ChronoDuration::days(1);
    let neighborhood_start = day_start_utc - ChronoDuration::days(1);
    let neighborhood_end = day_end_utc + ChronoDuration::days(1);
    let existing_all = list_appointments_in_range(
        conn,
        neighborhood_start.timestamp(),
        neighborhood_end.timestamp(),
    )?;
    let existing: Vec<&Appointment> = existing_all
        .iter()
        .filter(|a| a.status == "booked")
        .collect();

    let buffer_secs = config.buffer_minutes as i64 * 60;
    let duration_secs = duration_minutes as i64 * 60;
    let step_secs = step_minutes as i64 * 60;
    // Don't quote slots that have already started. Without this, the
    // bot offers "today at 9am" at 2pm, the customer accepts, and
    // the book tool then refuses with a past-date error — confusing
    // for everyone. We use the same 5-min grace as `book_appointment`
    // so the two checks stay symmetric.
    let earliest_start = now_unix() - 5 * 60;

    let mut slots = Vec::new();
    for (win_start, win_end) in windows {
        let win_start_utc = local_date_to_utc(tz, date, win_start);
        let win_end_utc = local_date_to_utc(tz, date, win_end);
        let mut cursor = win_start_utc.timestamp();
        let win_end_unix = win_end_utc.timestamp();
        while cursor + duration_secs <= win_end_unix {
            let candidate_end = cursor + duration_secs;
            if cursor < earliest_start {
                cursor += step_secs;
                continue;
            }
            let conflicts = existing.iter().any(|a| {
                let other_start = a.start_time - buffer_secs;
                let other_end = a.start_time + (a.duration_minutes as i64 * 60) + buffer_secs;
                cursor < other_end && other_start < candidate_end
            });
            if !conflicts {
                slots.push(Slot {
                    start_unix: cursor,
                    end_unix: candidate_end,
                });
            }
            cursor += step_secs;
        }
    }
    Ok(slots)
}

/// Convert a (`tz`-local) date+time to a UTC `DateTime<Utc>`. Picks the
/// earliest interpretation if the local time is ambiguous (DST fall-back)
/// or skipped (DST spring-forward) — pragmatic default for booking
/// scheduling, where we just need *a* deterministic UTC value.
fn local_date_to_utc(tz: Tz, date: NaiveDate, time: NaiveTime) -> DateTime<Utc> {
    let local = date.and_time(time);
    let local_dt = match tz.from_local_datetime(&local) {
        chrono::LocalResult::Single(dt) => dt,
        chrono::LocalResult::Ambiguous(dt, _) => dt,
        chrono::LocalResult::None => {
            // Spring-forward gap: nudge forward by 1h until we land in
            // a valid local instant. Loop is bounded — gaps are at
            // most a few hours.
            let mut t = local;
            for _ in 0..6 {
                t += ChronoDuration::hours(1);
                if let chrono::LocalResult::Single(dt) = tz.from_local_datetime(&t) {
                    return dt.with_timezone(&Utc);
                }
            }
            return Utc.from_utc_datetime(&local);
        }
    };
    local_dt.with_timezone(&Utc)
}

/// Today's calendar date in the calendar's configured timezone (or
/// host-local if the config field is empty). Use this anywhere the LLM
/// or the user needs "today" — `Utc::now().date_naive()` is wrong in
/// any non-UTC zone around the midnight rollover.
pub fn today_in_config_tz(config: &CalendarConfig) -> NaiveDate {
    let tz = resolve_tz(&config.timezone);
    Utc::now().with_timezone(&tz).date_naive()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE services (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                duration_minutes INTEGER NOT NULL CHECK(duration_minutes > 0),
                description TEXT,
                active INTEGER NOT NULL DEFAULT 1,
                sort_order INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                requires_address INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE appointments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                service_id INTEGER REFERENCES services(id) ON DELETE SET NULL,
                service_name TEXT NOT NULL,
                start_time INTEGER NOT NULL,
                duration_minutes INTEGER NOT NULL CHECK(duration_minutes > 0),
                customer_phone TEXT,
                customer_name TEXT,
                notes TEXT,
                status TEXT NOT NULL DEFAULT 'booked'
                    CHECK(status IN ('booked','cancelled','completed')),
                source TEXT NOT NULL DEFAULT 'manual'
                    CHECK(source IN ('sms','call','manual')),
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn cfg_with_hours(hours: BusinessHours) -> CalendarConfig {
        CalendarConfig {
            enabled: true,
            timezone: "UTC".to_string(),
            buffer_minutes: 0,
            business_hours: hours,
        }
    }

    /// Test anchor — the next upcoming Monday relative to the system
    /// clock. The earlier tests pinned to a hardcoded 2026-05-04 which
    /// turned the entire suite stale 24 h after that date passed.
    /// A far-future fixed date (2099) is always-future for
    /// `book_appointment` but falls outside the 1-year lookup window
    /// `MyAppointments` uses, so we instead anchor to "next Monday."
    /// That's always future (so `book_appointment`'s past-time guard
    /// is happy), within the MyAppointments window, and a real calendar
    /// Monday so business-hours/lunch tests still exercise weekday
    /// logic. The day-of-the-week itself is what matters; the absolute
    /// date sliding forward as the test runs is harmless.
    fn future_monday_date() -> NaiveDate {
        let today = Utc::now().date_naive();
        let days_until = (7 - today.weekday().num_days_from_monday()) % 7;
        let days_to_add = if days_until == 0 { 7 } else { days_until };
        today + ChronoDuration::days(days_to_add as i64)
    }

    fn future_monday_unix(hour: u32) -> i64 {
        let date = future_monday_date();
        Utc.from_utc_datetime(&date.and_hms_opt(hour, 0, 0).expect("valid hour"))
            .timestamp()
    }

    #[test]
    fn services_round_trip() {
        let conn = fresh_db();
        let id = create_service(
            &conn,
            ServiceInput {
                name: "Lawn mow".into(),
                duration_minutes: 30,
                description: Some("standard 30-min mow".into()),
                active: true,
                sort_order: 0,
                requires_address: false,
            },
        )
        .unwrap();
        let svc = get_service(&conn, id).unwrap().unwrap();
        assert_eq!(svc.name, "Lawn mow");
        assert_eq!(svc.duration_minutes, 30);
        assert!(svc.active);
    }

    #[test]
    fn deactivated_service_filtered_by_only_active() {
        let conn = fresh_db();
        let active_id = create_service(
            &conn,
            ServiceInput {
                name: "A".into(),
                duration_minutes: 30,
                description: None,
                active: true,
                sort_order: 0,
                requires_address: false,
            },
        )
        .unwrap();
        let inactive_id = create_service(
            &conn,
            ServiceInput {
                name: "B".into(),
                duration_minutes: 30,
                description: None,
                active: true,
                sort_order: 0,
                requires_address: false,
            },
        )
        .unwrap();
        deactivate_service(&conn, inactive_id).unwrap();
        let active = list_services(&conn, true).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, active_id);
        let all = list_services(&conn, false).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn book_then_conflict_then_cancel_then_rebook() {
        let conn = fresh_db();
        // Far-future Monday 10:00 UTC, 60-min slot — see
        // `future_monday_unix` for the rationale.
        let start = future_monday_unix(10);
        let id = book_appointment(
            &conn,
            AppointmentInput {
                service_id: None,
                service_name: "Mow".into(),
                start_time: start,
                duration_minutes: 60,
                customer_phone: Some("+61432602110".into()),
                customer_name: Some("Lance".into()),
                notes: None,
                source: "manual".into(),
            },
            0,
        )
        .unwrap();
        // Overlapping booking same-time = conflict.
        let err = book_appointment(
            &conn,
            AppointmentInput {
                service_id: None,
                service_name: "Mow".into(),
                start_time: start + 30 * 60,
                duration_minutes: 60,
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
            },
            0,
        );
        assert!(matches!(err, Err(BookingError::Conflict(_))));
        // Cancelling it frees the slot.
        cancel_appointment(&conn, id).unwrap();
        let id2 = book_appointment(
            &conn,
            AppointmentInput {
                service_id: None,
                service_name: "Mow".into(),
                start_time: start + 30 * 60,
                duration_minutes: 60,
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
            },
            0,
        )
        .unwrap();
        assert_ne!(id, id2);
    }

    #[test]
    fn buffer_blocks_immediately_adjacent_bookings() {
        let conn = fresh_db();
        let start = future_monday_unix(10);
        // First booking: 10:00–11:00.
        book_appointment(
            &conn,
            AppointmentInput {
                service_id: None,
                service_name: "A".into(),
                start_time: start,
                duration_minutes: 60,
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
            },
            0,
        )
        .unwrap();
        // Second booking exactly back-to-back: 11:00–12:00 with a
        // 15-min buffer should now conflict.
        let err = book_appointment(
            &conn,
            AppointmentInput {
                service_id: None,
                service_name: "B".into(),
                start_time: start + 60 * 60,
                duration_minutes: 60,
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
            },
            15,
        );
        assert!(matches!(err, Err(BookingError::Conflict(_))));
    }

    #[test]
    fn available_slots_respects_business_hours_and_lunch() {
        let conn = fresh_db();
        // Mon 09:00–17:00 with 12:00–13:00 lunch.
        let mut hours = default_business_hours();
        hours.monday.open = true;
        hours.monday.start = "09:00".into();
        hours.monday.end = "17:00".into();
        hours.monday.lunch = Some(TimeRange {
            start: "12:00".into(),
            end: "13:00".into(),
        });
        let cfg = cfg_with_hours(hours);
        let date = future_monday_date();
        let slots = available_slots(&conn, &cfg, date, 60, 60).unwrap();
        // 09–12 = 3 slots; 13–17 = 4 slots; total 7.
        assert_eq!(slots.len(), 7);
        // No slot can start during lunch.
        for s in &slots {
            let start_dt = Utc.timestamp_opt(s.start_unix, 0).single().unwrap();
            let hour = start_dt.with_timezone(&Tz::UTC).format("%H").to_string();
            assert_ne!(hour, "12", "no slot at 12:00 (lunch)");
        }
    }

    #[test]
    fn available_slots_drops_collisions_with_existing_bookings() {
        let conn = fresh_db();
        let mut hours = default_business_hours();
        hours.monday.lunch = None; // simplify
        let cfg = cfg_with_hours(hours);
        let date = future_monday_date();
        // Manually book 10:00–11:00 UTC.
        let ten_am_utc =
            local_date_to_utc(Tz::UTC, date, NaiveTime::from_hms_opt(10, 0, 0).unwrap())
                .timestamp();
        book_appointment(
            &conn,
            AppointmentInput {
                service_id: None,
                service_name: "Existing".into(),
                start_time: ten_am_utc,
                duration_minutes: 60,
                customer_phone: None,
                customer_name: None,
                notes: None,
                source: "manual".into(),
            },
            0,
        )
        .unwrap();
        let slots = available_slots(&conn, &cfg, date, 60, 60).unwrap();
        // 09, 11, 12, 13, 14, 15, 16 — should be 7 free hourly slots.
        assert_eq!(slots.len(), 7);
        assert!(!slots.iter().any(|s| s.start_unix == ten_am_utc));
    }

    #[test]
    fn closed_day_yields_no_slots() {
        let conn = fresh_db();
        let mut hours = default_business_hours();
        hours.sunday.open = false;
        let cfg = cfg_with_hours(hours);
        let sunday = NaiveDate::from_ymd_opt(2026, 5, 3).unwrap();
        assert_eq!(sunday.weekday(), Weekday::Sun);
        let slots = available_slots(&conn, &cfg, sunday, 60, 60).unwrap();
        assert!(slots.is_empty());
    }
}
