# AGENTS.md — rust-content

`content`:自包含的内容 / 对象存储**领域 / 服务库(零 HTTP)**,由 baserust 以 git 依赖引用:
发过版的按 tag 钉(`tag = "vX.Y.Z"`),没打 tag 的版本按提交钉(`rev = "<提交>"`,如 0.4.0 时 baserust 钉的 `rev = "fa3e600"`)。
代码规范、工具、检查口径和 baserust 一致(同一套 `just` 目标、同一份 lint 配置、同一个 Rust 版本)。

- 人看的用法 / 端口 / 测试 → `README.md`
- **怎么干活 → 本文**

## 在这干活的铁律

1. **零 HTTP、零框架**:不许引入 axum / tower / utoipa / reqwest / 任何 web 框架;路由、DTO、校验、cookie、状态码、
   OpenAPI 全归消费方(baserust)。错误只用本 crate 的 `ContentError`,HTTP 映射由消费方 `From<ContentError> for AppError` 做。
2. **每个改动过 `just check && just test && just lint`**(clippy `-D warnings`,零警告;Rust 版本由 `rust-toolchain.toml` 固定;
   `just fmt` / `just lint` 另要 nightly rustfmt,只用于 Google 风格的三项格式化配置,见 README)。
   动了 PG 仓储再跑 `just test-pg`。CI(`.github/workflows/ci.yml`)跑的是同一套。
3. **仓储改动 = 内存和 PG 两个实现一起改,契约一起加**:`tests/content_repo_conformance.rs` 的 `*_contract` 是唯一真相源,
   内存 / PG 各跑一遍。断言顺序、相对关系、可见性、错误类型;时间戳由调用方传入、仓储按传入值落库,
   要断言绝对值就传截到微秒的固定时刻(PG timestamptz 的精度,见 `timestamps_follow_input_contract`),
   别拿系统时钟的读数去比。仓储写方法不许自己取时间(不调 `now_utc()`、不靠 DB `now()` / 列默认值)。
4. **每个 PG 仓储方法加 `#[tracing::instrument(name = "pg.<表>.<方法>", skip_all)]`**:baserust 后台「系统监控」的
   数据库耗时就是按这个 span 统计的,不加就看不到。`skip_all`:参数里有用户数据,别进日志字段。
   以后若有每秒一轮的轮询方法(像 rust-idm outbox 的 `poll_unpublished`)不加,否则空轮询刷屏。
5. **事务**:一个原子单元 = 一个 trait 方法(方法内开事务);事务对象不进 trait 签名;内存实现先校验、后改状态。
6. **迁移在 `migrations/`**:消费方拷进自己的 `migrations/content` 跑(落哪个 schema 由 role 的 search_path 决定)。
   SQL 不写 schema 前缀。改了迁移,baserust 那边要同步拷贝。
7. **发版**:先升 `Cargo.toml` 的 `version`,在 README「版本变更」写这一版的变化(破坏性变更列「旧 → 新」),
   再打新 tag(`vX.Y.Z`,和 `version` 一致);然后 baserust 改 `Cargo.toml` 的 tag(原先按 `rev` 钉的改回 `tag`)、更新 `Cargo.lock`,
   再在 baserust-stack 提交两边的新子模块指针(stack 的 CI 会核对子模块 = `Cargo.lock` 锁的提交)。
8. **注释、文档用中文**;代码标识符英文。
9. **提交需许可**:未经明确同意不要 commit / push / 打 tag。

## 代码风格

依据 [Google Rust 风格指南](https://google.github.io/styleguide/rustguide.html)(标 `[chrome]` / `[Fuchsia]` 的条目不适用)。
原则是**全面贴合**:只有落在指南**明文例外条款**里的才保留,并就地写明依据的条款名(`#[allow(..., reason = "条款例外(…):…")]`
或文档注释);新加例外照此办理,同时补进下面的表。

**工具守的**(过 `just lint` 即算数,CI 跑同一套):

- 格式:稳定版 rustfmt(`rustfmt.toml`:`max_width = 100`)+ nightly rustfmt 的三项
  `imports_granularity=Module,hex_literal_case=Lower,format_macro_matchers=true`(justfile 的 `google_fmt`,
  和 `.github/workflows/ci.yml` 必须一致)。两种 `--check` 都要干净。
- clippy `-D warnings` + `Cargo.toml` 的 `[lints]`:`unsafe_code = "forbid"`、`missing_docs`、`absolute_paths`
  (`clippy.toml` 限两段)、`must_use_candidate` / `return_self_not_must_use`、`missing_panics_doc`、`redundant_clone`、
  `exhaustive_enums` / `exhaustive_structs`;产品代码另有 `#![warn(clippy::unwrap_used)]`(`lib.rs`),测试里的
  `unwrap` / `expect` 放行(`clippy.toml`,指南「Reporting errors」:Tests may use panics for assertions)。
- `RUSTDOCFLAGS=-D warnings cargo doc --no-deps` 无警告(文档里的链接要能解析)。

**人守的**(clippy 管不到,评审时看):

- 文件 / 叶子模块按主类型命名;非叶子模块(`lib.rs`、`repo/mod.rs`)只放子模块声明和 re-export。
- 一个 `use` 只从一个模块导入,不用 glob;类型 / trait 导入后裸用,顶层函数带模块名调用(`pg_support::internal`)。
- 别过度依赖类型推断:类型标在**变量声明**上,不写 turbofish ——
  `let row: Option<ContentRecord> = sqlx::query_as_with(..)`、`let status: ContentStatus = s.parse()?`;
  编译器非要 turbofish 时才写,并尽量用 `_`。
- 能用模式匹配证明就别「先判断再 unwrap」;必须 panic 用 `expect("原因")`;断言带上下文字符串。
- 命名不缩写(例外见表);返回 `Result` 的构造函数叫 `try_new*`;并排的同类型参数 / `bool` 参数改具名结构体 / 枚举。
- 公开枚举、带公开字段的公开结构体加 `#[non_exhaustive]`,结构体配 `new(必填字段)`,可选字段构造后直接赋值;
  错误类型、builder 加 `#[must_use]`。
- 服务的端口是类型参数(泛型),库内部不用 `Arc<dyn …>`;每个端口 trait(两个仓储、`ObjectStore`、`Clock`)都给
  `Arc<T>` / `Box<T>` 写转发实现,**全部方法**都转发(有默认实现的也转发,否则下游覆盖的实现会被默认实现顶掉)。
- 公开 API 都有中文文档注释;返回 `Result` 的写 `# Errors`,可能 panic 的写 `# Panics`。

**保留的例外**(代码里都写了依据):

| 位置 | 例外 | 依据条款 |
|---|---|---|
| `ObjectLocation`(`repo/object_repo.rs`) | 不加 `#[non_exhaustive]`,保留字面量构造(`#[allow(clippy::exhaustive_structs, reason)]`) | 「Apply `#[non_exhaustive]` to public structs with public fields」例外:字段描述既有本体(位置就是「后端 + key」两段,不会再加) |
| `SystemClock`(`clock.rs`) | 同上(无字段的单元结构体) | 同一条款的同一例外:无状态、不会加字段 |
| `UpdateContentInput`(`input.rs`) | 不加 `#[non_exhaustive]`,保留字面量 + `Default`,让调用方写 `UpdateContentInput { name, ..Default::default() }` | 同一条款例外:You want to enable your users to use functional record update (FRU);另见「Options structs should implement Default」 |
| `__pg_testing`(`lib.rs`) | `#[doc(hidden)] pub mod`,只在 `pg-conformance` feature 下编译 | 「Do not access items marked `#[doc(hidden)]` that you don't own」:只给本仓库自己的 `tests/content_repo_conformance.rs` 用(同一个 owner),下游不得引用 |
| `ContentRepo::get_many` / `list`、`ObjectRepo::list_by_content`、`ContentService::list_content` 返回 `Vec` | 不返回迭代器 | 「Iterators vs. owned collections in return types」例外:The function's implementation naturally constructs a vector(数据库取回的就是 `Vec`) |
| `get_many(ids: &[Uuid])` / `get_metadata_many` 收切片 | 不收 `impl IntoIterator` | 「Iterators vs. slices in function parameters」让位于「Generics versus `dyn Trait`」:泛型方法会让 trait 失去 dyn 兼容性,而下游要用 `Arc<dyn ContentRepo>` |
| 命名里的 `Repo`、`Pg`(`ContentRepo`、`PgContentRepo`、`PgObjectRepo`…) | 保留缩写 | 「Avoid abbreviations」例外:Abbreviations that are unlikely to be confusing to people outside of your project(和 sqlx 的 `PgPool`、baserust 的 `*Repo` 同一套叫法;见 `repo/mod.rs`) |
| `fmt::Result` 带模块名(`content_status.rs` / `object_status.rs` 的 `Display`) | 不裸用类型名 | 「Type names should be imported and used unqualified」例外:有歧义时可带模块名(和 `std::result::Result` 同名) |
| `ContentError` 不实现 `Copy` | — | 错误类型「Implement the Copy trait whenever possible」:变体带 `String` / `anyhow::Error`,做不到(见 `content_error.rs`) |

## 命令

`just check` · `just test` · `just test-pg` · `just lint` · `just fmt` · `just fix` · `just watch`
