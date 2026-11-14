# ビューへの INSTEAD OF ROW トリガ（Issue #3193。TABLE-45・TABLE-43・RLS-10 ポインタ）

`CREATE TRIGGER ... INSTEAD OF {INSERT | UPDATE | DELETE} ON <ビュー> FOR EACH ROW` を受理し、ビューへの
`INSERT`／`UPDATE`／`DELETE` を行ごとにトリガ関数へ振り替える。トリガ本体の書き込みは基表に対して通常の
RLS（呼び出しテナントの `PolicyContext`）で、外側の文と同じ write txn の中で実行する（既存の
`TriggerWriteHandle`・[trigger-execution-model.md](trigger-execution-model.md) の D9 の契約のまま）。
spec は TABLE-45・TABLE-43 のポインタのみ（本文は転記しない）。

## 決定事項

- **D1 定義の対象**: テナントの SCHEMA 配下のビュー（内部キー `#<id>.<name>`）に限る。共有 public のビュー
  （裸の名前）は定義時に `0A000`（共有 public 表と同じ理由。トリガの scope が作成テナント単位で、共有
  オブジェクトへの全テナントの書き込みを表現できない。Issue #3361）。発火時の保険（`PlpgsqlTriggerInvoker`）も
  ビューにそのまま効く。
- **D2 形の制約（PostgreSQL に合わせる）**: `FOR EACH ROW` 必須（省略・`FOR EACH STATEMENT` は `0A000`）、
  イベントは INSERT／UPDATE／DELETE（TRUNCATE は `0A000`）、`UPDATE OF <列>`・`WHEN` は `0A000`。表への
  INSTEAD OF は `42809`。ビューへの BEFORE／AFTER は従来どおり `42809`。判定順は構文 → 権限 → 明示 Tx →
  SCHEMA の現存 → 対象の種別 → 対象の存在 → 共有 public → 関数 → 同名 → 件数上限（他テナントの定義の有無に
  依存しない）。
- **D3 優先順位**: 書き込みテナント自身の scope に該当イベントの INSTEAD OF トリガがあれば、更新可能性の
  判定（`view_updatability`）より先にトリガへ振り分ける（更新可能ビューでもトリガを優先。PostgreSQL と同じ）。
  無ければ従来経路（自動更新可能なら基表へ書き換え、不可なら `55000`）。判定は書き込みテナントの scope の
  カタログだけで行い、存在情報を応答の差に出さない。
- **D4 対応する文の形**: `INSERT ... VALUES`（複数行・列リスト指定）、単一行形・述語形の `UPDATE`／
  `DELETE`（`WHERE` なしを含む。`WHERE` 内のサブクエリは読み取り経路が受理する範囲）。次は固定文言の
  `0A000`: `RETURNING`・`ON CONFLICT`・`PARTITIONED`・`USING OPERATION_ID`（外側の台帳エントリの付け先を
  持たないため）・列リストを省略した INSERT（ビューの公開列での展開が未対応。#1730 の既存方針）・揮発的な値
  （`VALUES` 内の `now()` 等の時刻関数や `nextval`。書き込み段での遅延適用が要るため）。
  `INSERT ... SELECT`・`UPDATE ... FROM`・`DELETE ... USING` は従来どおりビューを受けない。
- **D5 行意味論**: トリガ関数が `NULL` を返した行は処理済みに数えない（影響行数に入らない）。複数の
  INSTEAD OF トリガは名前のバイト順に連鎖し、INSERT／UPDATE は前のトリガが返した行が次の NEW になる。DELETE は
  返却行を使わない。`TG_WHEN` は `INSTEAD OF`、`TG_LEVEL` は `ROW`、`TG_TABLE_NAME` はビュー名、NEW／OLD の列は
  ビューの公開列。
- **D6 ビューの行型**: `CREATE OR REPLACE VIEW`・matview と同じ導出器（`view_output_columns`・
  `column_defs_from_view_columns`）でビューの公開列（列別名適用後）と型を得て `TableSchema` を合成する
  （採番方式の形。疑似列 `id` を持たない。公開名 `id` は通常の利用者列）。型を表現できない列（静的な型を持たない
  式・ENUM・HALFVEC 等）を含むビューは `0A000`（fail-closed）。
- **D7 OLD 行**: UPDATE／DELETE の対象行は、文の `WHERE` 以降のトークンを再利用した `SELECT * FROM <ビュー>
  [WHERE ...]` を、**外側の write txn を読み取り源**にして既存の読み取り経路で実行して得る（SQL 文字列は
  組み立てない）。明示 Tx では自 Tx の未 commit の変更が見え、評価は呼び出しセッション自身の `PolicyContext`
  で行うため、作成者の可視性・他テナントの行は材料にならない（RLS-10 (b)）。結果行数は既存の結果行数上限と
  `--max-dml-affected-rows` で頭打ちにする。UPDATE の NEW は OLD に `SET` を適用して作る（式は更新前の行に
  対して評価。既存の `bind_set_assignments`・`eval_set_expr_to_value` を共有）。
- **D8 原子性・台帳**: 文全体は単一 write txn・単一 commit。トリガ内の例外は文全体の取り消し（autocommit）／
  Tx の `Failed`（明示 Tx）。トリガが書いた表は、明示 Tx では `mark_written`（COMMIT 時の遅延 FK 検査）、
  autocommit では文の終わりに遅延 FK を検査する。ビュー自体は行を持たないため台帳エントリは付けない
  （`USING OPERATION_ID` は D4 で `0A000`）。既知の制限: `--require-operation-id` 構成ではビューへの書き込みに
  `operation_id` が必須のままで（構文段の判定）、INSTEAD OF 経路は `operation_id` を受けないため事実上書けない。
- **D9 カタログ blob**: `TriggerTiming::InsteadOf` を timing タグ `2` として追加する（blob の版は `1` のまま。
  加法的）。このタグを知らない旧バイナリは `CorruptSchema` で fail-closed に拒否する（旧バイナリで INSTEAD OF を
  含む DB を開くとトリガ表の読み出しが失敗する）。`validate_shape` は INSTEAD OF の ROW 必須・`UPDATE OF`／
  `WHEN` なしを decode 時にも検査する。

## 実装の所在

| 項目 | 場所 |
| ---- | ---- |
| 構文（`INSTEAD OF`・形の制約） | `sql/allowlist/trigger.rs` |
| カタログ（timing タグ・種別判定・DROP の対象） | `catalog/trigger.rs` |
| ビュー削除時のトリガ掃除 | `catalog.rs` の `drop_view_in_txn`・`drop_view_with_behavior`（`DROP SCHEMA ... CASCADE` を含む） |
| 論理ダンプ（ビューの CREATE の後の PostData） | `catalog/ddl_export/access.rs` |
| 発火計画・連鎖実行 | `trigger/plan.rs`（`plan_instead_of`）・`trigger/fire.rs`（`fire_instead_of_row`） |
| 振り分けと実行本体 | `core/view_instead_of.rs` |
| 構文解析段の変更 | `core.rs`（自動更新不可の `55000` を実行段・Describe へ移動） |

## 構文解析段の変更

従来は自動更新不可のビューへの INSERT／UPDATE／DELETE を Parse で `55000` にしていた。トリガの有無は書き込み
テナントの scope で決まり、テナントを持たない構文解析段では引けない（引いて応答を変えると他テナントの定義の
有無が観測できる）ため、`55000` の判定を実行段（`execute_view_*` の書き換え）と Describe（セッション自身の
テナントの scope を見る `describe_view_instead_of`）へ移した。トリガが無ければ同じ `55000` が実行・Describe で返る
（拡張クエリでは Parse ではなく Bind／Describe／Execute で返る点だけが変わる）。

## 未対応の形

- `RETURNING`・`ON CONFLICT`・`PARTITIONED`・`USING OPERATION_ID`・列リスト省略の INSERT・`UPDATE OF`・`WHEN`・
  文トリガ・共有 public ビュー（上記 D1・D4）。
- `INSTEAD OF` の `CREATE TRIGGER` を明示 Tx 内で実行する形（既存の DDL と同じく `0A000`）。
