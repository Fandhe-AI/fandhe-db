# 生成列（`GENERATED ALWAYS AS (<式>) STORED`）の設計（Issue #3326）

## ステータス・対象

- ステータス: 決定（設計記録。本 Issue ではコードを変えない）
- 対象: 式の制限、カタログの永続形、書き込み経路ごとの計算点、台帳との関係、依存検査
- spec のポインタ: TABLE-50 (1)〜(5)・RECOVER-11・TASK-345（`docs/spec/04-behavior/` 配下。本文は転記しない）
- 実装: 親 #3325 の子 #3327〜#3332 と #3355。「要確認」は自動運転下の判断で、PostgreSQL 17（以下 PG17）の実機と突き合わせる担当の Issue を併記する

## 背景

- 現状は `sql::allowlist::Parser::parse_identity_clause` が `AS` の直後の `(` を見て、固定文言の `0A000`（`generated columns are not supported`）で拒否する。
- 設計記録で生成列に触れているのは [sequence-namespace-and-ledger.md](sequence-namespace-and-ledger.md) §13.1 の 1 行だけで、実装の前提が無い。
- 本記録は、子 Issue が迷わず実装できるように、決定を 1 か所へ集める。`0A000` の挙動は #3327 まで変わらない。

## 現状（設計の根拠。行番号は書かずシンボルで参照する）

| 事項 | 現状 |
| ---- | ---- |
| 列定義 | `catalog::ColumnDef`（全フィールドが `pub`）。`sql/ctas.rs`・`sql/matview.rs`・crate 内テストが構造体リテラルで構築する |
| カタログ版 | 最新は `CATALOG_FORMAT_VERSION_V15`。各版は互いに排他な正規形で、旧版のバイト列は変えない。前方互換は持たない |
| 版追加の罠 | 新しい版を `FK_BEARING_FORMAT_VERSIONS` へ追記しないと FK 検査が fail-open になる（P0 注記あり）。`catalog_value_references_enum_type` も新しい版を認識する必要がある（CHECK v7 の前例） |
| 決定性検査の前例 | `sql/index_expr.rs`（INDEX-10）の `check_expr_with`・`is_known_non_immutable_name`・`IndexExprSite`・`bind_index_expression` |
| 正規描画 | `sql/check_constraint.rs` の `render_expr`（第 2 のレンダラーは作らない） |
| 代入の束縛・変換 | `sql/set_expr.rs` の `bind_set_expr`・`eval_with_scalars`・`expr_value_to_column_value`（2^53 を超える BIGINT は `22003`） |
| 検査点 | `constraint::enforce_row_constraints_in_txn` は行を書いた後に読み戻し、RLS WITH CHECK → key_index → CHECK → 式索引 → 部分索引 → UNIQUE → FK の順で検査する |
| 428C9 | `ErrorClass::GeneratedAlways`（IDENTITY ALWAYS 用。HTTP は 400）が既にある。`VALUES (DEFAULT)`・`SET c = DEFAULT` も実装済み |
| 台帳 | 省略列に適用した DEFAULT は内容照合ハッシュの材料から外す（[sequence-namespace-and-ledger.md](sequence-namespace-and-ledger.md) D2） |

## 決定

### G1. 構文と組み合わせ（構文段）

- 受理する形、併記できる列制約、拒否する組み合わせと各 SQLSTATE は TABLE-50 (4)・(5) のポインタとして参照し、本記録へは転記しない（公開範囲の方針は [spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）。
- 実装の割り当てだけを決める: CREATE TABLE は #3327、`ALTER TABLE ADD [COLUMN]` は #3330。構文段の判定はカタログを引かないので存在オラクルにならない。IDENTITY の組み合わせ規則（§13.1）と同じ作法で、拒否は固定文言にする。
- `id`・`tenant_id`・`visibility` は予約列名のまま。
- 未確定の分類（FK の参照アクションとの組み合わせ等）は PG17 の実機と突き合わせる（要確認。#3327）。

### G2. 式の制限（判定順を固定する）

SQLSTATE を含む個別の規則は TABLE-50 (1) のポインタとし、本記録へは転記しない。実装の方式だけを決める。

1. 構文段: `index_expr::check_expr_with` を共有し、`IndexExprSite` に `Generated` を足して文言だけ切り替える。非決定的な式・サブクエリ・集計等の拒否はここで行う。式索引の前例との SQLSTATE の差は PG17 実機と突き合わせる（#3332。要確認）。
2. 組み込み以外の関数（セッション UDF・WASM UDF・カタログ関数）は決定性にかかわらず拒否する。本体の実行が未実装で、書き込みごとの再束縛で評価できず fail-open になるため。式索引の UDF 揮発性判定（#2217 の保留）は行わない。
3. 束縛段（DDL 権限ゲートの後・write txn 内）: 未知の列・生成列自身や他の生成列の参照・疑似列 `id`・VECTOR 列の参照を拒否する。他の行・他の表は構文上作れない。
4. 代入の型: `set_expr::bind_set_expr` と同じ判定を通す。
5. 型依存の決定性の再判定: `index_expr` の束縛後の判定表を共有する。
6. 正規描画: `render_expr` で描画し、再解析して元の式と一致することを確かめる。不一致は拒否する。生の SQL 原文は保存しない。
7. 参照列を抽出する（依存検査と UPDATE の再計算に使う）。
8. 上限: 式テキストは `MAX_CHECK_PREDICATE_SQL_LEN` と同値、ノード数・深さは `MAX_EXPR_NODES`／`MAX_EXPR_DEPTH` を再利用し、参照列数にも上限を置く。確保より前に検査する。

### G3. 評価器

- 評価は `eval_with_scalars`、列値化は `expr_value_to_column_value`（単一の変換器）。第 2 の評価器・変換器は作らない。
- 2^53 を超える BIGINT は `22003`。PG17 との既知の差として記録する。
- 評価エラー（`22012`・`22003` 等）は文全体を失敗させる。commit しないので行・台帳・世代に痕跡は残らない。応答に値・行 id・テナントを載せない。

### G4. 永続形

- メモリ上の表現: `ColumnDef` に `pub(crate) generated: Option<GeneratedColumnDef>`（正規描画と参照列）を足す。公開は読み取り用アクセサ `generation_expression() -> Option<&str>` だけ。Rust API から任意の SQL テキストを差し込めないよう構築は crate 内に限る（CHECK の `with_checks` の前例）。構造体リテラル（ctas.rs・matview.rs・テスト）には `generated: None` を足す。CTAS・マテリアライズドビューは通常の列を作る（PG と同じ）。workspace 外にリテラル構築は無い。公開 API の破壊的変更になるかの最終判断は #3327。
- カタログ版: 実装時点で未使用の次の版（本記録の時点では v16。#3327 の着手時に open PR を確かめ直す）。v15 本体の上位集合とし、末尾に `gens:<n>`（`n >= 1`）と `n` 行の `gen:<列名>:<参照列 CSV>:<hex(正規描画)>` を足す。
  - 列行に第 8 フィールドは足さない（列行を読む軽量スキャナを変えない）。
  - 生成列を持たない表はバイト列が不変（既存ゴールデンは不変）。旧バイナリは新しい版を未知の版として拒否する。
  - デコードは fail-closed で、CHECK の D2 と同じ順（件数 → 行の形 → 識別子 → 参照列数 → hex 長 → hex → UTF-8）。参照列の実在と生成列どうしの参照の不在は `validate_schema` が再検証する。
  - **#3327 の必須作業**: 新しい版を `FK_BEARING_FORMAT_VERSIONS` へ追記する。`catalog_value_references_enum_type` に認識させる。`CATALOG_FORMAT_VERSION_V15` を判定する全箇所を grep で洗い出して追随させる。
- 行のバイト列は変えない。生成列は通常の列と同じスロットに計算済みの値を持つ。行形式・バックアップ・PITR・変更ログ・レプリカは変更不要で、レプリカは再計算せず計算済みの値を適用する。
- 漂流の扱い: 保存済みの式が再解析・再束縛・往復一致に失敗したら、書き込みを `XX000`（`CorruptSchema`）で拒否する。読み取りは保存済みの値を返すだけなので影響しない。
- ダンプ: `export_schema_ddl` は `GENERATED ALWAYS AS (<正規描画>) STORED` を出力し往復できる（#3327）。

### G5. 計算点

採用: **符号化の直前で計算し、検査点で照合する**。

- 計算は 1 つの関数（例: `sql::generated_column::compute_into`）だけが行い、全経路が呼ぶ。位置は、SEQUENCE の採番・rowid の払い出し・BEFORE ROW トリガの連鎖・UPDATE の SET の適用がすべて済み、行の値が確定した直後・`encode_scalar_columns` の直前。
- `enforce_row_constraints_in_txn` の先頭（RLS WITH CHECK より前）に照合段を足す。生成列を持つ表に限り、保存した値を再計算して比較し、不一致は `XX000`（固定文言）で拒否する。結線を漏らした経路を fail-closed で検出する多層防御で、生成列の無い表のコストはゼロ。

却下: 検査点の中で読み戻して書き直す案。理由は次のとおり。

- RETURNING 用の `CapturedRow` は書き込み時点の値で捕捉されるため、計算前の値を返してしまう（TABLE-50 (4)）。
- AFTER ROW トリガも計算前の NEW を見る。
- 1 行 2 回書きになり、変更ログに中間像が残り I/O も倍になる。
- 生の `RowInput` 経路（metadata バイト列）には値の列が無い。

文の中の順序（PG の ExecInsert と同じく、計算は WCO・制約より前）:

1. 束縛: 生成列への明示値は、型の束縛より前・txn より前に `428C9`。`DEFAULT` は省略と同じ「送られていない列」として受理する。
2. content_hash を計算し `ledger::record_in_txn` で照合する。一致する再送は、計算の前に `23505`（TABLE-50 (5)）。
3. 計算前の NOT NULL 検査（`reject_missing_not_null`）から生成列を外す。生成列の NOT NULL は計算後に `23502`。
4. BEFORE STATEMENT → SEQUENCE の採番 → rowid の払い出し → BEFORE ROW の連鎖。
5. 生成列を計算する → 生成列の NOT NULL を検査する。
6. 符号化して書き込む。RETURNING と変更ログの捕捉は計算後の値を見る。
7. 検査点: 照合段 → RLS WITH CHECK → key_index → CHECK → 式索引 → 部分索引 → UNIQUE → FK。
8. AFTER ROW と AFTER STATEMENT。

### G6. 書き込み経路と担当

| 経路 | 担当 | 備考 |
| ---- | ---- | ---- |
| SQL INSERT（単一行・複数行・`INSERT ... SELECT`） | #3328 | 明示値は `428C9`、`DEFAULT` は受理 |
| UPDATE（リテラル・式・述語形・分割実行 DML） | #3328 | SET の生成列は `428C9`（`DEFAULT` は受理） |
| UPSERT（挿入側・`DO UPDATE SET`） | #3328 | `EXCLUDED.<生成列>` の値は PG17 実機で確認（要確認） |
| COPY FROM | #3329 | 列リストに生成列を含めると `428C9`（TABLE-50 (2)。PG17 実機の SQLSTATE と突き合わせ。要確認）。列リスト無しの COPY が生成列を除いた並びを期待するかも確認 |
| NoSQL insert・update・upsert | #3329 | 生成列のキー指定は `428C9`（HTTP 400） |
| 自動更新可能ビュー・INSTEAD OF トリガ経由の基表書き込み | #3329 | 基表の経路と同じ計算を通る |
| BEFORE ROW トリガの後・`TriggerWriteHandle`・PL/pgSQL・DO ブロックの DML | #3329 | NEW の生成列への代入は計算で上書き（PG17 実機で確認）。BEFORE トリガの WHEN が NEW の生成列を参照する場合の拒否も確認 |
| FK の参照アクション（`RowPolicyWrite::Internal`。子表の CASCADE・SET NULL・SET DEFAULT が生成列の参照列を書き換える） | #3329 | Issue 本文に記載が無い。受入条件への追記は承認待ち |
| ファイル形 INSERT・生の `RowInput` の Rust API | #3329 | 値の列を持たない生の metadata 経路は、生成列を持つ表で `0A000`（fail-closed） |
| `ALTER TABLE ADD COLUMN ... GENERATED` のバックフィル | #3330 | G8 |
| 参照しない列の `ALTER COLUMN TYPE` の行の書き換え | #3331 | 生成列の値は変わらない。照合段を通ることを確かめる |
| MERGE | #3355 | 本記録の計算点を使う |

暫定の状態: #3327 は、生成列を持つ表へのすべての書き込みを固定文言の `0A000` で拒否する（黙って NULL を書かない）。経路ごとに、担当の子 Issue が結線した時点でこの拒否を外す。結線漏れは G5 の照合段が `XX000` で止める。

### G7. UPDATE・UPSERT・トリガ・RETURNING

- UPDATE は、更新した各行の生成列を、SET の適用と BEFORE UPDATE の連鎖の後に再計算する。式は決定的なので参照列が変わらない行では値も変わらない。#3328 は、参照列が SET の対象に無く BEFORE トリガも無いときに再計算を省く最適化をしてよい（観測上は同じ結果）。
- RETURNING・AFTER トリガ・索引・UNIQUE・CHECK・FK は計算後の値に対して働く（TABLE-50 (4)）。

### G8. ADD COLUMN のバックフィル（#3330）

- 1 つの write txn で、全テナント・全可視性の既存行を計算して書き直す。`PolicyContext` で絞らない。前例は ALTER COLUMN TYPE の全行書き換え（`column_rewrite`・[alter-table-drop-modify-column.md](alter-table-drop-modify-column.md)）と [alter-table-check-constraint.md](alter-table-check-constraint.md) の D6。
- 評価エラーが出たら、カタログの変更も含めて commit しない。エラー文言は `redact_row_conversion_error` と同様に SQLSTATE を保った固定文言とし、テナント・行・値を載せない（RLS-9・RLS-10 (c) のポインタ）。
- 時間とメモリは既存の全行書き換えと同じ上限に従う。fast default（`fill_value`）は使わない。
- `crash_test_ddl.sh` に ADD COLUMN 途中の SIGKILL の段を足し、再起動後に「カタログだけ変わった状態」も「一部の行だけ書き直された状態」も残らないことを検査する。

### G9. 依存検査（#3331。TABLE-50 (3)・TABLE-38 のポインタ）

| 操作 | 決定 |
| ---- | ---- |
| 参照列の `DROP COLUMN` | `2BP01`。CHECK と同じ段で、索引宣言の掃除より前に判定 |
| `DROP COLUMN ... CASCADE` | 現在は構文段で `42601`。生成列の連鎖削除として受理するかは #3331（TABLE-50 (3)） |
| 参照列の `RENAME COLUMN` | 同じ txn で正規描画を書き換え、往復一致を再検証（CHECK と #1651 の前例） |
| 参照列の `ALTER COLUMN TYPE` | `0A000`。固定文言 `cannot alter type of a column used by a generated column`（ビューのゲートの前例）。#3331 の本文にある「再検証に通れば許す」案は採らない |
| 生成列自身の `ALTER COLUMN TYPE`・`SET EXPRESSION`・`SET DEFAULT` | まず `0A000`／`42601`（PG17 の分類は要確認）。PG17 との同等化は #3331 |
| `DROP EXPRESSION` | 値を保ったまま通常の列にする（TABLE-50 (3)）。#3331 |
| 生成列自身の DROP・表の改名・参照しない列の変更 | 影響なし（式は修飾名を持たない） |

### G10. 台帳・crash（#3332。RECOVER-11・TABLE-24 (3) のポインタ）

- `content_hash` の材料に生成列の値を入れない（既存 golden は不変）。再送の扱いは [sequence-namespace-and-ledger.md](sequence-namespace-and-ledger.md) の台帳契約に従う。
- 行・台帳・世代は 1 つの write txn で commit し、crash 後に中間状態を残さない。検査は #3332。

### G11. エラー契約

- 新しい `wire_code`・`ErrorClass` は追加しない。既存の分類（`InvalidObjectDefinition`・`GeneratedAlways` 等）を使い、個別の割り当ては各子 Issue の実装で TABLE-50 のポインタに従う。
- 応答に載せるのは、利用者自身が書いた列名だけ。

### G12. テナント境界

- 式は書き込んだセッションのテナント文脈で評価し、参照するのは同じ行の値だけ。カタログ・他の行・`visible()` は参照しない（TABLE-50 (5)）。
- 構文段の判定はカタログを参照しない。束縛段は DDL 権限ゲートの後にだけ到達する。

## 却下案

- 検査点の中で読み戻して書き直す（G5）。
- 列行へ第 8 フィールドを足す永続形（G4。軽量スキャナを変えたくない）。
- 式索引と同じ UDF 揮発性判定（G2。本体の実行が未実装）。
- 参照列の `ALTER COLUMN TYPE` を再検証つきで許す案（G9）。

## PostgreSQL との差（既知・要確認）

| 項目 | 差 | 確認の担当 |
| ---- | -- | ---------- |
| 2^53 を超える BIGINT | 評価で `22003` | 既知の差 |
| IMMUTABLE な UDF | `42P17` | 既知の差 |
| 式の SQLSTATE（サブクエリ・集計等） | 式索引の前例と異なる可能性 | #3332 |
| `id` 参照・COPY 列リスト・`EXCLUDED.<生成列>`・NEW への代入 | PG17 実機で確認 | #3329・#3328・#3332 |

## 対象外（受け皿）

- 宣言とカタログ: #3327
- INSERT・UPDATE・UPSERT: #3328
- COPY・NoSQL・ビュー・トリガ・FK の参照アクション・生の RowInput: #3329
- ADD COLUMN: #3330
- 依存検査と生成列自身の ALTER: #3331
- 台帳・crash・越境・PG17 の突き合わせ: #3332
- MERGE: #3355
- 一時表での生成列: `0A000` のまま。受け皿は未起票（起票は承認待ち）

## セキュリティ

- インジェクション: 保存するのは正規描画だけで、再解析と往復一致を必須にする。構築は `pub(crate)` に限る。
- テナント境界: 同じ行だけを参照する。バックフィルのエラーは固定文言で、他テナントの存在・値を漏らさない。
- fail-closed: 未結線の経路は `0A000`、結線漏れは照合段の `XX000`、漂流した式は `XX000`、旧バイナリは新しい版を拒否する。
- DoS: 式の長さ・ノード数・参照列数を確保より前に検査する。
- 依存・unsafe: 追加なし。
