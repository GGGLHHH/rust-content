# AGENTS.md — rust-content

`content`:自包含的内容 / 对象存储**领域 / 服务库(零 HTTP)**,由 baserust 按 git tag 依赖。
代码规范、工具、检查口径和 baserust 一致(同一套 `just` 目标、同一份 lint 配置、同一个 Rust 版本)。

- 人看的用法 / 端口 / 测试 → `README.md`
- **怎么干活 → 本文**

## 在这干活的铁律

1. **零 HTTP、零框架**:不许引入 axum / tower / utoipa / reqwest / 任何 web 框架;路由、DTO、校验、cookie、状态码、
   OpenAPI 全归消费方(baserust)。错误只用本 crate 的 `ContentError`,HTTP 映射由消费方 `From<ContentError> for AppError` 做。
2. **每个改动过 `just check && just test && just lint`**(clippy `-D warnings`,零警告;Rust 版本由 `rust-toolchain.toml` 固定)。
   动了 PG 仓储再跑 `just test-pg`。CI(`.github/workflows/ci.yml`)跑的是同一套。
3. **仓储改动 = 内存和 PG 两个实现一起改,契约一起加**:`tests/content_repo_conformance.rs` 的 `*_contract` 是唯一真相源,
   内存 / PG 各跑一遍。只断言顺序、相对关系、可见性、错误类型,**不断言绝对时间戳**(PG `now()` ≠ 内存时钟)。
4. **每个 PG 仓储方法加 `#[tracing::instrument(name = "pg.<表>.<方法>", skip_all)]`**:baserust 后台「系统监控」的
   数据库耗时就是按这个 span 统计的,不加就看不到。`skip_all`:参数里有用户数据,别进日志字段。
   以后若有每秒一轮的轮询方法(像 rust-idm outbox 的 `poll_unpublished`)不加,否则空轮询刷屏。
5. **事务**:一个原子单元 = 一个 trait 方法(方法内开事务);事务对象不进 trait 签名;内存实现先校验、后改状态。
6. **迁移在 `migrations/`**:消费方拷进自己的 `migrations/content` 跑(落哪个 schema 由 role 的 search_path 决定)。
   SQL 不写 schema 前缀。改了迁移,baserust 那边要同步拷贝。
7. **发版**:改完打新 tag(`vX.Y.Z`),baserust 改 `Cargo.toml` 的 tag、更新 `Cargo.lock`,
   再在 baserust-stack 提交两边的新子模块指针(stack 的 CI 会核对子模块 = `Cargo.lock` 锁的提交)。
8. **注释、文档用中文**;代码标识符英文。
9. **提交需许可**:未经明确同意不要 commit / push / 打 tag。

## 命令

`just check` · `just test` · `just test-pg` · `just lint` · `just fmt` · `just fix` · `just watch`
