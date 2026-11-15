//! Searching an album's index: plain words people type, matched against
//! what the index knows about each item (date, type, favorite, file name).
//!
//! - Dates: "2021", "march", "march 2021", "3 march", "march 3 2021",
//!   "today", "yesterday", "this week", "last month", "this year", ...
//! - Types: "videos", "live photos", "photos", "favorites".
//! - Anything else must appear in the file name ("img_12", "heic", "png").
//!
//! Every word must match. Small words ("from", "in", "my") are skipped.
//! Nothing here looks inside the photos: Apple's object, face and text
//! search runs on the iPhone and isn't on the web.

use jiff::civil::Date;

use crate::index::{IndexItem, Kind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    terms: Vec<Term>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Term {
    Photo,
    Video,
    Live,
    Favorite,
    /// Any of the parts that were given: "march" matches every March.
    Date {
        year: Option<i16>,
        month: Option<i8>,
        day: Option<i8>,
    },
    /// A span of days, both ends included.
    Days(Date, Date),
    /// Part of the file name, lowercased.
    Name(String),
}

const SKIPPED: &[&str] = &[
    "a", "an", "and", "at", "from", "in", "my", "of", "on", "the", "taken", "with", "all",
];

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

fn month(word: &str) -> Option<i8> {
    // Full names, or the first three letters and more ("sept", "dec").
    if word.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|name| name.starts_with(word))
        .map(|i| i as i8 + 1)
}

fn day_number(word: &str) -> Option<i8> {
    let word = word
        .strip_suffix("st")
        .or_else(|| word.strip_suffix("nd"))
        .or_else(|| word.strip_suffix("rd"))
        .or_else(|| word.strip_suffix("th"))
        .unwrap_or(word);
    let n: i8 = word.parse().ok()?;
    (1..=31).contains(&n).then_some(n)
}

fn year(word: &str) -> Option<i16> {
    let n: i16 = word.parse().ok()?;
    (1900..=2200).contains(&n).then_some(n)
}

impl Query {
    /// `None` when there's nothing to search for.
    pub fn parse(text: &str, today: Date) -> Option<Query> {
        let lowered = text.to_lowercase();
        let words: Vec<&str> = lowered
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| !w.is_empty() && !SKIPPED.contains(w))
            .collect();

        let mut terms = Vec::new();
        // Date parts collect into one term, in any order.
        let (mut y, mut m, mut d) = (None, None, None);
        let mut i = 0;
        while i < words.len() {
            let word = words[i];
            let next = words.get(i + 1).copied();
            let mut used = 1;
            match (word, next) {
                ("this" | "last", Some(unit @ ("week" | "month" | "year"))) => {
                    terms.push(relative(unit, word == "last", today));
                    used = 2;
                }
                ("today", _) => terms.push(Term::Days(today, today)),
                ("yesterday", _) => {
                    let day = today.yesterday().unwrap_or(today);
                    terms.push(Term::Days(day, day));
                }
                ("live", Some("photo" | "photos")) => {
                    terms.push(Term::Live);
                    used = 2;
                }
                ("live" | "lives", _) => terms.push(Term::Live),
                ("video" | "videos" | "movie" | "movies" | "clip" | "clips", _) => {
                    terms.push(Term::Video)
                }
                ("photo" | "photos" | "picture" | "pictures" | "pics" | "stills", _) => {
                    terms.push(Term::Photo)
                }
                (
                    "favorite" | "favorites" | "favourite" | "favourites" | "fav" | "favs"
                    | "faves" | "loved" | "hearted",
                    _,
                ) => terms.push(Term::Favorite),
                _ => {
                    if let Some(n) = year(word) {
                        y = Some(n);
                    } else if let Some(n) = month(word) {
                        m = Some(n);
                    } else if let Some(n) = day_number(word)
                        && (m.is_some() || next.and_then(month).is_some())
                    {
                        // A day only counts next to a month: "3 march", "march 3".
                        d = Some(n);
                    } else {
                        terms.push(Term::Name(word.to_string()));
                    }
                }
            }
            i += used;
        }
        if y.is_some() || m.is_some() || d.is_some() {
            terms.push(Term::Date {
                year: y,
                month: m,
                day: d,
            });
        }
        (!terms.is_empty()).then_some(Query { terms })
    }

    /// Whether `item`, taken on `day` (local), matches every term.
    pub fn matches(&self, item: &IndexItem, day: Option<Date>) -> bool {
        self.terms.iter().all(|term| match term {
            Term::Photo => item.kind == Kind::Photo || item.kind == Kind::Live,
            Term::Video => matches!(item.kind, Kind::Video(_)),
            Term::Live => item.kind == Kind::Live,
            Term::Favorite => item.favorite,
            Term::Date {
                year,
                month,
                day: d,
            } => day.is_some_and(|day| {
                year.is_none_or(|y| day.year() == y)
                    && month.is_none_or(|m| day.month() == m)
                    && d.is_none_or(|d| day.day() == d)
            }),
            Term::Days(from, to) => day.is_some_and(|day| *from <= day && day <= *to),
            Term::Name(part) => item
                .name
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(part.as_str())),
        })
    }

    /// Positions of the items that match, in order.
    pub fn filter(&self, items: &[IndexItem], days: &[Option<Date>]) -> Vec<usize> {
        items
            .iter()
            .zip(days)
            .enumerate()
            .filter(|(_, (item, day))| self.matches(item, **day))
            .map(|(i, _)| i)
            .collect()
    }
}

/// "this week" (from Monday), "last month", and so on.
fn relative(unit: &str, last: bool, today: Date) -> Term {
    let span = |from: Option<Date>, to: Option<Date>| match (from, to) {
        (Some(from), Some(to)) => Term::Days(from, to),
        _ => Term::Days(today, today),
    };
    match unit {
        "week" => {
            let monday = today
                .checked_sub(jiff::Span::new().days(today.weekday().to_monday_zero_offset()))
                .ok();
            let monday = if last {
                monday.and_then(|d| d.checked_sub(jiff::Span::new().days(7)).ok())
            } else {
                monday
            };
            let sunday = monday.and_then(|d| d.checked_add(jiff::Span::new().days(6)).ok());
            span(monday, sunday)
        }
        "month" => {
            let first = today.first_of_month();
            let first = if last {
                first.checked_sub(jiff::Span::new().months(1)).ok()
            } else {
                Some(first)
            };
            span(first, first.map(|d| d.last_of_month()))
        }
        _ => {
            let year = today.year() - i16::from(last);
            span(Date::new(year, 1, 1).ok(), Date::new(year, 12, 31).ok())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn item(kind: Kind, favorite: bool, name: &str) -> IndexItem {
        IndexItem {
            id: "x".into(),
            date: 0,
            aspect: 1.,
            kind,
            favorite,
            name: Some(name.into()),
        }
    }

    // A Wednesday.
    const TODAY: Date = date(2026, 11, 18);

    fn q(text: &str) -> Query {
        Query::parse(text, TODAY).expect("a query")
    }

    #[test]
    fn empty_and_filler_words_are_no_query() {
        assert_eq!(Query::parse("", TODAY), None);
        assert_eq!(Query::parse("  in the ", TODAY), None);
    }

    #[test]
    fn dates() {
        let photo = item(Kind::Photo, false, "IMG_1.HEIC");
        let march_2021 = Some(date(2021, 3, 14));
        assert!(q("2021").matches(&photo, march_2021));
        assert!(q("march").matches(&photo, march_2021));
        assert!(q("Mar 2021").matches(&photo, march_2021));
        assert!(q("2021 march").matches(&photo, march_2021));
        assert!(q("march 14th").matches(&photo, march_2021));
        assert!(q("14 march 2021").matches(&photo, march_2021));
        assert!(!q("march 15").matches(&photo, march_2021));
        assert!(!q("april 2021").matches(&photo, march_2021));
        assert!(!q("march 2022").matches(&photo, march_2021));
        // Undated items match no date.
        assert!(!q("2021").matches(&photo, None));
        // "ma" is too short to be a month: it's a file name part.
        assert_eq!(q("ma").terms, vec![Term::Name("ma".into())]);
    }

    #[test]
    fn relative_dates() {
        let photo = item(Kind::Photo, false, "a.jpg");
        assert!(q("today").matches(&photo, Some(TODAY)));
        assert!(q("yesterday").matches(&photo, Some(date(2026, 11, 17))));
        assert!(!q("yesterday").matches(&photo, Some(TODAY)));
        // The week runs Monday 16 to Sunday 22.
        assert!(q("this week").matches(&photo, Some(date(2026, 11, 16))));
        assert!(!q("this week").matches(&photo, Some(date(2026, 11, 15))));
        assert!(q("last week").matches(&photo, Some(date(2026, 11, 9))));
        assert!(q("last month").matches(&photo, Some(date(2026, 10, 31))));
        assert!(!q("last month").matches(&photo, Some(date(2026, 11, 1))));
        assert!(q("this year").matches(&photo, Some(date(2026, 1, 1))));
        assert!(q("last year").matches(&photo, Some(date(2025, 6, 1))));
    }

    #[test]
    fn types_favorites_and_names() {
        let day = Some(date(2024, 7, 4));
        let video = item(Kind::Video(30), true, "IMG_0042.MOV");
        let live = item(Kind::Live, false, "IMG_0043.HEIC");
        assert!(q("videos").matches(&video, day));
        assert!(!q("videos").matches(&live, day));
        assert!(q("live photos").matches(&live, day));
        assert!(!q("live photos").matches(&video, day));
        assert!(q("photos").matches(&live, day));
        assert!(q("favorites").matches(&video, day));
        assert!(!q("favorites").matches(&live, day));
        assert!(q("img_0042").matches(&video, day));
        assert!(q("heic").matches(&live, day));
        assert!(!q("heic").matches(&video, day));
        // Everything must match.
        assert!(q("favorite videos from july 2024").matches(&video, day));
        assert!(!q("favorite videos from june 2024").matches(&video, day));
    }

    #[test]
    fn filter_keeps_order() {
        let items = vec![
            item(Kind::Video(1), false, "a.mov"),
            item(Kind::Photo, false, "b.jpg"),
            item(Kind::Video(2), false, "c.mov"),
        ];
        let days = vec![None, None, None];
        assert_eq!(q("videos").filter(&items, &days), vec![0, 2]);
    }
}
