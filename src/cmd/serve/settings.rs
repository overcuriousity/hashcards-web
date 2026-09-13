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

//! The per-user settings page.
//!
//! Everything here edits the *user* layer: the instance's `[defaults]` sit
//! under it and a collection's `.hashcards.toml` sits over it, so a blank
//! field is not an empty value but an inherited one.

use std::collections::HashMap;
use std::path::Path;

use axum::Form;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use maud::Markup;
use maud::html;
use serde::Deserialize;

use crate::cmd::drill::template::page_template;
use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::cards::CardRoot;
use crate::cmd::serve::cards::user_db_path;
use crate::cmd::serve::reviewdb::refuse_if_unconsolidated;
use crate::cmd::serve::state::AppState;
use crate::error::Fallible;
use crate::error::fail;
use crate::flash::Flash;
use crate::types::free_days::FreeDays;
use crate::types::limits::DailyLimits;
use crate::types::performance::DesiredRetention;
use crate::types::performance::Jitter;
use crate::types::performance::MaxInterval;
use crate::types::performance::Scheduling;
use crate::user_db::UserDatabase;
use crate::user_settings::UserSettings;

/// Every control on the page, as the browser submits it.
///
/// Strings throughout: the parse and its error message belong to
/// `settings_from_form`, which can say what the range is, not to serde,
/// which cannot. A checkbox absent from a submission is an unticked box, so
/// each is an `Option<String>` that is `None` when not ticked.
#[derive(Deserialize, Default)]
pub struct SettingsForm {
    #[serde(default)]
    pub desired_retention: String,
    #[serde(default)]
    pub max_interval_days: String,
    #[serde(default)]
    pub jitter: String,
    #[serde(default)]
    pub bury_siblings: Option<String>,
    #[serde(default)]
    pub max_reviews_per_day: String,
    #[serde(default)]
    pub max_new_per_day: String,
    #[serde(default)]
    pub free_mon: Option<String>,
    #[serde(default)]
    pub free_tue: Option<String>,
    #[serde(default)]
    pub free_wed: Option<String>,
    #[serde(default)]
    pub free_thu: Option<String>,
    #[serde(default)]
    pub free_fri: Option<String>,
    #[serde(default)]
    pub free_sat: Option<String>,
    #[serde(default)]
    pub free_sun: Option<String>,
}

/// A blank box is "inherit"; anything else is validated by the newtype that
/// owns the range. Those error messages already name their bounds and are
/// written to be read, so they are returned as they are rather than wrapped
/// in a second sentence.
fn optional_number<T>(
    raw: &str,
    what: &str,
    make: impl Fn(f64) -> Fallible<T>,
) -> Fallible<Option<T>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<f64>() {
        Ok(n) => make(n).map(Some),
        Err(_) => fail(format!("{what} must be a number, got: {raw}")),
    }
}

pub fn settings_from_form(form: &SettingsForm) -> Fallible<UserSettings> {
    let ticked = |box_: &Option<String>| box_.is_some();
    let free_days = [
        ticked(&form.free_mon),
        ticked(&form.free_tue),
        ticked(&form.free_wed),
        ticked(&form.free_thu),
        ticked(&form.free_fri),
        ticked(&form.free_sat),
        ticked(&form.free_sun),
    ];
    Ok(UserSettings {
        retention: optional_number(
            &form.desired_retention,
            "desired retention",
            DesiredRetention::new,
        )?,
        max_interval: optional_number(
            &form.max_interval_days,
            "the maximum interval",
            MaxInterval::new,
        )?,
        jitter: optional_number(&form.jitter, "interval jitter", Jitter::new)?,
        // A checkbox absent from a submission is an unticked box, not a
        // missing answer, so this is never `None` from this form. The
        // `Option` is for a user who has never opened the page at all.
        bury_siblings: Some(form.bury_siblings.is_some()),
        limits: DailyLimits {
            reviews: DailyLimits::parse(&form.max_reviews_per_day)?,
            new: DailyLimits::parse(&form.max_new_per_day)?,
        },
        free_days: Some(FreeDays::new(free_days)?),
    })
}

/// What the boxes say when the user has set nothing: the value that is in
/// force, and where it came from.
fn inherited(value: String) -> Markup {
    html! {
        span.hint { "Inheriting " (value) " from the server." }
    }
}

fn number_field(
    name: &str,
    label: &str,
    hint: &str,
    value: Option<String>,
    inherits: String,
    attrs: (f64, f64, f64),
) -> Markup {
    let (min, max, step) = attrs;
    html! {
        div.setting {
            label for=(name) { (label) }
            input.input type="number" id=(name) name=(name)
                min=(min) max=(max) step=(step)
                value=(value.clone().unwrap_or_default())
                placeholder=(inherits.clone());
            p.hint { (hint) }
            @if value.is_none() { (inherited(inherits)) }
        }
    }
}

pub fn render_settings(
    user: &UserSettings,
    inherited_scheduling: Scheduling,
    inherited_limits: DailyLimits,
    flash: Option<Flash>,
) -> Markup {
    let free = user.free_days.unwrap_or(inherited_scheduling.free_days);
    let days: [(&str, &str, bool); 7] = [
        ("free_mon", "Monday", free.is_free(chrono::Weekday::Mon)),
        ("free_tue", "Tuesday", free.is_free(chrono::Weekday::Tue)),
        ("free_wed", "Wednesday", free.is_free(chrono::Weekday::Wed)),
        ("free_thu", "Thursday", free.is_free(chrono::Weekday::Thu)),
        ("free_fri", "Friday", free.is_free(chrono::Weekday::Fri)),
        ("free_sat", "Saturday", free.is_free(chrono::Weekday::Sat)),
        ("free_sun", "Sunday", free.is_free(chrono::Weekday::Sun)),
    ];
    let bury = user.bury_siblings.unwrap_or(true);

    page_template(html! {
        div.landing {
            @if let Some(f) = &flash { (f.render()) }
            div.browse-header {
                a.back-link href="/" { "← Collections" }
                h1 { "Settings" }
            }
            p.hint {
                "These apply to every collection you own. A collection can \
                 override the two scheduling numbers and the daily limits on \
                 its own settings page. Leave a box empty to use the \
                 server's value."
            }

            form.add-source-form action="/settings" method="post" {
                h2 { "Scheduling" }
                (number_field(
                    "desired_retention",
                    "Desired retention",
                    "The chance a card is still remembered when it comes back. At 0.95, a card \
                     you would have seen in 30 days comes back in about 18: more reviews, more \
                     of them remembered. Between 0.7 and 0.99.",
                    user.retention.map(|v| v.into_inner().to_string()),
                    inherited_scheduling.retention.into_inner().to_string(),
                    (0.7, 0.99, 0.01),
                ))
                (number_field(
                    "max_interval_days",
                    "Maximum interval (days)",
                    "How far ahead a schedule is willing to plan. A statement about you rather \
                     than about the card: material you mean to keep for a career tolerates a \
                     longer ceiling than material you need until an exam.",
                    user.max_interval.map(|v| v.into_inner().to_string()),
                    inherited_scheduling.max_interval.into_inner().to_string(),
                    (1.0, 36500.0, 1.0),
                ))
                (number_field(
                    "jitter",
                    "Interval jitter",
                    "Scatters each interval by up to this fraction, so reviews that were \
                     learned together do not come back together forever. 0.05 is plus or minus \
                     five percent.",
                    user.jitter.map(|v| v.into_inner().to_string()),
                    inherited_scheduling.jitter.into_inner().to_string(),
                    (0.0, 0.5, 0.01),
                ))

                h2 { "Daily limits" }
                p.hint {
                    "Applied per collection, not shared between them. Empty means no limit; \
                     zero means none of that kind today."
                }
                (number_field(
                    "max_reviews_per_day",
                    "Most reviews per day",
                    "Cards you have seen before. The rest wait for tomorrow -- a backlog is \
                     never hidden, so a capped collection says how many are really due.",
                    user.limits.reviews.map(|v| v.to_string()),
                    inherited_limits
                        .reviews
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "no limit".to_string()),
                    (0.0, 100000.0, 1.0),
                ))
                (number_field(
                    "max_new_per_day",
                    "Most new cards per day",
                    "Cards you have never seen. A collection you have not drilled yet is all \
                     new cards, so this is the limit that bites first.",
                    user.limits.new.map(|v| v.to_string()),
                    inherited_limits
                        .new
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "no limit".to_string()),
                    (0.0, 100000.0, 1.0),
                ))

                h2 { "Your week" }
                p.hint { "Cards never come due on these days. At least one day must stay open." }
                @for (name, label, is_free) in days {
                    div.setting {
                        label {
                            @if is_free {
                                input type="checkbox" name=(name) checked;
                            } @else {
                                input type="checkbox" name=(name);
                            }
                            " " (label)
                        }
                    }
                }

                h2 { "Sessions" }
                div.setting {
                    label {
                        @if bury {
                            input type="checkbox" name="bury_siblings" checked;
                        } @else {
                            input type="checkbox" name="bury_siblings";
                        }
                        " Bury siblings"
                    }
                    p.hint {
                        "Show one deletion from a cloze note per session, and leave the rest of \
                         the family for another day."
                    }
                }

                div.add-source-row {
                    input.btn.btn-primary type="submit" value="Save settings";
                }
            }
        }
    })
}

/// The caller's review database.
///
/// On a GET a missing file is *not* created: serving a page must not
/// materialize a database for someone who has never drilled. They see the
/// inherited values, which is the truth.
fn settings_db(
    state: &AppState,
    owner: Option<&str>,
    create: bool,
) -> Fallible<Option<UserDatabase>> {
    let Some(data_dir) = state.config.data_dir.as_ref() else {
        return fail("No data directory is configured, so settings cannot be stored.");
    };
    let root = CardRoot::open(data_dir, owner)?;
    let path = user_db_path(&root, &data_dir.join("db"))?;
    if !create && !Path::new(&path).exists() {
        return Ok(None);
    }
    // The same gate every path that opens one of these files takes.
    refuse_if_unconsolidated(state, &path)?;
    Ok(Some(UserDatabase::open(&path)?))
}

fn load_settings(state: &AppState, owner: Option<&str>) -> Fallible<UserSettings> {
    Ok(settings_db(state, owner, false)?
        .map(|db| db.user_settings())
        .unwrap_or_default())
}

fn save_settings(state: &AppState, owner: Option<&str>, form: &SettingsForm) -> Fallible<()> {
    let settings = settings_from_form(form)?;
    match settings_db(state, owner, true)? {
        Some(db) => db.save_user_settings(&settings),
        None => fail("Your review database could not be opened, so nothing was saved."),
    }
}

/// What the user layer falls back to, for the "inheriting ..." lines.
fn inherited_from_instance(state: &AppState) -> (Scheduling, DailyLimits) {
    let scheduling = state
        .config
        .defaults
        .scheduling()
        .unwrap_or_else(|_| Scheduling::default());
    (scheduling, state.config.defaults.limits())
}

pub async fn settings_get_handler(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    let owner = current_user.map(|u| u.email);
    let state2 = state.clone();
    // SQLite, so not on the async executor (BUG-44).
    let loaded = run_blocking(move || load_settings(&state2, owner.as_deref())).await;
    let (scheduling, limits) = inherited_from_instance(&state);
    let markup = match loaded {
        Ok(settings) => render_settings(&settings, scheduling, limits, flash),
        Err(e) => render_settings(
            &UserSettings::default(),
            scheduling,
            limits,
            Some(Flash::error(e.to_string())),
        ),
    };
    (StatusCode::OK, Html(markup.into_string()))
}

pub async fn settings_post_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<SettingsForm>,
) -> (StatusCode, Html<String>) {
    let owner = current_user.map(|u| u.email);
    let state2 = state.clone();
    let outcome = run_blocking(move || {
        save_settings(&state2, owner.as_deref(), &form)?;
        load_settings(&state2, owner.as_deref())
    })
    .await;
    let (scheduling, limits) = inherited_from_instance(&state);
    match outcome {
        Ok(settings) => (
            StatusCode::OK,
            Html(
                render_settings(
                    &settings,
                    scheduling,
                    limits,
                    Some(Flash::success("Settings saved.".to_string())),
                )
                .into_string(),
            ),
        ),
        // The submitted values are gone by here, so the page comes back
        // showing what is stored rather than what was typed. The error says
        // which value was refused and why.
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Html(
                render_settings(
                    &UserSettings::default(),
                    scheduling,
                    limits,
                    Some(Flash::error(e.to_string())),
                )
                .into_string(),
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every control the page promises is there, and each shows the value in
    /// force rather than an empty box.
    #[test]
    fn the_page_shows_the_settings_in_force() -> Fallible<()> {
        let settings = UserSettings {
            retention: Some(DesiredRetention::new(0.85)?),
            free_days: Some(FreeDays::parse_list("sat")?),
            limits: DailyLimits {
                reviews: Some(40),
                new: None,
            },
            ..UserSettings::default()
        };
        let html = render_settings(
            &settings,
            Scheduling::default(),
            DailyLimits::default(),
            None,
        )
        .into_string();
        assert!(html.contains("name=\"desired_retention\""));
        assert!(html.contains("name=\"max_interval_days\""));
        assert!(html.contains("name=\"jitter\""));
        assert!(html.contains("name=\"max_reviews_per_day\""));
        assert!(html.contains("name=\"max_new_per_day\""));
        assert!(html.contains("name=\"free_sat\""));
        assert!(html.contains("0.85"));
        assert!(html.contains("40"));
        Ok(())
    }

    /// A field the user has not set says what it inherits, so the page never
    /// implies they have an opinion they do not have.
    #[test]
    fn an_unset_field_says_what_it_inherits() -> Fallible<()> {
        let html = render_settings(
            &UserSettings::default(),
            Scheduling::default(),
            DailyLimits::default(),
            None,
        )
        .into_string();
        assert!(html.contains("Inheriting"), "{html}");
        assert!(html.contains("no limit"), "an unset limit is no limit");
        Ok(())
    }

    /// A blank limit and a zero limit are different answers, and the form
    /// must carry the difference intact.
    #[test]
    fn blank_and_zero_limits_survive_the_form() -> Fallible<()> {
        let blank = settings_from_form(&SettingsForm {
            max_new_per_day: String::new(),
            ..SettingsForm::default()
        })?;
        assert_eq!(blank.limits.new, None);
        let zero = settings_from_form(&SettingsForm {
            max_new_per_day: "0".to_string(),
            ..SettingsForm::default()
        })?;
        assert_eq!(zero.limits.new, Some(0));
        Ok(())
    }

    /// A value out of range is refused with a message naming the range,
    /// because a form has an author who can be told.
    #[test]
    fn an_out_of_range_value_is_refused_with_a_message() {
        let err = settings_from_form(&SettingsForm {
            desired_retention: "2.0".to_string(),
            ..SettingsForm::default()
        })
        .expect_err("2.0 is out of range");
        assert!(err.to_string().contains("0.7"), "message was: {err}");
    }

    /// All seven days ticked is refused rather than saved: a week with no
    /// open day has no due date to offer.
    #[test]
    fn a_week_with_no_open_day_is_refused() {
        let ticked = || Some("on".to_string());
        let form = SettingsForm {
            free_mon: ticked(),
            free_tue: ticked(),
            free_wed: ticked(),
            free_thu: ticked(),
            free_fri: ticked(),
            free_sat: ticked(),
            free_sun: ticked(),
            ..SettingsForm::default()
        };
        let err = settings_from_form(&form).expect_err("seven free days is not a week");
        assert!(err.to_string().contains("open"), "message was: {err}");
    }

    /// An unticked box is an answer, not an absence: unticking the last free
    /// day must clear the setting rather than re-inherit the server's.
    #[test]
    fn unticking_every_day_is_no_free_days() -> Fallible<()> {
        let settings = settings_from_form(&SettingsForm::default())?;
        assert_eq!(settings.free_days, Some(FreeDays::none()));
        assert_eq!(settings.bury_siblings, Some(false));
        Ok(())
    }
}
