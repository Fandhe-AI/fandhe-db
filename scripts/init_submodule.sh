#!/usr/bin/env bash
# docs/spec サブモジュールを初期化・更新する（`make submodule` の実体）。
# docs/spec（fandhe-db-spec）は private リポジトリのため、アクセス権のない環境では
# 取得に失敗する。実装コードのビルド・テストは docs/spec 抜きでも成立させる方針
# （README「開発環境構築」）のため、失敗しても警告のみで成功終了する。
set -euo pipefail

git submodule update --init || {
    echo "warning: failed to fetch docs/spec (private). Expected without access; build and tests work without the spec" >&2
    true
}
