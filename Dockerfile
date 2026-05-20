# syntax=docker/dockerfile:1

# ===================================================
# Build stage
# ===================================================
# rust-toolchain.toml で channel が pin されているため、
# rustup show でその channel を自動 install させます。
FROM rust:slim-bookworm AS builder

WORKDIR /app

# wreq は内部で BoringSSL (boring-sys) を使うため、ビルド時に cmake / git などが必要です。
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        git \
        libclang-dev && \
    rm -rf /var/lib/apt/lists/*

# rust-toolchain.toml に書かれた channel を install させます。
COPY rust-toolchain.toml ./
RUN rustup show

# 依存関係のキャッシュ層を作るため、まず manifest だけコピーして
# ダミー src でビルドし、その後で本体 src をコピーします。
# 依存に変更が無ければ、ここまでのレイヤがキャッシュヒットします。
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && \
    echo 'fn main() {}' > src/main.rs && \
    cargo build --release && \
    rm -rf src

# 本物のソースをコピーして、再ビルド。
# 依存関係のビルドキャッシュは残しつつ、自前 crate だけ再ビルドされます。
COPY src ./src
COPY config ./config

# touch しないと cargo が main.rs の変更を検知しないことがあります。
RUN touch src/main.rs && \
    cargo build --release && \
    # 実行ファイルだけを取り出して、後段の COPY を簡潔にします。
    cp target/release/tech-radar-mcp-rs /app/tech-radar-mcp-rs

# ===================================================
# Runtime stage
# ===================================================
# distroless/cc-debian13 は glibc + libgcc1 + ca-certificates 等を含みます。
# wreq は webpki-roots（Mozilla の CA を静的バンドル）を使うため
# 通常は CA store は不要ですが、保険として cc-debian13 を選んでいます。
# :nonroot タグは UID/GID 65532 の非 root ユーザーで実行する variant です。
FROM gcr.io/distroless/cc-debian13:nonroot

WORKDIR /app

# 実行ファイルと設定ファイルをコピーします。
# 設定は config/sources.toml をデフォルトで埋め込んでおき、
# 必要なら Cloud Run でボリュームマウントなどで差し替える運用にします。
COPY --from=builder /app/tech-radar-mcp-rs /app/tech-radar-mcp-rs
COPY --from=builder /app/config /app/config

# Cloud Run は環境変数 PORT で listen ポートを指示します。
# 本コンテナはデフォルトで 8080 を expose しつつ、
# 起動時に PORT が来ればそれを優先する実装になっています（main.rs 参照）。
EXPOSE 8080

# distroless には shell が無いので、ENTRYPOINT / CMD は exec 形式（JSON 配列）で書きます。
# CMD で --transport=http をデフォルトに固定し、
# 環境変数だけで運用できるようにしています。
ENTRYPOINT ["/app/tech-radar-mcp-rs"]
CMD ["--transport=http"]
