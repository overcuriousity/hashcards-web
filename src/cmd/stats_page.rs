//! Collection statistics: gathering (from the DB and the parsed collection)
//! and server-side rendering with dependency-free CSS bar charts.
//!
//! Shared by serve mode (`/collection/{slug}/stats`), drill mode (`/stats`),
//! and the `hashcards stats` command.

use std::collections::BTreeMap;

use maud::Markup;
use maud::html;

use crate::db::Database;
use crate::db::GradeDistribution;
use crate::error::Fallible;
use crate::types::card::Card;
use crate::types::date::Date;

/// How many days ahead the due forecast covers.
pub const FORECAST_DAYS: u64 = 30;
/// How many days back the review history and retention window cover.
pub const HISTORY_DAYS: u64 = 90;

/// Per-deck card counts.
pub struct DeckStats {
    pub deck_name: String,
    pub due: usize,
    pub total: usize,
}

/// Everything the stats page shows.
pub struct CollectionStats {
    /// Cards becoming due on each of the next `FORECAST_DAYS` days; index 0 is
    /// today. Overdue and never-reviewed cards count into today's bucket.
    pub due_forecast: Vec<(Date, usize)>,
    /// Non-voided reviews per day over the last `HISTORY_DAYS` days; index 0
    /// is the oldest day. Days without reviews are present with count 0.
    pub reviews_per_day: Vec<(Date, usize)>,
    /// All-time non-voided review counts per grade.
    pub grades: GradeDistribution,
    /// Fraction of non-voided reviews in the last `HISTORY_DAYS` days graded
    /// better than Forgot. `None` when the window has no reviews.
    pub retention: Option<f64>,
    /// Per-deck due/total counts, sorted by deck name.
    pub decks: Vec<DeckStats>,
}

/// Collect all stats for one collection.
pub fn gather_stats(db: &Database, cards: &[Card], today: Date) -> Fallible<CollectionStats> {
    // Both the forecast and the per-deck column are derived from the parsed
    // collection, looked up in the DB. Aggregating the `cards` table directly
    // gets this wrong in both directions: nothing inserts a card row before
    // its first drill, so a never-drilled collection would report nothing due;
    // and rows left behind by deleted cards would inflate the forecast while
    // the per-deck column ignored them, so the two halves of the page could
    // not be reconciled.
    let due_dates = db.due_dates()?;
    // A card with no row has never been seen by the scheduler, so it is new
    // and therefore due today -- the same meaning `due_today` gives a row
    // whose `due_date` is NULL.
    let due_date_of = |card: &Card| due_dates.get(&card.hash()).copied().flatten();
    let by_due_date: Vec<(Option<Date>, usize)> =
        cards.iter().map(|card| (due_date_of(card), 1)).collect();
    let due_forecast = bucket_due_forecast(&by_due_date, today, FORECAST_DAYS)?;

    let since = today.sub_days(HISTORY_DAYS - 1)?;
    let per_day = db.count_reviews_per_day_since(since)?;
    let reviews_per_day = fill_missing_days(&per_day, since, HISTORY_DAYS)?;

    let grades = db.grade_distribution()?;
    let retention = db.retention_since(since)?;

    // Per-deck due/total comes from the parsed collection, since deck names
    // exist only on disk, not in the DB.
    let mut deck_counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for card in cards {
        let entry = deck_counts
            .entry(card.deck_name().clone())
            .or_insert((0, 0));
        entry.1 += 1;
        if due_date_of(card).is_none_or(|due| due <= today) {
            entry.0 += 1;
        }
    }
    let decks = deck_counts
        .into_iter()
        .map(|(deck_name, (due, total))| DeckStats {
            deck_name,
            due,
            total,
        })
        .collect();

    Ok(CollectionStats {
        due_forecast,
        reviews_per_day,
        grades,
        retention,
        decks,
    })
}

/// Bucket per-due-date card counts into a `days`-long forecast starting at
/// `today`. `None` (never reviewed) and overdue dates count into today's
/// bucket; dates beyond the window are dropped.
fn bucket_due_forecast(
    rows: &[(Option<Date>, usize)],
    today: Date,
    days: u64,
) -> Fallible<Vec<(Date, usize)>> {
    let mut buckets: Vec<(Date, usize)> = Vec::with_capacity(days as usize);
    for i in 0..days {
        buckets.push((today.add_days(i)?, 0));
    }
    for (due, count) in rows {
        let index: usize = match due {
            None => 0,
            Some(due) => {
                let offset = today.days_until(*due);
                if offset < 0 {
                    0
                } else if (offset as u64) < days {
                    offset as usize
                } else {
                    continue;
                }
            }
        };
        buckets[index].1 += count;
    }
    Ok(buckets)
}

/// Expand sparse per-day counts into a dense `days`-long series from `since`.
fn fill_missing_days(
    rows: &[(Date, usize)],
    since: Date,
    days: u64,
) -> Fallible<Vec<(Date, usize)>> {
    let mut filled: Vec<(Date, usize)> = Vec::with_capacity(days as usize);
    for i in 0..days {
        filled.push((since.add_days(i)?, 0));
    }
    for (date, count) in rows {
        let offset = since.days_until(*date);
        if offset >= 0 && (offset as u64) < days {
            filled[offset as usize].1 = *count;
        }
    }
    Ok(filled)
}

/// Render the stats page body (callers wrap it in `page_template`).
pub fn render_stats_page(
    collection_name: &str,
    stats: &CollectionStats,
    back_href: Option<&str>,
) -> Markup {
    let total_reviews =
        stats.grades.forgot + stats.grades.hard + stats.grades.good + stats.grades.easy;
    html! {
        div.stats-page {
            div.stats-header {
                @if let Some(href) = back_href {
                    a.back-link href=(href) { "\u{2190} Back" }
                }
                h1 { (collection_name) " \u{2014} Stats" }
            }
            section.stats-section {
                h2 { "Due forecast (next 30 days)" }
                (bar_chart(&stats.due_forecast))
            }
            section.stats-section {
                h2 { "Reviews per day (last 90 days)" }
                (bar_chart(&stats.reviews_per_day))
            }
            section.stats-section {
                h2 { "Grade distribution" }
                (grade_table(&stats.grades, total_reviews))
            }
            section.stats-section {
                h2 { "Retention" }
                @match stats.retention {
                    Some(r) => p.retention {
                        (format!("{:.1}% of reviews in the last 90 days were remembered.", r * 100.0))
                    },
                    None => p.retention { "No reviews in the last 90 days." },
                }
            }
            section.stats-section {
                h2 { "Decks" }
                @if stats.decks.is_empty() {
                    p { "No decks found." }
                } @else {
                    div.table-scroll {
                        table.deck-stats {
                            thead {
                                tr { th { "Deck" } th { "Due" } th { "Total" } }
                            }
                            tbody {
                                @for deck in &stats.decks {
                                    tr {
                                        td { (deck.deck_name) }
                                        td { (deck.due) }
                                        td { (deck.total) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A vertical CSS bar chart: one flex column per day, height proportional to
/// the maximum value. Dependency-free: no JS, no chart library.
/// A day with reviews on it is drawn, however few: `1 * 100 / 500` is zero,
/// and a zero-height bar is indistinguishable from a day with nothing on it.
/// A day with none is still zero.
fn bar_percent(count: usize, max: usize) -> usize {
    let scaled = count * 100 / max;
    if count > 0 { scaled.max(1) } else { 0 }
}

fn bar_chart(data: &[(Date, usize)]) -> Markup {
    let max = data.iter().map(|&(_, n)| n).max().unwrap_or(0).max(1);
    html! {
        div.bar-chart {
            @for (date, count) in data {
                div.bar-slot title=(format!("{date}: {count}")) {
                    div.bar style=(format!("height: {}%;", bar_percent(*count, max))) {}
                }
            }
        }
    }
}

/// Grade counts with horizontal CSS bars.
fn grade_table(grades: &GradeDistribution, total: usize) -> Markup {
    let rows = [
        ("Forgot", grades.forgot),
        ("Hard", grades.hard),
        ("Good", grades.good),
        ("Easy", grades.easy),
    ];
    let max = total.max(1);
    html! {
        table.grade-dist {
            tbody {
                @for (label, count) in rows {
                    tr {
                        td.key { (label) }
                        td.val { (count) }
                        td.bar-cell {
                            div.hbar style=(format!("width: {}%;", count * 100 / max)) {}
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::db::Database;
    use crate::types::card::CardContent;
    use crate::types::card_hash::CardHash;
    use crate::types::timestamp::Timestamp;

    /// Regression: a day with reviews on it was drawn at zero height when it
    /// rounded below one percent of the busiest day, and a day with none was
    /// drawn as a 1px line by the stylesheet's old floor. Zero must be the
    /// only thing that is invisible.
    #[test]
    fn test_bar_percent_floors_only_nonzero_days() {
        assert_eq!(bar_percent(0, 500), 0);
        assert_eq!(bar_percent(1, 500), 1);
        assert_eq!(bar_percent(250, 500), 50);
        assert_eq!(bar_percent(500, 500), 100);
    }

    fn date(s: &str) -> Date {
        Date::try_from(s.to_string()).unwrap()
    }

    fn make_card(deck: &str, question: &str) -> Card {
        Card::new(
            deck.to_string(),
            PathBuf::from("/tmp/deck.md"),
            (1, 2),
            CardContent::new_basic(question, "answer"),
        )
    }

    /// Regression: due counts must come from the parsed collection, not from
    /// whatever happens to be in the `cards` table. Nothing inserts card rows
    /// outside the start-drill path, so a collection that has never been
    /// drilled would otherwise report Due = 0 for every deck while showing a
    /// full card total, and the forecast would disagree with the deck table.
    #[test]
    fn test_never_drilled_collection_counts_every_card_due() -> Fallible<()> {
        let db = Database::memory()?;
        let cards = vec![
            make_card("Biology", "Q1"),
            make_card("Biology", "Q2"),
            make_card("Chemistry", "Q3"),
        ];
        // No card rows at all: this collection has never been drilled.
        let today = date("2026-09-01");
        let stats = gather_stats(&db, &cards, today)?;

        assert_eq!(stats.decks.len(), 2);
        for deck in &stats.decks {
            assert_eq!(
                deck.due, deck.total,
                "every card in '{}' is new, so all are due",
                deck.deck_name
            );
        }
        // The forecast must agree with the deck table rather than showing 0.
        assert_eq!(stats.due_forecast[0].1, 3);
        let deck_due: usize = stats.decks.iter().map(|d| d.due).sum();
        assert_eq!(deck_due, stats.due_forecast[0].1);
        Ok(())
    }

    /// The forecast counts only cards the collection still contains: a row
    /// left behind by a deleted card is an orphan, and counting it in the
    /// forecast while the per-deck column ignores it makes the two halves of
    /// the page irreconcilable.
    #[test]
    fn test_orphan_rows_are_excluded_from_the_forecast() -> Fallible<()> {
        let db = Database::memory()?;
        let cards = vec![make_card("Biology", "Q1")];
        let now = Timestamp::now();
        db.insert_card(cards[0].hash(), now)?;
        // A row whose card no longer exists in any deck.
        db.insert_card(CardHash::hash_bytes(b"deleted-card"), now)?;

        let stats = gather_stats(&db, &cards, date("2026-09-01"))?;
        let total: usize = stats.due_forecast.iter().map(|(_, n)| n).sum();
        assert_eq!(total, 1, "the orphan row must not appear in the forecast");
        Ok(())
    }

    #[test]
    fn test_bucket_due_forecast_clamps_overdue_and_new_to_today() -> Fallible<()> {
        let today = date("2026-08-31");
        let rows = vec![
            (None, 2),                     // new cards: due today
            (Some(date("2026-08-01")), 3), // overdue: due today
            (Some(date("2026-08-31")), 1), // due today
            (Some(date("2026-09-05")), 4), // day 5
            (Some(date("2026-10-15")), 9), // beyond the window: excluded
        ];
        let buckets = bucket_due_forecast(&rows, today, FORECAST_DAYS)?;
        assert_eq!(buckets.len(), 30);
        assert_eq!(buckets[0], (today, 6)); // 2 + 3 + 1
        assert_eq!(buckets[5], (date("2026-09-05"), 4));
        assert_eq!(buckets.iter().map(|&(_, n)| n).sum::<usize>(), 10);
        Ok(())
    }

    #[test]
    fn test_fill_missing_days() -> Fallible<()> {
        let since = date("2026-08-01");
        let rows = vec![(date("2026-08-01"), 5), (date("2026-08-03"), 7)];
        let filled = fill_missing_days(&rows, since, 4)?;
        assert_eq!(
            filled,
            vec![
                (date("2026-08-01"), 5),
                (date("2026-08-02"), 0),
                (date("2026-08-03"), 7),
                (date("2026-08-04"), 0),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_gather_stats_per_deck_counts() -> Fallible<()> {
        let db = Database::memory()?;
        let now = Timestamp::now();
        let a = make_card("Alpha", "q1");
        let b = make_card("Alpha", "q2");
        let c = make_card("Beta", "q3");
        for card in [&a, &b, &c] {
            db.insert_card(card.hash(), now)?;
        }
        // All three cards are new, hence due today.
        let cards = vec![a, b, c];
        let stats = gather_stats(&db, &cards, Date::today())?;
        assert_eq!(stats.decks.len(), 2);
        assert_eq!(stats.decks[0].deck_name, "Alpha");
        assert_eq!(stats.decks[0].due, 2);
        assert_eq!(stats.decks[0].total, 2);
        assert_eq!(stats.decks[1].deck_name, "Beta");
        assert_eq!(stats.decks[1].due, 1);
        assert_eq!(stats.decks[1].total, 1);
        // Three new cards land in today's forecast bucket.
        assert_eq!(stats.due_forecast[0].1, 3);
        assert_eq!(stats.due_forecast.len(), FORECAST_DAYS as usize);
        assert_eq!(stats.reviews_per_day.len(), HISTORY_DAYS as usize);
        assert_eq!(stats.retention, None);
        Ok(())
    }

    #[test]
    fn test_render_stats_page_sections() -> Fallible<()> {
        let db = Database::memory()?;
        let stats = gather_stats(&db, &[], Date::today())?;
        let html = render_stats_page("MyCollection", &stats, Some("/collection/my")).into_string();
        assert!(html.contains("MyCollection"));
        assert!(html.contains("Due forecast"));
        assert!(html.contains("Reviews per day"));
        assert!(html.contains("Grade distribution"));
        assert!(html.contains("Retention"));
        assert!(html.contains("No reviews in the last 90 days."));
        assert!(html.contains("Decks"));
        assert!(html.contains("/collection/my"));
        Ok(())
    }
}
