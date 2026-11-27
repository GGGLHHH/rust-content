# 开发命令(和 baserust 同一套目标名)。用法:`just <目标>`。

# 读 .env(可选):DATABASE_URL 等。
set dotenv-load := true

# PG conformance 连 content role(search_path=content 由 role 配置继承)。默认指向 baserust 本地 compose 的 pg;
# 第一次跑前在 baserust 里 `just pg-test-grant`(给 content 授 CREATEDB,#[sqlx::test] 要建临时库),
# 另外 content 要有 CREATEROLE:测试 bootstrap 会建一个 NOLOGIN、NOBYPASSRLS 的运行时账号 content_rt,
# 仓储经 SET ROLE 用它跑,RLS 才真的生效。**只在一次性的测试库 / 测试容器上授**(超级用户执行
# `alter role content createrole`),别在开发库(默认端口 5821)或任何长期库上授:运行时账号带 CREATEROLE
# 等于能建账号、改别的账号的成员关系。测试容器换端口跑:`PG_PORT=<测试容器端口> just test-pg`。
pg_url := env_var_or_default("DATABASE_URL", "postgres://" + env_var_or_default("CONTENT_DB_USER", "content") + ":" + env_var_or_default("CONTENT_DB_PASSWORD", "pwd") + "@" + env_var_or_default("PG_HOST", "localhost") + ":" + env_var_or_default("PG_PORT", "5821") + "/" + env_var_or_default("POSTGRES_DB", "baserust") + "?sslmode=disable")

# 编译/clippy 实时反馈面板(bacon 默认键位:面板内按 c 切 clippy、t 切 test、d 开文档)
watch:
    bacon clippy

# 快速编译检查
check:
    cargo check --workspace --all-targets

# 测试(默认:零 DB,含内存侧仓储一致性 conformance)
test:
    cargo test --workspace

# 仓储一致性打真 PG(同一份契约,内存 / PG 各跑一遍,防 drift)
test-pg:
    DATABASE_URL="{{pg_url}}" cargo test --features pg-conformance --test content_repo_conformance -- --nocapture

# 全量:内存 + PG
test-all: test test-pg

# lint:格式检查 + clippy(警告即失败)
lint:
    cargo fmt --all -- --check
    cargo +nightly fmt --all -- --check --config {{google_fmt}}
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# 自动格式化
fmt:
    cargo fmt --all
    cargo +nightly fmt --all -- --config {{google_fmt}}

# Google Rust 风格指南要求、但只有 nightly rustfmt 支持的三项(稳定版只有 rustfmt.toml 里的 max_width)。
# 和 .github/workflows/ci.yml 里 nightly fmt 那一步的 --config 必须一致,改一处要同步另一处。
google_fmt := "imports_granularity=Module,hex_literal_case=Lower,format_macro_matchers=true"

# 自动修复:clippy 机器可应用的建议自动改 + 格式化
fix:
    cargo clippy --fix --allow-dirty --allow-staged --workspace --all-targets --all-features
    cargo fmt --all
    cargo +nightly fmt --all -- --config {{google_fmt}}

# 清理本 crate 的编译产物与增量缓存(保留依赖缓存)
clean:
    cargo clean -p content
    rm -rf target/debug/incremental
