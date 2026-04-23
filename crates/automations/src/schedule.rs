use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use sunrise::{Coordinates, SolarDay, SolarEvent};

use crate::types::{Trigger, TriggerContext};

pub(crate) fn next_schedule_time(
    trigger: &Trigger,
    after: DateTime<Utc>,
    trigger_context: TriggerContext,
) -> Option<DateTime<Utc>> {
    match trigger {
        Trigger::WallClock { hour, minute } => {
            next_wall_clock_occurrence(*hour, *minute, after, trigger_context)
        }
        Trigger::Cron { schedule, .. } => schedule.after(&after).next(),
        Trigger::Sunrise { offset_mins } => {
            next_solar_occurrence(after, trigger_context, true, *offset_mins)
        }
        Trigger::Sunset { offset_mins } => {
            next_solar_occurrence(after, trigger_context, false, *offset_mins)
        }
        _ => None,
    }
}

fn next_solar_occurrence(
    after: DateTime<Utc>,
    trigger_context: TriggerContext,
    sunrise: bool,
    offset_mins: i64,
) -> Option<DateTime<Utc>> {
    let (latitude, longitude) = (trigger_context.latitude?, trigger_context.longitude?);

    for day_offset in 0..=366 {
        let date = after
            .date_naive()
            .checked_add_signed(ChronoDuration::days(day_offset))?;
        let event_time = solar_event_time(date, latitude, longitude, sunrise, offset_mins)?;
        if event_time > after {
            return Some(event_time);
        }
    }

    None
}

pub(crate) fn solar_event_time(
    date: NaiveDate,
    latitude: f64,
    longitude: f64,
    sunrise: bool,
    offset_mins: i64,
) -> Option<DateTime<Utc>> {
    let coordinates = Coordinates::new(latitude, longitude)?;
    let solar_day = SolarDay::new(coordinates, date);
    let base = solar_day.event_time(if sunrise {
        SolarEvent::Sunrise
    } else {
        SolarEvent::Sunset
    });
    Some(base + ChronoDuration::minutes(offset_mins))
}

pub(crate) fn next_wall_clock_occurrence(
    hour: u32,
    minute: u32,
    after: DateTime<Utc>,
    trigger_context: TriggerContext,
) -> Option<DateTime<Utc>> {
    let timezone = trigger_context.timezone.unwrap_or(Tz::UTC);
    let local_after = after.with_timezone(&timezone);
    let scheduled_today = local_after.date_naive().and_hms_opt(hour, minute, 0)?;
    let scheduled_today = timezone.from_local_datetime(&scheduled_today).single()?;

    if scheduled_today.with_timezone(&Utc) > after {
        return Some(scheduled_today.with_timezone(&Utc));
    }

    let next_day = local_after
        .date_naive()
        .checked_add_signed(ChronoDuration::days(1))?;
    let scheduled_next = next_day.and_hms_opt(hour, minute, 0)?;
    Some(
        timezone
            .from_local_datetime(&scheduled_next)
            .single()?
            .with_timezone(&Utc),
    )
}
