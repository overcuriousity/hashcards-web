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

use crate::cmd::serve::decks::owned_collections;
use crate::fsrs::Grade;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::path::PathBuf;

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
use crate::fsrs::Weights;
use crate::fsrs::optimize::FitOutcome;
use crate::fsrs::optimize::MIN_REVIEWS;
use crate::fsrs::optimize::fit;
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
    #[serde(default)]
    pub weights: String,
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
        weights: parse_weights(&form.weights)?,
    })
}

/// A blank box inherits. Emptying it is how the page offers "back to the
/// defaults": pasting today's defaults back in would freeze them into the
/// user's settings, so that improving them later would skip this user.
fn parse_weights(raw: &str) -> Fallible<Option<Weights>> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    Weights::parse_list(raw).map(Some)
}

/// A fit the user has not accepted yet.
///
/// Shown in the weight box with a notice saying what it would buy. Nothing
/// is stored until they submit the form: a schedule changing because a
/// button was pressed once is not something anyone asked for.
pub struct Proposal {
    pub weights: Weights,
    pub loss_before: f64,
    pub loss_after: f64,
    pub reviews: usize,
}

/// A fit is only offered when it beats the weights already in force.
///
/// Proposing a worse schedule as an improvement is the one outcome this
/// feature must never produce, so the check lives here rather than in the
/// page that renders it.
pub fn proposal_from(outcome: FitOutcome) -> Fallible<Proposal> {
    if outcome.loss_after >= outcome.loss_before {
        return fail(
            "Your current weights already explain your reviews as well as anything this fit \
             could find, so nothing is proposed. That is a good sign, not a failure.",
        );
    }
    Ok(Proposal {
        weights: outcome.weights,
        loss_before: outcome.loss_before,
        loss_after: outcome.loss_after,
        reviews: outcome.reviews,
    })
}

/// What each block of the vector governs, so the numbers are not 19
/// anonymous floats.
const WEIGHT_GROUPS: [(&str, &str); 5] = [
    ("0-3", "initial stability, one per grade"),
    ("4-5", "initial difficulty"),
    ("6-7", "how difficulty moves with each grade"),
    (
        "8-16",
        "how stability grows on success and collapses on a lapse",
    ),
    ("17-18", "same-day reviews"),
];

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
    render_settings_with(user, inherited_scheduling, inherited_limits, None, flash)
}

pub fn render_settings_with(
    user: &UserSettings,
    inherited_scheduling: Scheduling,
    inherited_limits: DailyLimits,
    proposal: Option<&Proposal>,
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

                h2 { "FSRS weights" }
                p.hint {
                    "The 19 numbers the scheduling formulas are built from. The defaults are \
                     fitted to a large population of other people's reviews; these can be \
                     fitted to yours. Paste a list from an optimizer, or empty the box to go \
                     back to the defaults."
                }
                div.setting {
                    @if let Some(p) = proposal {
                        div.notice {
                            p {
                                (format!(
                                    "Fitted to {} of your reviews. Predicted-recall error \
                                     falls from {:.4} to {:.4}.",
                                    p.reviews, p.loss_before, p.loss_after
                                ))
                            }
                            p { strong { "Nothing is saved until you press Save settings." } }
                        }
                    }
                    textarea.input name="weights" rows="4"
                        placeholder=(inherited_scheduling.weights.to_list()) {
                        @match proposal {
                            Some(p) => (p.weights.to_list()),
                            None => (user.weights.map(|w| w.to_list()).unwrap_or_default()),
                        }
                    }
                    @if user.weights.is_none() && proposal.is_none() {
                        (inherited(inherited_scheduling.weights.to_list()))
                    }
                    table.weight-table {
                        tbody {
                            @for (range, what) in WEIGHT_GROUPS {
                                tr { td { (range) } td { (what) } }
                            }
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

            // A separate form, so pressing Optimize cannot save the rest of
            // the page as a side effect.
            form.add-source-form action="/settings/optimize" method="post" {
                div.add-source-row {
                    input.btn.btn-secondary type="submit"
                        value="Fit the weights to my reviews";
                }
                p.hint {
                    (format!(
                        "Reads your whole review history and looks for weights that predict \
                         it better than the ones in force. Needs at least {} reviews. It \
                         proposes; you decide.",
                        MIN_REVIEWS
                    ))
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

/// Every review this user has, across every collection they own.
///
/// One fit over the whole tree rather than one per collection: 19
/// parameters need every observation there is, and a person's forgetting is
/// more theirs than it is their Spanish deck's.
fn user_review_sequences(
    state: &AppState,
    owner: Option<&str>,
) -> Fallible<Vec<Vec<(f64, Grade)>>> {
    let mut opened: HashMap<PathBuf, UserDatabase> = HashMap::new();
    let mut sequences = Vec::new();
    for rc in owned_collections(state, owner) {
        let user_db = match opened.entry(rc.db_path.clone()) {
            Entry::Occupied(slot) => slot.into_mut(),
            Entry::Vacant(slot) => {
                refuse_if_unconsolidated(state, &rc.db_path)?;
                slot.insert(UserDatabase::open(&rc.db_path)?)
            }
        };
        sequences.extend(
            user_db
                .collection(rc.collection_id.clone())
                .review_sequences()?,
        );
    }
    Ok(sequences)
}

/// Fit the weights currently in force to this user's history.
fn optimize_for(state: &AppState, owner: Option<&str>) -> Fallible<Proposal> {
    let sequences = user_review_sequences(state, owner)?;
    let user = load_settings(state, owner)?;
    let (scheduling, _) = inherited_from_instance(state);
    let in_force = user.weights.unwrap_or(scheduling.weights);
    proposal_from(fit(&sequences, &in_force)?)
}

pub async fn settings_optimize_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let owner = current_user.map(|u| u.email);
    let state2 = state.clone();
    // A bounded fit, but a fit: SQLite and a few thousand replays have no
    // business on the async executor.
    let outcome = run_blocking(move || {
        let proposal = optimize_for(&state2, owner.as_deref())?;
        let settings = load_settings(&state2, owner.as_deref())?;
        Ok((proposal, settings))
    })
    .await;
    let (scheduling, limits) = inherited_from_instance(&state);
    match outcome {
        Ok((proposal, settings)) => (
            StatusCode::OK,
            Html(
                render_settings_with(&settings, scheduling, limits, Some(&proposal), None)
                    .into_string(),
            ),
        ),
        // Too short a history and an unimprovable one are both ordinary
        // answers rather than errors, and both already say so in words.
        Err(e) => (
            StatusCode::OK,
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

    /// The box holds the whole vector in the format an optimizer emits, so
    /// a fitted list can be pasted straight in, and the table says what the
    /// numbers govern.
    #[test]
    fn the_weights_box_shows_the_vector_in_force() {
        let html = render_settings(
            &UserSettings::default(),
            Scheduling::default(),
            DailyLimits::default(),
            None,
        )
        .into_string();
        assert!(html.contains("name=\"weights\""), "{html}");
        assert!(html.contains("0.40255"), "{html}");
        assert!(html.contains("initial stability"), "{html}");
    }

    #[test]
    fn a_pasted_weight_vector_is_accepted() -> Fallible<()> {
        let mut w = Weights::DEFAULT;
        w[0] = 0.5;
        let parsed = settings_from_form(&SettingsForm {
            weights: Weights::new(w)?.to_list(),
            ..SettingsForm::default()
        })?;
        assert_eq!(parsed.weights, Some(Weights::new(w)?));
        Ok(())
    }

    /// Blank clears the override, so "back to the defaults" is emptying the
    /// box rather than pasting today's defaults in and freezing them.
    #[test]
    fn an_empty_weights_box_inherits() -> Fallible<()> {
        assert_eq!(settings_from_form(&SettingsForm::default())?.weights, None);
        Ok(())
    }

    /// A bad paste is refused with a message naming the count, because 19
    /// numbers are hard to eyeball.
    #[test]
    fn a_bad_weight_paste_is_refused_with_a_count() {
        let err = settings_from_form(&SettingsForm {
            weights: "0.4, 1.2, 3.1".to_string(),
            ..SettingsForm::default()
        })
        .expect_err("three is not nineteen");
        assert!(err.to_string().contains("19"), "message was: {err}");
        assert!(err.to_string().contains('3'), "message was: {err}");
    }

    /// The proposal is shown, not applied: the box holds the fitted vector
    /// and the notice says what it would buy, but nothing is stored until
    /// the user submits the form.
    #[test]
    fn a_proposal_is_shown_rather_than_saved() -> Fallible<()> {
        let mut w = Weights::DEFAULT;
        w[2] = 4.0;
        let proposal = Proposal {
            weights: Weights::new(w)?,
            loss_before: 0.4231,
            loss_after: 0.3122,
            reviews: 1200,
        };
        let html = render_settings_with(
            &UserSettings::default(),
            Scheduling::default(),
            DailyLimits::default(),
            Some(&proposal),
            None,
        )
        .into_string();
        assert!(html.contains("1200"), "says what it fitted on: {html}");
        assert!(html.contains("0.4231") && html.contains("0.3122"), "{html}");
        assert!(
            html.contains(&proposal.weights.to_list()),
            "the box holds the proposal"
        );
        assert!(
            html.contains("Nothing is saved until"),
            "and says it is not saved: {html}"
        );
        Ok(())
    }

    /// A fit that did not improve is refused rather than offered. Proposing
    /// a worse schedule as an improvement is the one outcome this feature
    /// must never produce.
    #[test]
    fn a_fit_that_does_not_improve_is_refused() {
        let outcome = FitOutcome {
            weights: Weights::default(),
            loss_before: 0.30,
            loss_after: 0.30,
            reviews: 900,
        };
        assert!(proposal_from(outcome).is_err());

        let worse = FitOutcome {
            weights: Weights::default(),
            loss_before: 0.30,
            loss_after: 0.31,
            reviews: 900,
        };
        assert!(proposal_from(worse).is_err());
    }

    /// Without a proposal the page is exactly what it was, so the optimizer
    /// is invisible until it is asked for.
    #[test]
    fn no_proposal_leaves_the_box_showing_the_users_own_weights() -> Fallible<()> {
        let mut w = Weights::DEFAULT;
        w[2] = 4.0;
        let mine = Weights::new(w)?;
        let html = render_settings(
            &UserSettings {
                weights: Some(mine),
                ..UserSettings::default()
            },
            Scheduling::default(),
            DailyLimits::default(),
            None,
        )
        .into_string();
        assert!(html.contains(&mine.to_list()), "{html}");
        assert!(!html.contains("Nothing is saved until"), "{html}");
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
