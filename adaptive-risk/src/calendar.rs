use chrono::{DateTime, Days, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use color_eyre::eyre::{Result, ensure, eyre};
use serde::{Deserialize, Serialize};

/// Half-open [start, end) trading day. Ambiguous/nonexistent reset times fail closed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TradingCalendar {
    pub timezone: Tz,
    pub reset_time: NaiveTime,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradingDay {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}
impl Default for TradingCalendar {
    fn default() -> Self {
        Self {
            timezone: chrono_tz::America::New_York,
            reset_time: NaiveTime::from_hms_opt(17, 0, 0).unwrap_or_default(),
        }
    }
}
impl TradingCalendar {
    pub fn validate(&self) -> Result<()> {
        self.day_at(DateTime::UNIX_EPOCH).map(|_| ())
    }
    pub fn day_at(&self, now: DateTime<Utc>) -> Result<TradingDay> {
        let local = now.with_timezone(&self.timezone);
        let date = if local.time() < self.reset_time {
            local.date_naive().checked_sub_days(Days::new(1))
        } else {
            Some(local.date_naive())
        }
        .ok_or_else(|| eyre!("trading day overflow"))?;
        let next = date
            .checked_add_days(Days::new(1))
            .ok_or_else(|| eyre!("trading day overflow"))?;
        let boundary = |date: chrono::NaiveDate| {
            self.timezone
                .from_local_datetime(&date.and_time(self.reset_time))
                .single()
                .map(|v| v.with_timezone(&Utc))
                .ok_or_else(|| eyre!("ambiguous or nonexistent daily reset"))
        };
        let day = TradingDay {
            start: boundary(date)?,
            end: boundary(next)?,
        };
        ensure!(day.start <= now && now < day.end, "invalid trading day");
        Ok(day)
    }
}
