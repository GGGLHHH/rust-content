# 开发命令(和 baserust 同一套目标名)。用法:`just <目标>`。

# 读 .env(可选):DATABASE_URL 等。
set dotenv-load := true

# PG conformance 连 content role(search_path=content 由 role 配置继承)。默认指向 baserust 本地 compose 的 pg;
# 第一次跑前在 baserust 里 `just pg-test-grant`(给 content 授 CREATEDB,#[sqlx::test] 要建临时库)。
pg_url := env_var_or_default("DATABASE_URL", "postgres://content:" + env_var_or_default("CONTENT_DB_PASSWORD", "pwd") + "@" + env_var_or_default("PG_HOST", "localhost") + ":" + env_var_or_default("PG_PORT", "5821") + "/" + env_var_or_default("POSTGRES_DB", "baserust") + "?sslmode=disable")

# 编译/clippy 实时反馈面板(面板内按 c/l/t 切 check/clippy/test)
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
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# 自动格式化
fmt:
    cargo fmt --all

# 自动修复:clippy 机器可应用的建议自动改 + 格式化
fix:
    cargo clippy --fix --allow-dirty --allow-staged --workspace --all-targets --all-features
    cargo fmt --all

# 清理本 crate 的编译产物与增量缓存(保留依赖缓存)
clean:
    cargo clean -p content
    rm -rf target/debug/incremental
