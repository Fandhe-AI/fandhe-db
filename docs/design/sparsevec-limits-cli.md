# SPARSEVEC の非ゼロ数上限の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #2027・TABLE-47 (2)／SQL-46 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--max-sparsevec-nnz`
- 先行: [regex-limits-cli.md](regex-limits-cli.md)（Issue #1786）・[jsonpath-limits-cli.md](jsonpath-limits-cli.md)（Issue #2184）と同型。列型の本体は [column-type-extension.md](column-type-extension.md)（Issue #2025 追記）

## 背景

`SPARSEVEC(N)` 列へ書き込む値の非ゼロ要素数は、Issue #2025 でハード上限 `16,000` に固定された。値が大きいほど解析と保存のコストが増えるため、運用者が他の資源上限と同じ方式で起動時に下げられるようにする。

## 決定

| # | 内容 |
| - | ---- |
| D1 | ハード上限 `16,000` を既定値とし、据え置く。設定できるのは**下げる方向だけ**（範囲は `1..=16000`）。リテラル長上限（512 KiB）と wire のメッセージ上限が 16,000 要素を前提に寸法を決めていること、decode と行 codec がハード上限で検査していることから、引き上げは安全弁を外すことになるため対象外 |
| D2 | 値の運搬は `regex_limits`／`jsonpath_limits` と同型。`engine::sparsevec_limits::SparseVecLimits`（`Copy`・フィールド非公開・ハード上限で飽和）を `EngineCore::with_sparsevec_limits` で起動時に 1 回注入する。範囲検証は engine の `validate_max_nnz` に一本化し、wire-server は CLI 文字列を厳密パースして渡すだけ |
| D3 | 束縛時に値を参照するため thread-local の RAII スコープ（`SparseVecLimitsScope`）を使う。`parse_sparsevec_literal` はスコープの値を読み、スコープが無ければハード上限へ戻る。このフォールバックは設定を下げた場合に fail-open になるため、スコープは SQL の文実行入口だけでなく、NoSQL／Rust API の束縛クロージャを呼ぶ全経路（`execute_bound_*`）と、`bind_insert_form`／`bind_update*`／`bind_predicate_update` を呼ぶ内部経路にも張る。一時表用のコア（`for_temp_session`）も設定を引き継ぐ |
| D4 | 上限の検査はテキスト入力から値を作る時点（`parse_sparsevec_literal`）だけ。行 codec の encode／decode はハード上限のまま。上限を下げて再起動しても既存行は読め、他の列の UPDATE による再 encode も拒否されない。下げた値は新しく書き込む値にだけ効く（SQL-46 (5)） |
| D5 | フラグ名は `--max-sparsevec-nnz <1-16000>`。範囲外・非数値・値欠落は起動エラー。2 回目以降の指定は fail-closed（last-wins にしない）。解決は bind・ユーザーストア読込より前。環境変数やセッション・SQL・テナントから変える経路は作らない（全テナントに同じ値が効く）。超過は SQLSTATE `54000` |

## 対象外

- ハード上限の引き上げ
- セッション単位の `SET` による変更
- `vector`↔`sparsevec` の CAST（TABLE-47 (3)。実装時は同じ `current_sparsevec_limits()` を使う）
- `INSERT ... SELECT` で既存の値をそのまま複製する場合の検査（テキスト入力の構築を経ないため本 Issue では検査しない。D4 の帰結。必要なら別 Issue で判断する）
