use crate::{gml::Value, math::Real};
#[cfg(not(target_arch = "wasm32"))]
use time::UtcOffset;
use time::{OffsetDateTime, PrimitiveDateTime};

/// Sleep for T minus 1 millisecond, and busywait for the rest of the duration.
#[cfg(not(target_arch = "wasm32"))]
pub fn sleep(dur: std::time::Duration) {
    // TODO: find a more precise way to sleep?
    let begin = std::time::Instant::now();
    if let Some(sleep_time) = dur.checked_sub(std::time::Duration::from_millis(1)) {
        std::thread::sleep(sleep_time);
    }
    while std::time::Instant::now() < begin + dur {}
}

/// Neither half of the native version exists on wasm32-unknown-unknown: `Instant::now()` panics
/// ("time not implemented on this platform") and `thread::sleep` has no thread to sleep, so a game
/// calling GML's `sleep()` would take the whole run down with it.
///
/// GML's `sleep` is defined as blocking the game, so busy-wait on the monotonic clock. The game is
/// frozen for the duration -- which is exactly what the game asked for, and what GM8 itself does.
#[cfg(target_arch = "wasm32")]
pub fn sleep(dur: std::time::Duration) {
    // Held across the loop both to keep it cheap and because a clock we can't read is a loop that
    // would never end -- skip the wait entirely rather than hang the tab on it.
    let Some(perf) = crate::dom::performance() else { return };
    let end = perf.now() + dur.as_secs_f64() * 1000.0;
    while perf.now() < end {}
}

fn epoch() -> PrimitiveDateTime {
    time::macros::date!(1899 - 12 - 30).midnight()
}

#[cfg(not(target_arch = "wasm32"))]
fn now() -> time::OffsetDateTime {
    OffsetDateTime::now_utc()
        + time::Duration::seconds(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC).whole_seconds().into())
}

/// Local wall-clock time, from JS.
///
/// Two things rule out the native path here. The `time` crate's `local-offset` feature (which reads
/// the OS timezone) isn't available on wasm32, and `OffsetDateTime::now_utc()` goes through
/// `SystemTime::now()`, which *panics* on wasm32-unknown-unknown. So both halves come from JS:
/// `Date.getTime()` for the instant and `Date.getTimezoneOffset()` for the zone.
///
/// The offset is `UTC - local` in minutes and is evaluated for that exact instant, so DST is
/// already accounted for; subtracting it gives local wall-clock in a nominally-UTC value, which is
/// the same shape the native path produces (a UTC `OffsetDateTime` shifted by the local offset).
/// Every caller immediately drops the offset into a `PrimitiveDateTime` anyway.
#[cfg(target_arch = "wasm32")]
fn now() -> time::OffsetDateTime {
    let date = js_sys::Date::new_0();
    let local_ms = date.get_time() - date.get_timezone_offset() * 60_000.0;
    OffsetDateTime::from_unix_timestamp_nanos((local_ms * 1_000_000.0) as i128).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

pub fn now_as_nanos() -> u128 {
    let datetime = now();
    datetime.unix_timestamp_nanos().try_into().unwrap_or(0)
}

/// Milliseconds off a monotonic clock, for GM's `current_time` (Win32 `GetTickCount()`, i.e. time
/// since boot -- games only ever difference it, so any fixed origin will do).
#[cfg(target_arch = "wasm32")]
fn monotonic_ms() -> f64 {
    // `performance.now()` counts from this document's navigation, and each game load gets a fresh
    // iframe, so it is effectively time since the game started. Unlike `Date.now()` it can't be
    // dragged backwards by an NTP correction mid-run. A worker's own clock shares the document's
    // origin, so it reads the same timeline there.
    crate::dom::performance().map(|p| p.now()).unwrap_or(0.0)
}

#[cfg(not(target_arch = "wasm32"))]
fn monotonic_ms() -> f64 {
    // Only reachable through `GameClock::Realtime`, which native never selects (it has a real
    // `Instant` to hand and uses `GameClock::StartupEpoch`) -- present so the enum stays portable.
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1000.0
}

/// `monotonic_ms` as GM sees it: a `u32` that wraps rather than saturating, like `GetTickCount()`.
pub fn tick_count() -> u32 {
    monotonic_ms() as u64 as u32
}

/// The same monotonic clock in microseconds, for GM8.2's `get_timer()`/`get_delta()`
/// (`gm82core.dll`'s high-resolution timer, originally `QueryPerformanceCounter`). Kept as `f64`
/// rather than wrapped into an integer: games difference it to build a `delta_time`, and
/// `performance.now()` carries sub-millisecond precision worth passing through.
pub fn monotonic_micros() -> f64 {
    monotonic_ms() * 1000.0
}

fn i32_to_month(m: i32) -> Option<time::Month> {
    use time::Month::*;
    Some(match m {
        1 => January,
        2 => February,
        3 => March,
        4 => April,
        5 => May,
        6 => June,
        7 => July,
        8 => August,
        9 => September,
        10 => October,
        11 => November,
        12 => December,
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateTime(PrimitiveDateTime);

impl DateTime {
    pub fn now() -> Self {
        let dt = now();
        Self(dt.date().with_time(dt.time()))
    }

    pub fn from_nanos(nanos: u128) -> Self {
        // manual unix timestamp because we need a PrimitiveDateTime
        // also split into seconds and nanos because time::Duration::nanoseconds takes an i64
        Self(
            time::macros::date!(1970 - 1 - 1).midnight()
                + time::Duration::new((nanos / 1_000_000_000) as _, (nanos % 1_000_000_000) as _),
        )
    }

    pub fn date(&self) -> Self {
        Self(self.0.date().midnight())
    }

    pub fn time(&self) -> Self {
        Self(epoch().date().with_time(self.0.time()))
    }

    pub fn from_ymd(y: i32, m: i32, d: i32) -> Option<Self> {
        // GM doesn't support BCE so we won't either
        time::Date::from_calendar_date(y, i32_to_month(m)?, d as _).ok().map(|d| Self(d.midnight()))
    }

    pub fn from_hms(h: i32, m: i32, s: i32) -> Option<Self> {
        epoch().date().with_hms(h as _, m as _, s as _).ok().map(|dt| Self(dt))
    }

    pub fn from_ymdhms(y: i32, mo: i32, d: i32, h: i32, mi: i32, s: i32) -> Option<Self> {
        time::Date::from_calendar_date(y, i32_to_month(mo)?, d as _)
            .and_then(|d| d.with_hms(h as _, mi as _, s as _))
            .ok()
            .map(Self)
    }

    pub fn year(&self) -> i32 {
        self.0.date().year()
    }

    pub fn month(&self) -> u32 {
        (self.0.date().month() as u8).into()
    }

    pub fn day(&self) -> u32 {
        self.0.date().day().into()
    }

    pub fn day_of_year(&self) -> u32 {
        self.0.date().ordinal().into()
    }

    pub fn hour_of_year(&self) -> u32 {
        (self.day_of_year() - 1) * 24 + u32::from(self.0.time().hour())
    }

    pub fn minute_of_year(&self) -> u32 {
        self.hour_of_year() * 60 + u32::from(self.0.time().minute())
    }

    pub fn second_of_year(&self) -> u32 {
        self.minute_of_year() * 60 + u32::from(self.0.time().second())
    }

    pub fn hour(&self) -> u32 {
        self.0.time().hour().into()
    }

    pub fn minute(&self) -> u32 {
        self.0.time().minute().into()
    }

    pub fn second(&self) -> u32 {
        self.0.time().second().into()
    }

    pub fn week(&self) -> u32 {
        self.0.iso_week().into()
    }

    pub fn weekday(&self) -> u32 {
        self.0.weekday().number_from_sunday().into()
    }

    pub fn days_in_month(&self) -> u32 {
        time::util::days_in_month(self.0.month(), self.0.year()).into()
    }

    pub fn days_in_year(&self) -> u32 {
        time::util::days_in_year(self.0.year()).into()
    }

    pub fn is_leap_year(&self) -> bool {
        time::util::is_leap_year(self.0.year())
    }

    /// Adds whole calendar months, clamping the day of the month to the target month's length: one
    /// month after 2020-01-31 is 2020-02-29, not 2020-03-02. That's Delphi's `IncMonth`, which is
    /// what GM8's `date_inc_month` (and `date_inc_year`, which is `IncMonth` by twelves) calls.
    /// Time of day is untouched.
    ///
    /// `None` if the result falls outside the representable year range.
    pub fn add_months(&self, months: i32) -> Option<Self> {
        let total = i64::from(self.0.year()) * 12 + i64::from(self.0.month() as u8 - 1) + i64::from(months);
        let year = i32::try_from(total.div_euclid(12)).ok()?;
        let month = i32_to_month((total.rem_euclid(12) + 1) as i32)?;
        let day = self.0.day().min(time::util::days_in_month(month, year));
        time::Date::from_calendar_date(year, month, day).ok().map(|d| Self(d.with_time(self.0.time())))
    }

    /// GM8 renders dates with the OS locale's *short date* pattern and times with its *long time*
    /// pattern -- it hands both straight to Delphi's `DateToStr`/`TimeToStr`, which read the
    /// Windows regional settings. In the browser the equivalent authority is the page's own locale,
    /// and `toLocaleDateString`/`toLocaleTimeString` with default options produce exactly those two
    /// patterns ("8/5/2026" + "2:03:00 PM" under en-US, "05/08/2026" + "14:03:00" under fr-FR).
    ///
    /// The "default" locale tag is a well-formed language subtag that matches nothing, so `Intl`
    /// falls back to the environment's own locale -- the point of passing it.
    #[cfg(target_arch = "wasm32")]
    fn as_js_date(&self) -> js_sys::Date {
        let date = js_sys::Date::new_with_year_month_day_hr_min_sec(
            self.year() as u32,
            self.month() as i32 - 1, // JS months are 0-based
            self.day() as i32,
            self.hour() as i32,
            self.minute() as i32,
            self.second() as i32,
        );
        // `new Date(yy, ...)` maps years 0..99 to 1900+yy; writing the year back undoes that.
        date.set_full_year(self.year() as u32);
        date
    }

    #[cfg(target_arch = "wasm32")]
    pub fn date_string(&self) -> String {
        self.as_js_date().to_locale_date_string("default", &wasm_bindgen::JsValue::UNDEFINED).into()
    }

    #[cfg(target_arch = "wasm32")]
    pub fn time_string(&self) -> String {
        self.as_js_date().to_locale_time_string("default").into()
    }

    // Native has no locale to read without pulling in an i18n dependency, and isn't the target this
    // engine actually ships to; ISO-ish fixed patterns keep the functions honest there.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn date_string(&self) -> String {
        format!("{:02}/{:02}/{:04}", self.month(), self.day(), self.year())
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn time_string(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour(), self.minute(), self.second())
    }
}

impl From<DateTime> for Real {
    fn from(dt: DateTime) -> Self {
        // calculate the ipart and fpart separately for maybe better precision?
        let ipart = Real::from((dt.0 - epoch()).whole_days() as f64);
        let fpart = Real::from((dt.time().0 - epoch()).whole_milliseconds() as f64) / Real::from(86400000);
        // the time part is the abs(fract()) of the datetime so that part increases backwards before the epoch
        if dt.0 >= epoch() { ipart + fpart } else { ipart - 1.into() - fpart }
    }
}

impl From<DateTime> for Value {
    fn from(dt: DateTime) -> Self {
        Real::from(dt).into()
    }
}

impl From<Real> for DateTime {
    fn from(dt: Real) -> Self {
        let days = time::Duration::days(dt.trunc().to_i32().into());
        // Round, not floor. The fractional part of a datetime has already given up bits to the day
        // count -- around 2026 an f64 resolves it to about a microsecond -- so 13:45:30 comes back
        // out as 49529999.9994ms about as often as 49530000.0006ms, and flooring dropped a whole
        // second on half of those. Delphi's `DecodeTime`, which is what GM8 runs here, rounds for
        // exactly this reason. Sign is left on the value so the negative-side correction below
        // still sees a negative `ms` (f64::round is away-from-zero, so it never crosses zero).
        let ms = time::Duration::milliseconds((dt.fract().into_inner() * 86_400_000.0).round() as i64);
        // negate the time (see the inverse function for explanation)
        Self(epoch() + days + if dt > 0.into() { ms } else { -ms })
    }
}
