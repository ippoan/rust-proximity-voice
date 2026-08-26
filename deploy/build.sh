#!/usr/bin/env bash
#
# リリース用ビルドと配置。
#
#   ./deploy/build.sh              ビルドして deploy/dist/ に置くだけ (root 不要)
#   sudo ./deploy/build.sh install ビルドして /opt/voicerelay/ に入れ、サービスを入れ替える
#
# 詳細な初回セットアップ手順は docs/deploy.md。
#
# **glibc のバージョン差に注意。** このバイナリは動的リンクなので、
# ビルドしたマシンより古い glibc の本番機では動かない。
# ゲームサーバー本体か、同じディストリ・同じバージョンのコンテナでビルドすること。

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$REPO_ROOT/deploy/dist"

PREFIX=/opt/voicerelay
SERVICE=voicerelay
SVC_USER=voicerelay

MODE="${1:-stage}"

# --- ビルド ---

cd "$REPO_ROOT"
echo "==> cargo build --release"
cargo build --release --workspace

BIN="$(cargo metadata --format-version 1 --no-deps \
        | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/relay"
[ -x "$BIN" ] || { echo "ビルド成果物が無い: $BIN" >&2; exit 1; }

mkdir -p "$DIST"
# 本番での実行ファイル名は voicerelay (unit の ExecStart に合わせる)。
install -m 0755 "$BIN" "$DIST/voicerelay"
install -m 0644 "$REPO_ROOT/deploy/config.example.toml" "$DIST/config.example.toml"
install -m 0644 "$REPO_ROOT/deploy/voicerelay.service"   "$DIST/voicerelay.service"

echo "==> $DIST に置いた"
ls -l "$DIST"

if [ "$MODE" != "install" ]; then
  cat <<MSG

本番機へ入れるには:

  scp -r deploy/dist/ <host>:/tmp/voicerelay-dist
  ssh <host> 'sudo /tmp/voicerelay-dist/... '   # または repo ごと置いて sudo ./deploy/build.sh install

初回は docs/deploy.md の「ユーザーとディレクトリを作る」を先にやること。
MSG
  exit 0
fi

# --- 配置 ---

[ "$(id -u)" -eq 0 ] || { echo "install には root が要る (sudo)" >&2; exit 1; }
id "$SVC_USER" >/dev/null 2>&1 || {
  echo "ユーザー $SVC_USER が無い。docs/deploy.md の初回手順を先に実行する" >&2; exit 1; }

echo "==> unit を更新"
install -m 0644 "$DIST/voicerelay.service" "/etc/systemd/system/$SERVICE.service"
systemctl daemon-reload

# 走っているバイナリを直接上書きすると ETXTBSY で失敗する。
# 別名で置いて mv (rename) すれば atomic に差し替わるが、
# systemd 側の再起動は結局要るので素直に止める。
echo "==> サービスを止める"
systemctl stop "$SERVICE" 2>/dev/null || true

echo "==> バイナリを配置"
# /opt/voicerelay は unit の ProtectSystem=strict で read-only になる。
# root が置く場所であって、サービスが書く場所ではない。
install -d -o root -g root -m 0755 "$PREFIX"
install -m 0755 "$DIST/voicerelay" "$PREFIX/voicerelay"
# ACME のキャッシュ先 (/var/lib/voicerelay) は StateDirectory= が作る。ここでは何もしない。

# config.toml は上書きしない。秘密が入っている。
if [ ! -f "$PREFIX/config.toml" ]; then
  # サービスからは読めればよい (書き込みは ProtectSystem=strict で塞がっている)。
  install -o root -g "$SVC_USER" -m 0640 \
    "$DIST/config.example.toml" "$PREFIX/config.toml"
  echo "!! $PREFIX/config.toml をひな形から作った。**編集してから起動すること**"
fi

# 443 を bind する権限は unit の AmbientCapabilities= が渡す。
# **バイナリへの setcap は要らない** (やっても害は無いが、入れ替えのたびに消える)。

echo "==> 起動"
systemctl enable --now "$SERVICE"
systemctl --no-pager status "$SERVICE" || true

cat <<MSG

証明書が取れたかは journal で見る:

  journalctl -u $SERVICE -f | grep -i acme
MSG
