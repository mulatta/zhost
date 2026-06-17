//! Parsing for the CLI-facing item query API (`GET /users/<id>/items`).
//!
//! This module is pure — it turns the request's query string into an
//! [`ItemQuery`] without touching the database — so the filter/sort/paginate
//! semantics can be unit-tested. `store::query_items` turns the result into SQL.

/// Multi-valued query parameters. axum's `Query<HashMap>` collapses repeated
/// keys, but Zotero's `tag` filter repeats the key to mean AND, so we keep every
/// occurrence and decode percent-escapes once.
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn parse(raw: Option<&str>) -> Self {
        let pairs = raw
            .and_then(|q| serde_urlencoded::from_str::<Vec<(String, String)>>(q).ok())
            .unwrap_or_default();
        Params(pairs)
    }

    /// First value for `key`, if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Every value given for `key`, in order.
    pub fn all(&self, key: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

/// `itemType` filter: a value like `book || journalArticle` includes those
/// types; a `-` prefix (`-attachment`) excludes one. An item matches when its
/// type is in `include` (or `include` is empty) and not in `exclude`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TypeFilter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl TypeFilter {
    fn parse(value: Option<&str>) -> Self {
        let mut filter = TypeFilter::default();
        let Some(value) = value else {
            return filter;
        };
        for token in value.split("||").map(str::trim).filter(|t| !t.is_empty()) {
            if let Some(neg) = token.strip_prefix('-') {
                filter.exclude.push(neg.trim().to_string());
            } else {
                filter.include.push(token.to_string());
            }
        }
        filter
    }
}

/// What `q` matches. `titleCreatorYear` (the default) searches title, creators
/// and date; `everything` additionally searches stored full-text content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QMode {
    TitleCreatorYear,
    Everything,
}

impl QMode {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("everything") => QMode::Everything,
            _ => QMode::TitleCreatorYear,
        }
    }
}

/// Sort key. Each maps to a SQL ordering expression over the item's jsonb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    DateModified,
    DateAdded,
    Title,
    Creator,
    Date,
    ItemType,
}

impl Sort {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("dateAdded") => Sort::DateAdded,
            Some("title") => Sort::Title,
            Some("creator") => Sort::Creator,
            Some("date") => Sort::Date,
            Some("itemType") => Sort::ItemType,
            _ => Sort::DateModified,
        }
    }

    /// The jsonb expression to order by. `creator` orders by the first creator's
    /// last name; the rest read a top-level data field.
    pub fn order_expr(self) -> &'static str {
        match self {
            Sort::DateModified => "data->>'dateModified'",
            Sort::DateAdded => "data->>'dateAdded'",
            Sort::Title => "data->>'title'",
            Sort::Creator => "data->'creators'->0->>'lastName'",
            Sort::Date => "data->>'date'",
            Sort::ItemType => "data->>'itemType'",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Asc,
    Desc,
}

impl Direction {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("asc") => Direction::Asc,
            _ => Direction::Desc,
        }
    }

    pub fn sql(self) -> &'static str {
        match self {
            Direction::Asc => "asc",
            Direction::Desc => "desc",
        }
    }
}

const DEFAULT_LIMIT: i64 = 25;
const MAX_LIMIT: i64 = 100;

/// A parsed item listing request: a search term, filters, an ordering and a page
/// window. Built from the query string; consumed by `store::query_items`.
#[derive(Debug)]
pub struct ItemQuery {
    pub q: Option<String>,
    pub qmode: QMode,
    pub item_type: TypeFilter,
    /// AND of OR-groups: each repeated `tag` param is one group, `||`-split into
    /// alternatives. An item must carry a tag from every group.
    pub tags: Vec<Vec<String>>,
    pub sort: Sort,
    pub direction: Direction,
    pub limit: i64,
    pub start: i64,
    pub include_trashed: bool,
}

impl ItemQuery {
    pub fn from_params(params: &Params) -> Self {
        let q = params
            .get("q")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);

        let tags = params
            .all("tag")
            .into_iter()
            .map(|group| {
                group
                    .split("||")
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .filter(|group: &Vec<String>| !group.is_empty())
            .collect();

        // Page window: default 25, hard cap 100. A non-positive or unparseable
        // limit falls back to the default rather than returning nothing.
        let limit = params
            .get("limit")
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|n| *n >= 1)
            .map(|n| n.min(MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);
        let start = params
            .get("start")
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|n| *n >= 0)
            .unwrap_or(0);

        let include_trashed = matches!(params.get("includeTrashed"), Some("1") | Some("true"));

        ItemQuery {
            q,
            qmode: QMode::parse(params.get("qmode")),
            item_type: TypeFilter::parse(params.get("itemType")),
            tags,
            sort: Sort::parse(params.get("sort")),
            direction: Direction::parse(params.get("direction")),
            limit,
            start,
            include_trashed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(raw: &str) -> ItemQuery {
        ItemQuery::from_params(&Params::parse(Some(raw)))
    }

    #[test]
    fn defaults_when_empty() {
        let q = query("");
        assert!(q.q.is_none());
        assert_eq!(q.qmode, QMode::TitleCreatorYear);
        assert_eq!(q.sort, Sort::DateModified);
        assert_eq!(q.direction, Direction::Desc);
        assert_eq!(q.limit, 25);
        assert_eq!(q.start, 0);
        assert!(!q.include_trashed);
        assert!(q.tags.is_empty());
        assert_eq!(q.item_type, TypeFilter::default());
    }

    #[test]
    fn blank_q_is_dropped() {
        assert!(query("q=").q.is_none());
        assert!(query("q=%20%20").q.is_none());
        assert_eq!(query("q=rust").q.as_deref(), Some("rust"));
    }

    #[test]
    fn qmode_everything_opt_in() {
        assert_eq!(query("qmode=everything").qmode, QMode::Everything);
        assert_eq!(
            query("qmode=titleCreatorYear").qmode,
            QMode::TitleCreatorYear
        );
        assert_eq!(query("qmode=bogus").qmode, QMode::TitleCreatorYear);
    }

    #[test]
    fn item_type_includes_and_excludes() {
        let f = query("itemType=book || journalArticle").item_type;
        assert_eq!(f.include, vec!["book", "journalArticle"]);
        assert!(f.exclude.is_empty());

        let f = query("itemType=-note || -attachment").item_type;
        assert!(f.include.is_empty());
        assert_eq!(f.exclude, vec!["note", "attachment"]);

        let f = query("itemType=book || -note").item_type;
        assert_eq!(f.include, vec!["book"]);
        assert_eq!(f.exclude, vec!["note"]);
    }

    #[test]
    fn tags_repeat_for_and_pipe_for_or() {
        // Two repeated params -> two AND groups; the second has two OR alternatives.
        let q = query("tag=urgent&tag=todo || later");
        assert_eq!(q.tags, vec![vec!["urgent"], vec!["todo", "later"]]);
    }

    #[test]
    fn limit_is_clamped_and_start_floored() {
        assert_eq!(query("limit=500").limit, 100);
        assert_eq!(query("limit=0").limit, 25);
        assert_eq!(query("limit=-3").limit, 25);
        assert_eq!(query("limit=10").limit, 10);
        assert_eq!(query("start=-5").start, 0);
        assert_eq!(query("start=40").start, 40);
    }

    #[test]
    fn include_trashed_opt_in() {
        assert!(query("includeTrashed=1").include_trashed);
        assert!(query("includeTrashed=true").include_trashed);
        assert!(!query("includeTrashed=0").include_trashed);
        assert!(!query("").include_trashed);
    }

    #[test]
    fn sort_and_direction_whitelist() {
        assert_eq!(query("sort=title").sort, Sort::Title);
        assert_eq!(query("sort=creator").sort, Sort::Creator);
        assert_eq!(query("sort=nonsense").sort, Sort::DateModified);
        assert_eq!(query("direction=asc").direction, Direction::Asc);
        assert_eq!(query("direction=nonsense").direction, Direction::Desc);
        assert_eq!(
            Sort::Creator.order_expr(),
            "data->'creators'->0->>'lastName'"
        );
    }
}
