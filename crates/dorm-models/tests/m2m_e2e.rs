//! Many-to-many, end to end on SQLite — the proof that DORM does M2M today
//! through an explicit junction model, with the [`ManyToMany`] manager for the
//! writes and the registry's two-way join resolver for the reads.
//!
//! Authors and Books, linked by an `Authorship { author, book }` junction. No
//! `ManyToMany` field type, no compiler change: `add`/`remove`/`clear` go through
//! the manager, and "an author's books" is a reverse-relation filter the query
//! layer already compiles.
//!
//! ```text
//! cargo test -p dorm-models --features sqlite --test m2m_e2e
//! ```
#![cfg(feature = "sqlite")]

use dorm_core::prelude::*;
use dorm_derive::Model;
use dorm_migrations::ddl::{schema_for_on, Backend};
use tokio::runtime::Runtime;

#[derive(Model, Debug, Clone)]
#[model(table = "m2m_authors", ordering = "id")]
struct Author {
    #[field(primary_key)]
    id: i64,
    #[field(max_length = 120)]
    name: String,
}

#[derive(Model, Debug, Clone)]
#[model(table = "m2m_books", ordering = "id")]
struct Book {
    #[field(primary_key)]
    id: i64,
    #[field(max_length = 200)]
    title: String,
}

/// The junction: a pure join row, two foreign keys. Both carry the same
/// `related_name`, which is unambiguous because the reverse resolver keys it by
/// the *target* model — `authorships` off `Author` finds `Authorship.author`,
/// off `Book` finds `Authorship.book`.
#[derive(Model, Debug, Clone)]
#[model(table = "m2m_authorships", ordering = "id")]
struct Authorship {
    #[field(primary_key)]
    id: i64,
    #[field(
        foreign_key = "Author",
        on_delete = "cascade",
        related_name = "authorships"
    )]
    author: ForeignKey<Author>,
    #[field(
        foreign_key = "Book",
        on_delete = "cascade",
        related_name = "authorships"
    )]
    book: ForeignKey<Book>,
}

fn runtime() -> &'static Runtime {
    static RT: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a tokio runtime")
    })
}

#[test]
fn many_to_many_through_a_junction_model() {
    std::env::set_var("DELONIX_ORM_DSN", "sqlite::memory:");

    runtime().block_on(async {
        let ddl = schema_for_on(
            &[
                <Author as Model>::meta(),
                <Book as Model>::meta(),
                <Authorship as Model>::meta(),
            ],
            Backend::Sqlite,
        )
        .expect("the schema compiles");
        db::run_ddl(&ddl).await.expect("create the schema");

        let ada = Author::objects()
            .create(NewAuthor { name: "Ada".into() })
            .await
            .expect("insert Ada");
        let grace = Author::objects()
            .create(NewAuthor {
                name: "Grace".into(),
            })
            .await
            .expect("insert Grace");
        let analytical = Book::objects()
            .create(NewBook {
                title: "The Analytical Engine".into(),
            })
            .await
            .expect("insert book 1");
        let compiler = Book::objects()
            .create(NewBook {
                title: "A-0 Compiler".into(),
            })
            .await
            .expect("insert book 2");

        let link = ManyToMany::on::<Authorship>("author", "book").expect("the junction resolves");

        // add() — Ada wrote both books; Grace wrote the compiler one.
        link.add(ada.pk(), analytical.pk()).await.expect("link 1");
        link.add(ada.pk(), compiler.pk()).await.expect("link 2");
        link.add(grace.pk(), compiler.pk()).await.expect("link 3");

        // Idempotent, like Django's .add(): re-adding changes nothing.
        link.add(ada.pk(), analytical.pk()).await.expect("re-link");
        assert_eq!(link.count(ada.pk()).await.expect("count"), 2, "Ada has two");
        assert_eq!(
            link.count(grace.pk()).await.expect("count"),
            1,
            "Grace has one"
        );
        assert!(link
            .exists(grace.pk(), compiler.pk())
            .await
            .expect("exists"));
        assert!(!link
            .exists(grace.pk(), analytical.pk())
            .await
            .expect("exists"));

        // keys() — the book ids Ada is linked to.
        let mut ada_book_ids: Vec<i64> = link.keys(ada.pk()).await.expect("keys");
        ada_book_ids.sort_unstable();
        assert_eq!(ada_book_ids, vec![analytical.id, compiler.id]);

        // The read side with NO new machinery: "Ada's books" is a reverse-relation
        // filter the query compiler already joins through the junction.
        let titles: Vec<String> = Book::objects()
            .filter_str("authorships__author", ada.id)
            .order_by_str(&["title"])
            .all()
            .await
            .expect("books of an author")
            .into_iter()
            .map(|b| b.title)
            .collect();
        assert_eq!(titles, vec!["A-0 Compiler", "The Analytical Engine"]);

        // And the other direction: the authors of the compiler book.
        let author_count = Author::objects()
            .filter_str("authorships__book", compiler.id)
            .count()
            .await
            .expect("authors of a book");
        assert_eq!(author_count, 2, "Ada and Grace both wrote it");

        // remove() one link, then clear() the rest.
        assert_eq!(
            link.remove(ada.pk(), compiler.pk()).await.expect("remove"),
            1
        );
        assert_eq!(link.count(ada.pk()).await.expect("count"), 1);
        assert_eq!(link.clear(ada.pk()).await.expect("clear"), 1);
        assert_eq!(link.count(ada.pk()).await.expect("count"), 0, "Ada cleared");
        // Grace's link is untouched.
        assert_eq!(link.count(grace.pk()).await.expect("count"), 1);
    });
}
