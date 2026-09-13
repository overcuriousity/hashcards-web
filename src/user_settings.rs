// Copyright 2025 Fernando Borretti
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! One user's settings: the layer between the instance's `[defaults]` and a
//! collection's own `.hashcards.toml`.
//!
//! Stored as rows in the `meta` table of the user's review database, one row
//! per setting rather than one blob, so that a single unreadable value
//! cannot cost the others.

use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::error::Fallible;
use crate::types::free_days::FreeDays;
use crate::types::limits::DailyLimits;
use crate::types::performance::DesiredRetention;
use crate::types::performance::Jitter;
use crate::types::performance::MaxInterval;

pub const KEY_RETENTION: &str = "setting.desired_retention";
pub const KEY_MAX_INTERVAL: &str = "setting.max_interval_days";
pub const KEY_JITTER: &str = "setting.jitter";
pub const KEY_BURY_SIBLINGS: &str = "setting.bury_siblings";
pub const KEY_MAX_REVIEWS: &str = "setting.max_reviews_per_day";
pub const KEY_MAX_NEW: &str = "setting.max_new_per_day";
pub const KEY_FREE_DAYS: &str = "setting.free_days";

/// What one user asks for, in place of the instance's settings.
///
/// Every field optional, `None` meaning "inherit". A user who has never
/// opened the settings page is scheduled exactly as they were before this
/// layer existed.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct UserSettings {
    pub retention: Option<DesiredRetention>,
    pub max_interval: Option<MaxInterval>,
    pub jitter: Option<Jitter>,
    pub bury_siblings: Option<bool>,
    pub limits: DailyLimits,
    pub free_days: Option<FreeDays>,
}

/// One warning per unreadable value, naming the key, so that a setting
/// silently inheriting is explicable from the log.
///
/// A function rather than a closure: it is used at four different types,
/// and a closure is inferred at exactly one.
fn lenient<T>(key: &str, parsed: Fallible<T>) -> Option<T> {
    match parsed {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("Ignoring {key}: {e}");
            None
        }
    }
}

/// Read every setting, forgiving every one of them.
///
/// A free function taking a `&Connection`: the caller holds the lock, which
/// is not reentrant.
pub fn read_settings(conn: &Connection) -> UserSettings {
    let get = |key: &str| -> Option<String> {
        conn.query_row(
            "select value from meta where key = ?;",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .unwrap_or(None)
    };
    let number = |key: &str| -> Option<f64> {
        let raw = get(key)?;
        match raw.trim().parse::<f64>() {
            Ok(n) => Some(n),
            Err(e) => {
                log::warn!("Ignoring {key}: {raw} is not a number ({e})");
                None
            }
        }
    };

    UserSettings {
        retention: number(KEY_RETENTION)
            .and_then(|n| lenient(KEY_RETENTION, DesiredRetention::new(n))),
        max_interval: number(KEY_MAX_INTERVAL)
            .and_then(|n| lenient(KEY_MAX_INTERVAL, MaxInterval::new(n))),
        jitter: number(KEY_JITTER).and_then(|n| lenient(KEY_JITTER, Jitter::new(n))),
        bury_siblings: get(KEY_BURY_SIBLINGS).and_then(|v| match v.trim() {
            "true" => Some(true),
            "false" => Some(false),
            other => {
                log::warn!("Ignoring {KEY_BURY_SIBLINGS}: {other} is not true or false");
                None
            }
        }),
        limits: DailyLimits {
            reviews: get(KEY_MAX_REVIEWS)
                .and_then(|v| lenient(KEY_MAX_REVIEWS, DailyLimits::parse(&v)))
                .flatten(),
            new: get(KEY_MAX_NEW)
                .and_then(|v| lenient(KEY_MAX_NEW, DailyLimits::parse(&v)))
                .flatten(),
        },
        free_days: get(KEY_FREE_DAYS)
            .and_then(|v| lenient(KEY_FREE_DAYS, FreeDays::parse_list(&v))),
    }
}

/// Write every setting, deleting the ones that are `None` so that clearing a
/// field really does return it to the inherited value.
///
/// A free function taking a `&mut Connection`, for the same reason as above.
/// One transaction, so a half-saved settings page is not a state anyone can
/// observe.
pub fn write_settings(conn: &mut Connection, settings: &UserSettings) -> Fallible<()> {
    let tx = conn.transaction()?;
    {
        let mut put = |key: &str, value: Option<String>| -> Fallible<()> {
            match value {
                Some(v) => {
                    tx.execute(
                        "insert into meta (key, value) values (?, ?) \
                         on conflict (key) do update set value = excluded.value;",
                        params![key, v],
                    )?;
                }
                None => {
                    tx.execute("delete from meta where key = ?;", params![key])?;
                }
            }
            Ok(())
        };
        put(
            KEY_RETENTION,
            settings.retention.map(|v| v.into_inner().to_string()),
        )?;
        put(
            KEY_MAX_INTERVAL,
            settings.max_interval.map(|v| v.into_inner().to_string()),
        )?;
        put(
            KEY_JITTER,
            settings.jitter.map(|v| v.into_inner().to_string()),
        )?;
        put(
            KEY_BURY_SIBLINGS,
            settings.bury_siblings.map(|v| v.to_string()),
        )?;
        put(
            KEY_MAX_REVIEWS,
            settings.limits.reviews.map(|v| v.to_string()),
        )?;
        put(KEY_MAX_NEW, settings.limits.new.map(|v| v.to_string()))?;
        put(
            KEY_FREE_DAYS,
            settings.free_days.map(|v| v.to_list().join(",")),
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_db::UserDatabase;

    #[test]
    fn a_fresh_database_inherits_everything() -> Fallible<()> {
        let db = UserDatabase::memory()?;
        assert_eq!(db.user_settings(), UserSettings::default());
        Ok(())
    }

    #[test]
    fn settings_round_trip() -> Fallible<()> {
        let db = UserDatabase::memory()?;
        let settings = UserSettings {
            retention: Some(DesiredRetention::new(0.85)?),
            max_interval: Some(MaxInterval::new(365.0)?),
            jitter: Some(Jitter::new(0.1)?),
            bury_siblings: Some(false),
            limits: DailyLimits {
                reviews: Some(40),
                new: Some(0),
            },
            free_days: Some(FreeDays::parse_list("sat,sun")?),
        };
        db.save_user_settings(&settings)?;
        assert_eq!(db.user_settings(), settings);
        Ok(())
    }

    /// Saving `None` clears the row, so the value inherits again rather than
    /// keeping whatever was there before. Otherwise raising the server's
    /// default would silently skip everyone who had ever opened the page.
    #[test]
    fn clearing_a_setting_removes_it() -> Fallible<()> {
        let db = UserDatabase::memory()?;
        db.save_user_settings(&UserSettings {
            retention: Some(DesiredRetention::new(0.85)?),
            ..UserSettings::default()
        })?;
        db.save_user_settings(&UserSettings::default())?;
        assert_eq!(db.user_settings(), UserSettings::default());
        Ok(())
    }

    /// A `meta` row nothing can parse costs that one setting and nothing
    /// else. The same leniency `collection_overrides` applies, for the same
    /// reason: a preference is not worth failing a page over.
    #[test]
    fn junk_costs_only_its_own_setting() -> Fallible<()> {
        let db = UserDatabase::memory()?;
        db.save_user_settings(&UserSettings {
            max_interval: Some(MaxInterval::new(365.0)?),
            ..UserSettings::default()
        })?;
        db.put_meta_for_test(KEY_RETENTION, "banana")?;
        db.put_meta_for_test(KEY_FREE_DAYS, "caturday")?;

        let loaded = db.user_settings();
        assert_eq!(loaded.retention, None, "junk inherits");
        assert_eq!(loaded.free_days, None, "junk inherits");
        assert_eq!(
            loaded.max_interval,
            Some(MaxInterval::new(365.0)?),
            "its neighbour survives"
        );
        Ok(())
    }

    /// Out of range is junk too: the newtype refuses it on the way in, and a
    /// value that got there another way must not get past on the way out.
    #[test]
    fn an_out_of_range_value_inherits() -> Fallible<()> {
        let db = UserDatabase::memory()?;
        db.put_meta_for_test(KEY_RETENTION, "2.5")?;
        assert_eq!(db.user_settings().retention, None);
        Ok(())
    }
}
