# INSERT ... SELECT の実行（SQL-51・Issue #1733）

親: #1731。構文・束縛は #1732、内容照合ハッシュの文正規化は #1734 の担当。本書は実行部分（内側 SELECT の
評価から挿入まで）の設計判断を記録する。ビヘイビアはポインタ表記のみ（SQL-51・SQL-34・SQL-21・INDEX-4・
RLS-7・RLS-10・RECOVER-10・RECOVER-11）。

## 実行経路

1. `ON CONFLICT` 併用は `0A000`（別 Issue。SQL-49）。
2. 対象表のスキーマ取得（`42P01`。それ以外の失敗は固定文言の `XX000`）。RETURNING は評価より前に束縛する。
3. 内側 SELECT を **書き込むセッションの `PolicyContext`** で既存の読み取り経路
   （`execute_read_statement_with_limits`）へ通して実体化する。独自の走査を持たないため RLS が暗黙に
   適用され、他テナントの Private 行・不可視行は材料にならない。
4. 1 段目の上限: 結果行数が `--max-result-rows`（matview と同じ `cap`）を超えたら `54000`。
5. 結果行を `Vec<BoundInsert>` へ変換する（`sql/insert_select.rs`）。省略列は DEFAULT 補完、
   `DEFAULT nextval` 系の印は COPY と同じく `0A000`。
6. 2 段目の上限: `--max-insert-rows` と INDEX-4 のバッチ上限を挿入前に判定する（`54000`）。
7. 書き込みは複数行 `VALUES`・COPY と同じ `sql::exec::execute_insert_batch_with_schema_in`
   （RETURNING は `execute_insert_returning_in`）。1 write txn・1 台帳エントリで、失敗は txn の abort で
   全体が拒否される。0 行は書き込みも台帳記録もしない。

## スナップショットの判断

- autocommit: 内側 SELECT は確定済みスナップショットで評価し、挿入は `WriteTarget::Autocommit`
  （既存の INSERT と同じ。DEFERRED の FK 検査を含む）で行う。全行を実体化してから書くため、自己参照
  （`INSERT INTO t SELECT ... FROM t`）でも挿入行を同じ文が読まない。
- 明示トランザクション: 共有 write txn を読み取り源にも書き込み先にも使う（自トランザクションの未 commit
  変更が見え、ROLLBACK で破棄される）。
- 採らなかった案: autocommit で自前の write txn を開き `WriteTarget::InTxn` を渡す方式。`InTxn` は
  DEFERRED の FK 検査を COMMIT へ先送りするため、autocommit で検査が抜ける（fail-open）。writer gate を
  保持したまま評価する方式（文単位の呼び出し側所有 txn で FK は全検査）は `tenant.rs` の P0 経路の変更を
  伴うため本 Issue では採らない。要判断: spec の操作欄が「書き込み txn のスナップショットで評価」と
  読めるため、人間の確認が要る。

## 現状の結線（分岐 B）

着手時点で #1732 が未マージだったため、本 Issue の型・処理は新規ファイルにだけ置き、`ParsedSql` への
結線はしていない（`parser.rs`・`allowlist.rs` に触れず衝突を避けるため）。受け口は
`core/insert_select_exec.rs` の `InsertSelectRequest`（表名・列リスト・検証済み内側 SELECT・
`operation_id`・RETURNING・ON CONFLICT 有無）。結線は #1732 のマージ後に、autocommit 腕・明示トランザクション腕
（`mark_written` を含む）・Describe・時刻/採番スコープ判定の 4 点へ行う。LIMIT 省略時の `cap+1` の付与は束縛側の
責務で、実行側も結果行数を `cap` と照合して超過を `54000` にする。

## 台帳（#1734 への引き継ぎ）

内容照合ハッシュは既存のバッチ経路（行の値ベース）をそのまま使う。同じ `operation_id` での二重挿入は台帳キーで
防がれるが、内側の結果が変わった後の同じ文の再送は #1734 がハッシュを文の正規化へ置き換えるまで `22023`
になる（拒否側。fail-open ではない）。ハッシュを渡す場所は predicate DELETE の `content_hash_value`
引数の前例に倣う想定で、本 Issue では引数を追加していない。

## 値の変換（代入変換）

VARCHAR/CHAR/ENUM/JSON は VALUES 形と同じ束縛関数で再検証し、整数幅は範囲検査（`22003`）、NUMERIC は
`apply_numeric_typmod`、整数 → 浮動小数は拡大のみ受理する。配列・浮動小数 → 整数・型族をまたぐ組は
`0A000`（固定文言）で、静的な型照合（`42804`）は #1732 の担当。
