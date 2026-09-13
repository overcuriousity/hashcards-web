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

//! One collection's scheduling settings.
//!
//! Four fields only. Jitter and free days are deliberately absent: they
//! spread one person's review peaks across every collection they own, so a
//! collection deciding them decides nothing.

use std::collections::HashMap;
use std::path::Path;

use axum::Form;
use axum::extract::Path as UrlPath;
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
use crate::cmd::serve::cards::collection_overrides;
use crate::cmd::serve::cards::write_collection_overrides;
use crate::cmd::serve::config::SchedulingOverrides;
use crate::cmd::serve::handlers::find_collection;
use crate::cmd::serve::reviewdb::user_settings_for;
use crate::cmd::serve::state::AppState;
use crate::error::Fallible;
use crate::error::fail;
use crate::flash::Flash;
use crate::types::limits::DailyLimits;
use crate::types::performance::DesiredRetention;
use crate::types::performance::MaxInterval;
use crate::types::performance::Scheduling;

/// The four fields this page edits, as the browser submits them. Blank means
/// "inherit", as it does on `/settings`.
#[derive(Deserialize, Default)]
pub struct CollectionSettingsForm {
    #[serde(default)]
    pub desired_retention: String,
    #[serde(default)]
    pub max_interval_days: String,
    #[serde(default)]
    pub max_reviews_per_day: String,
    #[serde(default)]
    pub max_new_per_day: String,
}

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

/// Parse the form and write it to the collection's own file.
///
/// The refusal `write_collection_overrides` makes on a file the parser
/// rejects is passed through as it is: this page is a new way to reach that
/// function, not a way around it.
pub fn apply_collection_form(folder: &Path, form: &CollectionSettingsForm) -> Fallible<()> {
    let retention = optional_number(
        &form.desired_retention,
        "desired retention",
        DesiredRetention::new,
    )?;
    let max_interval = optional_number(
        &form.max_interval_days,
        "the maximum interval",
        MaxInterval::new,
    )?;
    let limits = DailyLimits {
        reviews: DailyLimits::parse(&form.max_reviews_per_day)?,
        new: DailyLimits::parse(&form.max_new_per_day)?,
    };
    write_collection_overrides(folder, retention, max_interval, limits)
}

fn field(
    name: &str,
    label: &str,
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
            @if value.is_none() {
                span.hint { "Inheriting " (inherits) "." }
            }
        }
    }
}

pub fn render_collection_settings(
    name: &str,
    slug: &str,
    overrides: SchedulingOverrides,
    inherited_scheduling: Scheduling,
    inherited_limits: DailyLimits,
    flash: Option<Flash>,
) -> Markup {
    page_template(html! {
        div.landing {
            @if let Some(f) = &flash { (f.render()) }
            div.browse-header {
                a.back-link href=(format!("/collection/{slug}")) { "← " (name) }
                h1 { "Settings for " (name) }
            }
            p.hint {
                "These apply to this collection alone. Leave a box empty to use your own \
                 setting, or the server's where you have none. Jitter and free weekdays are \
                 set once for you on the "
                a href="/settings" { "settings page" }
                ": they spread your review peaks across every collection, so one collection \
                 deciding them would decide nothing."
            }

            form.add-source-form action=(format!("/collection/{slug}/settings")) method="post" {
                (field(
                    "desired_retention",
                    "Desired retention",
                    overrides.retention.map(|v| v.into_inner().to_string()),
                    inherited_scheduling.retention.into_inner().to_string(),
                    (0.7, 0.99, 0.01),
                ))
                (field(
                    "max_interval_days",
                    "Maximum interval (days)",
                    overrides.max_interval.map(|v| v.into_inner().to_string()),
                    inherited_scheduling.max_interval.into_inner().to_string(),
                    (1.0, 36500.0, 1.0),
                ))
                (field(
                    "max_reviews_per_day",
                    "Most reviews per day",
                    overrides.limits.reviews.map(|v| v.to_string()),
                    inherited_limits
                        .reviews
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "no limit".to_string()),
                    (0.0, 100000.0, 1.0),
                ))
                (field(
                    "max_new_per_day",
                    "Most new cards per day",
                    overrides.limits.new.map(|v| v.to_string()),
                    inherited_limits
                        .new
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "no limit".to_string()),
                    (0.0, 100000.0, 1.0),
                ))
                div.add-source-row {
                    input.btn.btn-primary type="submit" value="Save settings";
                }
            }
        }
    })
}

/// What this collection would be scheduled by if it overrode nothing: the
/// instance's settings with the user's laid over them.
fn inherited_for(
    state: &AppState,
    owner: Option<&str>,
    slug: &str,
) -> Fallible<(String, Scheduling, DailyLimits, SchedulingOverrides)> {
    let Some(rc) = find_collection(state, slug, owner) else {
        return fail(format!("Unknown collection: {slug}"));
    };
    let user = user_settings_for(state, &rc.db_path);
    let defaults = state.config.defaults.scheduling()?;
    let inherited = Scheduling {
        retention: user.retention.unwrap_or(defaults.retention),
        max_interval: user.max_interval.unwrap_or(defaults.max_interval),
        jitter: user.jitter.unwrap_or(defaults.jitter),
        free_days: user.free_days.unwrap_or(defaults.free_days),
    };
    let limits = user.limits.or(state.config.defaults.limits());
    Ok((rc.name.clone(), inherited, limits, rc.overrides))
}

pub async fn collection_settings_get_handler(
    State(state): State<AppState>,
    UrlPath(slug): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    let owner = current_user.map(|u| u.email);
    let slug2 = slug.clone();
    let state2 = state.clone();
    // SQLite and a directory walk, so not on the async executor (BUG-44).
    let loaded = run_blocking(move || inherited_for(&state2, owner.as_deref(), &slug2)).await;
    match loaded {
        Ok((name, scheduling, limits, overrides)) => (
            StatusCode::OK,
            Html(
                render_collection_settings(&name, &slug, overrides, scheduling, limits, flash)
                    .into_string(),
            ),
        ),
        // A slug that is not the caller's is indistinguishable from one that
        // does not exist, as it is everywhere else.
        Err(_) => (
            StatusCode::NOT_FOUND,
            Html(
                page_template(html! {
                    div.error {
                        h1 { "Error" }
                        p { "Unknown collection" }
                        a href="/" { "Back to collections" }
                    }
                })
                .into_string(),
            ),
        ),
    }
}

pub async fn collection_settings_post_handler(
    State(state): State<AppState>,
    UrlPath(slug): UrlPath<String>,
    current_user: Option<CurrentUser>,
    Form(form): Form<CollectionSettingsForm>,
) -> (StatusCode, Html<String>) {
    let owner = current_user.map(|u| u.email);
    let slug2 = slug.clone();
    let state2 = state.clone();
    let outcome = run_blocking(move || {
        let Some(rc) = find_collection(&state2, &slug2, owner.as_deref()) else {
            return fail(format!("Unknown collection: {slug2}"));
        };
        apply_collection_form(&rc.coll_dir, &form)?;
        let (name, scheduling, limits, _) = inherited_for(&state2, owner.as_deref(), &slug2)?;
        Ok((name, scheduling, limits, collection_overrides(&rc.coll_dir)))
    })
    .await;
    match outcome {
        Ok((name, scheduling, limits, overrides)) => (
            StatusCode::OK,
            Html(
                render_collection_settings(
                    &name,
                    &slug,
                    overrides,
                    scheduling,
                    limits,
                    Some(Flash::success("Settings saved.".to_string())),
                )
                .into_string(),
            ),
        ),
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Html(
                page_template(html! {
                    div.error {
                        h1 { "Error" }
                        p { (e) }
                        a href=(format!("/collection/{slug}/settings")) { "Back to settings" }
                    }
                })
                .into_string(),
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::cards::COLLECTION_META_FILE;

    /// A field the collection does not override shows what it inherits, so
    /// the page never implies a collection has an opinion it does not have.
    #[test]
    fn inherited_values_are_shown_as_inherited() {
        let html = render_collection_settings(
            "Spanish",
            "spanish",
            SchedulingOverrides::default(),
            Scheduling::default(),
            DailyLimits::default(),
            None,
        )
        .into_string();
        assert!(html.contains("Inheriting"), "{html}");
    }

    /// Jitter and free days have no collection layer, so the page must not
    /// offer them here.
    #[test]
    fn the_collection_page_offers_no_jitter_or_free_days() {
        let html = render_collection_settings(
            "Spanish",
            "spanish",
            SchedulingOverrides::default(),
            Scheduling::default(),
            DailyLimits::default(),
            None,
        )
        .into_string();
        assert!(!html.contains("name=\"jitter\""), "{html}");
        assert!(!html.contains("name=\"free_sat\""), "{html}");
    }

    /// Clearing a field removes the override rather than freezing the value
    /// it happened to be inheriting into the file. Otherwise raising the
    /// server's default would silently skip every collection ever edited.
    #[test]
    fn clearing_a_field_removes_the_override() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let folder = dir.path().join("Spanish");
        std::fs::create_dir(&folder)?;
        write_collection_overrides(
            &folder,
            Some(DesiredRetention::new(0.85)?),
            Some(MaxInterval::new(365.0)?),
            DailyLimits::default(),
        )?;
        assert!(collection_overrides(&folder).retention.is_some());

        apply_collection_form(
            &folder,
            &CollectionSettingsForm {
                desired_retention: String::new(),
                max_interval_days: "365".to_string(),
                ..CollectionSettingsForm::default()
            },
        )?;

        let after = collection_overrides(&folder);
        assert_eq!(after.retention, None, "the override is gone, not frozen");
        assert_eq!(
            after.max_interval,
            Some(MaxInterval::new(365.0)?),
            "its neighbour is untouched"
        );
        Ok(())
    }

    /// The limits round-trip through the file, including a zero, which must
    /// not read back as "no limit".
    #[test]
    fn limits_round_trip_through_the_collection_file() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let folder = dir.path().join("Spanish");
        std::fs::create_dir(&folder)?;
        apply_collection_form(
            &folder,
            &CollectionSettingsForm {
                max_reviews_per_day: "40".to_string(),
                max_new_per_day: "0".to_string(),
                ..CollectionSettingsForm::default()
            },
        )?;
        let after = collection_overrides(&folder);
        assert_eq!(after.limits.reviews, Some(40));
        assert_eq!(after.limits.new, Some(0), "zero is a limit, not an absence");
        Ok(())
    }

    /// This page is a new way to reach `write_collection_overrides`, not a
    /// way around it. Its refusal to rewrite a file the parser rejects is
    /// load-bearing: a file salvaged by eye would be rewritten as
    /// id-plus-settings over whatever the user was in the middle of.
    #[test]
    fn a_broken_collection_file_is_refused_not_rewritten() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let folder = dir.path().join("Spanish");
        std::fs::create_dir(&folder)?;
        let meta = folder.join(COLLECTION_META_FILE);
        let broken = "id = \"abc\"\ndesired retention = 0.95\n";
        std::fs::write(&meta, broken)?;

        let err = apply_collection_form(
            &folder,
            &CollectionSettingsForm {
                desired_retention: "0.85".to_string(),
                ..CollectionSettingsForm::default()
            },
        )
        .expect_err("a broken file is refused");
        assert!(
            err.to_string().contains(COLLECTION_META_FILE),
            "message was: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&meta)?,
            broken,
            "the file the user was editing is left exactly as it was"
        );
        Ok(())
    }
}
