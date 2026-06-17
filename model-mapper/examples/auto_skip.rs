#![allow(dead_code)]

use model_mapper::Mapper;

mod auto_skip_basic {
    use super::*;

    // `Foo` is the target type referenced by `ty = Foo`. It does NOT have
    // `id`, `creator`, `create_time`, etc., so those fields on `Bar` must
    // be skipped automatically.
    struct Foo {
        field1: String,
        field2: i64,
    }

    #[derive(Mapper)]
    #[mapper(from, ty = Foo, auto_skip)]
    struct Bar {
        field1: String,
        field2: i64,
        // These fields exist on Bar but not on Foo, so they should be
        // skipped (and populated with Default::default() for `from`).
        id: i64,
        creator: Option<String>,
        create_time: Option<String>,
        updater: Option<String>,
        update_time: Option<String>,
        tenant_id: i64,
    }
}

mod auto_skip_into {
    use super::*;

    // Same as above, but for `into` direction. The fields missing on the
    // target (`Foo`) on `Bar`'s side are skipped.
    #[derive(Default)]
    struct Foo {
        field1: String,
        field2: i64,
    }

    #[derive(Mapper)]
    #[mapper(into, ty = Foo, auto_skip)]
    struct Bar {
        field1: String,
        field2: i64,
        // Missing on Foo -> skipped (and Foo requires Default for `into`).
        id: i64,
        creator: Option<String>,
        create_time: Option<String>,
        updater: Option<String>,
        update_time: Option<String>,
        tenant_id: i64,
    }
}

fn main() {}
