# Making queries

Everything starts at `Model::objects()`. It returns a `Manager`, and from there
you build a **`QuerySet`** — lazy and immutable. Every method consumes `self` and
returns a new one, so nothing reaches the database until you `await` a terminal
method, and a base query is safely reusable:

```rust
let base = Post::objects().filter(post::title.icontains("rust"));
let recent = base.clone().order_by([post::published_at.desc()]).limit(5);
let count   = base.count().await?;   // `base` is untouched by `recent`
```

Two branches built from `base` cannot disturb each other, because neither
mutated it.

## Retrieving objects

| Terminal | Returns | Notes |
|---|---|---|
| `.all()` | `Vec<T>` | Every matching row. |
| `.first()` | `Option<T>` | The first, respecting the ordering. |
| `.last()` | `Option<T>` | The last; reverses the ordering and takes one. |
| `.get(q)` | `T` | Exactly one. Errors `DoesNotExist` for none, `MultipleObjectsReturned` for more than one. |
| `.count()` | `i64` | `COUNT(*)` of the match. |
| `.exists()` | `bool` | Whether anything matches, without counting. |

```rust
let all   = Post::objects().all().await?;
let one   = Post::objects().get(post::id.eq(42)).await?;
let maybe = Post::objects().filter(post::slug.eq("on-engines")).first().await?;
let n     = Post::objects().filter(post::title.icontains("rust")).count().await?;
```

## Filtering: `filter`, `exclude`, and `Q`

`filter` narrows; `exclude` is its complement. Repeated calls are `AND`-ed, as
Django's are:

```rust
Post::objects()
    .filter(post::title.icontains("rust"))
    .exclude(post::slug.startswith("draft-"))
```

Each `post::field.<lookup>(value)` builds a `Q`. Combine them with the operators
`&` (and), `|` (or), `!` (not):

```rust
use dorm_core::prelude::*;

let q = (post::title.icontains("rust") | post::title.icontains("ferris"))
      & !post::slug.startswith("draft-");
Post::objects().filter(q).all().await?;
```

### Field lookups

Typed handles carry only the lookups that make sense for the column's type.

| Lookup | Method | SQL |
|---|---|---|
| exact | `field.eq(v)` | `col = $1` |
| greater / less | `field.gt(v)`, `.gte(v)`, `.lt(v)`, `.lte(v)` | `col > $1`, … |
| in a set | `field.is_in([a, b, c])` | `col IN ($1, $2, $3)` |
| in a range | `field.range(lo, hi)` | `col BETWEEN $1 AND $2` |
| is null | `field.isnull(true)` | `col IS NULL` |
| case-insensitive exact | `field.iexact(v)` | `col ILIKE $1` *(text only)* |
| contains | `field.contains(v)`, `.icontains(v)` | `col LIKE '%'||$1||'%'` *(text only)* |
| starts with | `field.startswith(v)` | `col LIKE $1||'%'` *(text only)* |

```rust
Post::objects().filter(post::id.is_in([1, 2, 3])).all().await?;
Post::objects().filter(post::published_at.range(start, end)).all().await?;
Post::objects().filter(post::bio.isnull(true)).all().await?;
```

### The dynamic (string-keyed) form

The same query can be built from strings — the form a REPL or an admin console
sends. It produces the identical plan:

```rust
Post::objects().filter_str("title__icontains", "rust").all().await?;
```

Only columns the derive macro put in the binary resolve; an invented name is an
error, not a query.

## Ordering, limit, offset, distinct

```rust
Post::objects()
    .order_by([post::published_at.desc(), post::slug.asc()])
    .offset(20)
    .limit(10)
    .distinct()
    .all()
    .await?;
```

- `order_by` **replaces** rather than appends — the last call wins (as Django).
- The string form: `.order_by_str(&["-published_at", "slug"])`.
- With no `order_by`, the model's `#[model(ordering = …)]` applies.

## Following relations

### Across a filter

Traverse a foreign key by calling the field on the relation handle; the compiler
resolves the join:

```rust
// posts whose author's name contains "ada"
Post::objects()
    .filter(post::author.author().name().eq("Ada Lovelace"))
    .all()
    .await?;
```

### Fetching the parent in the same query

`select_related` joins the parent in and attaches it, so there is no second
round trip:

```rust
for post in Post::objects().select_related("author").all().await? {
    if let Some(author) = post.author.get() {
        println!("{} by {}", post.slug, author.name);
    }
}
```

## Aggregates: `annotate`, `aggregate`, `count_of`

`aggregate` computes over the whole query and returns one row of named values:

```rust
use dorm_core::prelude::*;

let row = Post::objects()
    .aggregate(vec![Aggregate::new("total", AggKind::Count, "id")])
    .await?;
let total = row.get("total");   // a Value
```

`annotate` adds a per-group aggregate — counting the reverse side of a relation
is the classic case:

```rust
// each author, with how many posts they have
Author::objects()
    .annotate(Aggregate::new("posts", AggKind::Count, "posts"))
    .all()
    .await?;
```

`AggKind` is `Count`, `Sum`, `Avg`, `Min`, `Max`. `count_of(path)` is shorthand
for a single `COUNT`.

## Projecting columns: `values` + `rows`

To select named columns instead of whole structs, project with `values` and read
with `rows` (name-to-`Value` maps):

```rust
let rows = Post::objects()
    .values(&["id", "title"])
    .rows()
    .await?;
for r in rows {
    println!("{} {}", r["id"], r["title"]);
}
```

`values()` and `all()` are mutually exclusive — calling `all()` on a projected
query is refused rather than decoding the wrong columns into the struct.

## Creating rows

Insert with the generated `New…` struct, which omits database-filled columns.
`create` returns the row as stored, with those columns filled in:

```rust
let ada = Author::objects()
    .create(NewAuthor { name: "Ada Lovelace".into() })
    .await?;
println!("new id = {}", ada.id);   // filled by the database
```

Insert many in a single statement:

```rust
let made = Author::objects()
    .bulk_create(vec![
        NewAuthor { name: "Ada".into() },
        NewAuthor { name: "Grace".into() },
    ])
    .await?;
```

Fetch-or-insert, idempotent under a race (a lost insert retries as a read):

```rust
let (author, created) = Author::objects()
    .get_or_create(author::name.eq("Ada"), NewAuthor { name: "Ada".into() })
    .await?;
```

## Updating rows

`update` applies a set of assignments to every matching row and returns how many
it touched. Build assignments with `field.set(expr)` or `Assignment::new`:

```rust
let n = Post::objects()
    .filter(post::slug.eq("on-engines"))
    .update(vec![post::title.set("On Analytical Engines")])
    .await?;
```

Use an `F` expression to compute from the column itself — this is **atomic in
the database**, so two concurrent callers both take effect:

```rust
use dorm_core::prelude::*;

// views = views + 1, inside the UPDATE
Post::objects()
    .filter(post::id.eq(42))
    .update(vec![Assignment::new("views", F::col("views") + 1)])
    .await?;
```

Reading the row, adding one in Rust, and writing it back would lose one of the
two increments; `F::col("views") + 1` does not.

## Deleting rows

```rust
let removed = Post::objects()
    .filter(post::slug.startswith("draft-"))
    .delete()
    .await?;   // u64: how many were deleted
```

## Seeing the SQL

Any `QuerySet` can show the SQL and parameters it would run — without a database,
and the foundation of DORM's snapshot tests:

```rust
let sql = Post::objects().filter(post::title.icontains("rust")).sql()?;
println!("{}", sql.text);          // SELECT … WHERE "posts"."title" ILIKE $1
println!("{}", sql.params.len());  // 1
```

## Next

- [Migrations](migrations.md) — generating and applying the schema these queries run against.
- [Connection & transactions](connection-and-transactions.md) — `atomic`, read-only mode, the pool.
