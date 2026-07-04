# content

Self-contained **content / media** domain & service crate (**zero HTTP**): `ContentService` + pluggable repo / blob-store / clock ports + its own `ContentError`. Embed it as a library — the consuming app owns the HTTP edge (routes, DTOs, validation, status codes) and injects the production S3/minio blob backend.

> Mirrors the **rust-idm** skeleton (trait ports + in-memory/Postgres impls + builder service + domain-owned errors); ported from the Go content service.

## What's inside

- **Domain** — `ContentService` (content CRUD / one-shot upload / download / metadata / status transitions) + repo ports (`ContentRepo` / `ObjectRepo`, in-memory + Postgres impls via sqlx + sea-query).
- **Blob-store port** — `ObjectStore` (`upload` / `download` / `delete` / `object_meta`), the analog of idm's token ports: default `InMemoryObjectStore` runs with zero external deps; the app injects minio/S3 for production via the builder. Payloads are owned `bytes::Bytes` (buffered; streaming is a DEFER item).
- **Clock port** — `Clock` / `SystemClock` (inject a fixed clock to test timestamps deterministically).
- **Domain types & errors** — `Content` / `Object` / `ContentMetadata` / `ObjectMetadata` / `UploadOutcome` (plain data, no serde on the wire) + typed `ContentStatus` / `ObjectStatus` lifecycles and a 7-variant `ContentError` (`NotFound` / `NotReady` / `InvalidState` / `InvalidStatus` / `Conflict` / `Storage` / `Internal`); HTTP status, machine code and wire shape are the app's job via `From<ContentError> for AppError`.
- **Migrations** — in `migrations/`; copy them into your app's `migrations/content` dir to run. `0002` (derived content) ships schema + an opaque `DerivedContent` stub only — derivation logic is DEFER.
- **Consistency (v0.1, documented trade-off)** — `upload_content` creates DB rows before pushing bytes, with no cross-repo transaction: a failed store upload can leave orphan rows; backend-metadata sync steps are non-fatal. See `service.rs` / `repo/mod.rs`.

## Use it

```toml
[dependencies]
content = { git = "https://github.com/GGGLHHH/rust-content", tag = "v0.1.0" }
```

Implement the repo ports (or use the bundled `InMemory*` / `Pg*` impls), inject an `ObjectStore`, build a `ContentService`, hold it in your `AppState` (cheap `Clone`), and call its methods from your own handlers:

```rust
use std::sync::Arc;
use content::{ContentService, InMemoryContentRepo, InMemoryObjectRepo, InMemoryObjectStore};

let svc = ContentService::new(
    Arc::new(InMemoryContentRepo::new()),
    Arc::new(InMemoryObjectRepo::new()),
    Arc::new(InMemoryObjectStore::new()),   // prod: your minio/S3 ObjectStore impl
    "memory",                               // storage backend name written to object rows
);
// or override individual ports (custom store / backend name / test clock) via the builder:
let svc = ContentService::builder(contents, objects)
    .store(my_s3_store)      // required — build() panics without it (wiring error, fails at startup)
    .backend_name("s3")
    .build();

let out = svc.upload_content(upload_input, Some(actor)).await?; // -> UploadOutcome { content, object }
let bytes = svc.download_content(out.content.id).await?;        // -> Bytes (NotReady until uploaded)
```

## Service methods

`ContentService`: `create_content` · `get_content` · `update_content` · `delete_content` (soft) · `list_content` · `upload_content` · `download_content` · `set_content_metadata` · `get_content_metadata` · `get_objects` · `set_content_status` · `set_object_status`. Methods take domain `*Input` structs (write methods also take `by: Option<String>` for audit) and return domain data; routing, auth/tenant enforcement and the wire shape are the app's responsibility.

## Testing

```sh
cargo test                                 # unit + in-memory repo conformance (no DB)
cargo test --features pg-conformance       # + memory↔Postgres repo parity (needs a running pg)
```

## License

MIT — see [LICENSE](LICENSE).
