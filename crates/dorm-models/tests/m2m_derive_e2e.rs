//! `#[m2m(..)]` end to end on SQLite — the proof that the derive makes a
//! many-to-many first-class: a struct attribute generates a bound accessor, so
//! `post.tags()?.add(tag_pk)` and `.all_keys()` work off the instance's own key.
//!
//! Posts and Tags, linked by a `PostTag { post, tag }` junction. `Post` carries
//! `#[m2m(name = "tags", through = "PostTag", via = "post", to = "tag")]`; the
//! derive emits `Post::tags(&self) -> Result<BoundManyToMany>`, binding
//! `post.pk()` so the manager's writes take only the other side.
//!
//! ```text
//! cargo test -p dorm-models --features sqlite --test m2m_derive_e2e
//! ```
#![cfg(feature = "sqlite")]

use dorm_core::prelude::*;
// The derive and its `m2m`/`field` helper attributes need the macro in scope —
// the prelude brings the `Model` *trait*, not the derive macro.
use dorm_derive::Model;
use dorm_migrations::ddl::{schema_for_on, Backend};
use tokio::runtime::Runtime;

#[derive(Model, Debug, Clone)]
#[model(table = "m2md_posts", ordering = "id")]
#[m2m(name = "tags", through = "PostTag", via = "post", to = "tag")]
struct Post {
    #[field(primary_key)]
    id: i64,
    #[field(max_length = 200)]
    title: String,
}

#[derive(Model, Debug, Clone)]
#[model(table = "m2md_tags", ordering = "id")]
struct Tag {
    #[field(primary_key)]
    id: i64,
    #[field(max_length = 60)]
    label: String,
}

/// The junction: a pure join row, two foreign keys. `post` is the `via` field
/// (points back at `Post`), `tag` the `to` field (points at `Tag`).
#[derive(Model, Debug, Clone)]
#[model(table = "m2md_post_tags", ordering = "id")]
struct PostTag {
    #[field(primary_key)]
    id: i64,
    #[field(foreign_key = "Post", on_delete = "cascade")]
    post: ForeignKey<Post>,
    #[field(foreign_key = "Tag", on_delete = "cascade")]
    tag: ForeignKey<Tag>,
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
fn derived_m2m_accessor_binds_the_instance_key() {
    std::env::set_var("DELONIX_ORM_DSN", "sqlite::memory:");

    runtime().block_on(async {
        let ddl = schema_for_on(
            &[
                <Post as Model>::meta(),
                <Tag as Model>::meta(),
                <PostTag as Model>::meta(),
            ],
            Backend::Sqlite,
        )
        .expect("the schema compiles");
        db::run_ddl(&ddl).await.expect("create the schema");

        let hello = Post::objects()
            .create(NewPost {
                title: "Hello".into(),
            })
            .await
            .expect("insert post 1");
        let world = Post::objects()
            .create(NewPost {
                title: "World".into(),
            })
            .await
            .expect("insert post 2");
        let rust = Tag::objects()
            .create(NewTag {
                label: "rust".into(),
            })
            .await
            .expect("insert tag 1");
        let orm = Tag::objects()
            .create(NewTag {
                label: "orm".into(),
            })
            .await
            .expect("insert tag 2");

        // The derived accessor: bound to `hello`'s key, so add() takes only the
        // tag. This is the ergonomics the task asks for.
        hello
            .tags()
            .expect("resolve")
            .add(rust.pk())
            .await
            .expect("link rust");
        hello
            .tags()
            .expect("resolve")
            .add(orm.pk())
            .await
            .expect("link orm");
        world
            .tags()
            .expect("resolve")
            .add(rust.pk())
            .await
            .expect("link rust to world");

        // Idempotent, like Django's .add().
        hello
            .tags()
            .expect("resolve")
            .add(rust.pk())
            .await
            .expect("re-link");

        assert_eq!(
            hello.tags().expect("resolve").count().await.expect("count"),
            2,
            "Hello has two tags"
        );
        assert_eq!(
            world.tags().expect("resolve").count().await.expect("count"),
            1,
            "World has one tag"
        );
        assert!(hello
            .tags()
            .expect("resolve")
            .exists(orm.pk())
            .await
            .expect("exists"));
        assert!(!world
            .tags()
            .expect("resolve")
            .exists(orm.pk())
            .await
            .expect("exists"));

        // all_keys() — the tag ids Hello is bound to, as i64 with no type named.
        let mut hello_tag_ids = hello
            .tags()
            .expect("resolve")
            .all_keys()
            .await
            .expect("all_keys");
        hello_tag_ids.sort_unstable();
        let mut expected = vec![rust.id, orm.id];
        expected.sort_unstable();
        assert_eq!(hello_tag_ids, expected);

        // remove() one, then clear() the rest — both bound to Hello.
        assert_eq!(
            hello
                .tags()
                .expect("resolve")
                .remove(rust.pk())
                .await
                .expect("remove"),
            1
        );
        assert_eq!(
            hello.tags().expect("resolve").count().await.expect("count"),
            1
        );
        assert_eq!(
            hello.tags().expect("resolve").clear().await.expect("clear"),
            1
        );
        assert_eq!(
            hello.tags().expect("resolve").count().await.expect("count"),
            0,
            "Hello cleared"
        );
        // World's link is untouched by Hello's clear().
        assert_eq!(
            world.tags().expect("resolve").count().await.expect("count"),
            1
        );
    });
}
