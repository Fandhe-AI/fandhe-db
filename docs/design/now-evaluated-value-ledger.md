# ADR: now() 系の評価済み値の台帳保存と内容照合ハッシュへの影響（Issue #1585）

- ステータス: Proposed
- 対応 Issue: #1585（親 #1584。後続 #1586・#1587・#2188）
- 関連ポインタ: `docs/spec/04-behavior/sql-surface.md`（SQL-37）・RECOVER-10・RECOVER-11・
  RECOVER-12・RECOVER-14・RECOVER-15・TABLE-16・TABLE-32・TABLE-33・TASK-248。spec 本文は
  転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/recovery/ledger.rs`・`recovery/content_hash.rs`・
  `recovery/partitioned_job.rs`・`sql/const_expr.rs`・`sql/parser.rs`（`fill_omitted_columns`・
  `bind_insert`）・`sql/copy.rs`・`tenant.rs`・`catalog.rs`（`ColumnDefault`）・
  `sql/transaction.rs`
- 検証コード: なし（本 Issue は設計記録のみ）

## 1. 背景・目的

現在時刻関数（`now()`・`CURRENT_TIMESTAMP` 等）は未実装で、呼び出しは `42883`、
`INSERT VALUES`・`DEFAULT` では `42601` で拒否される（[datetime-scalar-functions.md](datetime-scalar-functions.md)
「NOW() と決定性」）。SQL-37 の改訂でこれらを受理する方針になったため、実装 Issue
（#1586 式評価器・#1587 DEFAULT now() と再送一貫性・#2188 statement／clock_timestamp と台帳記録）
の前提として、次の 3 点を本書で確定する。

1. 評価済みの時刻値を台帳へどう保存するか
2. `content_hash` への影響
3. 再送（同一 `operation_id`）・クラッシュ復旧・分割実行 DML での一貫性

評価時点は PostgreSQL と同じとする。`now()`・`CURRENT_TIMESTAMP`・`transaction_timestamp()` は
トランザクション開始時刻、`statement_timestamp()` は文開始時刻、`clock_timestamp()` は呼び出し時刻。

## 2. 公開コードの現状（設計の前提）

| 事実 | 所在 |
| ---- | ---- |
| 台帳値は v1（版バイトのみ）か v2（版＋ハッシュ 32 バイト）。未知の版は fail-closed。照合はハッシュ一致で `23505`、不一致で `22023`（keep-first） | `ledger.rs` |
| 式 DML（述語形 UPDATE／DELETE の WHERE・SET、単一行 UPDATE の SET 式）は `Expr` の**構文形**をハッシュする。式タグは 1〜12 使用中（3＝`Call`、2＝列参照）。`OpTag` は 1〜13 使用中 | `content_hash.rs` |
| INSERT／バッチ INSERT／UPSERT は**束縛済みの値**（`row_codec::Value`）をハッシュする。`INSERT VALUES` の定数式は構文段で `InsertLiteral` へ畳み込まれ、省略列の DEFAULT は `fill_omitted_columns` で値へ束縛されてからハッシュ入力に入る | `const_expr.rs`・`parser.rs`・`copy.rs`・`tenant.rs` |
| `CURRENT_TIMESTAMP` 等のキーワード形は現状 `Expr::Ident` として解析され、列参照（タグ 2）と区別できない | `const_expr.rs` |
| 分割実行ジョブ記録 `JobRecord` は版 1 で時刻を持たない | `partitioned_job.rs` |
| `ADD COLUMN ... DEFAULT` は既存行を読み出し時に補完する | `row_codec::default_scalar` |
| 台帳は COMMIT と同じ txn で永続化される（RECOVER-12） | `sql/transaction.rs` |

帰結: 時刻を評価してからハッシュすると再送のたびにハッシュが変わり、同じ文の再送が `22023` になって
冪等性が壊れる。特に INSERT 系の値ベース経路が最大のリスクである。

## 3. 決定

### D1. ハッシュ入力は評価前の構文形（最優先）

時刻値関数を含む書き込みの `content_hash` は、送られた文の構文形（関数呼び出し・キーワードのまま）から
計算する。評価済みの時刻はハッシュに入れない。同じ文の再送は評価時刻によらず同一ハッシュで `23505`、
文が異なれば `22023`。[multi-row-dml-operation-id.md](multi-row-dml-operation-id.md) 4.4 節の
「構文形をハッシュする」方針の延長である。

### D2. INSERT 系の値ベース経路

- 時刻値関数を含む `INSERT VALUES` 要素は `fold_const_literal` で畳み込まず、束縛段まで式として運ぶ。
  評価済みの値は行へ、構文形はハッシュへ渡す。
- 時刻値関数を 1 つ以上含む INSERT／バッチ INSERT／UPSERT に限り、別ドメインの操作種別タグ
  （`InsertExpr`・`InsertBatchExpr`・`UpsertExpr`。番号は `OpTag` の空き番号 14 以降を実装時に確認）を使う。
  `UpdateColumnsExpr`（13）が「式を含む場合だけ別ドメイン」とした先例に従う。
- 新レイアウトは列ごとに種別バイトを置く。`0`＝リテラル値（`push_value`）、`1`＝式（`push_dml_expr`）、
  `2`＝揮発性 DEFAULT の適用（列名＋DEFAULT 式の構文形）。行ごとの列数プレフィクスは
  `for_typed_insert_batch` と同様に保持し、行境界の曖昧性を避ける。
- **後方互換**: 時刻値関数を含まない文のハッシュは既存レイアウトとバイト単位で同一にする（既存台帳エントリとの
  再送互換）。golden テストで固定することを #1587・#2188 の必須条件とする。
- 影響箇所: `bind_insert`／`fill_omitted_columns`、`copy.rs`（COPY は `operation_id` を持つ）、
  NoSQL insert（`bind_insert` 経由）、`tenant.rs` の `for_typed_insert`・`for_typed_insert_batch`・
  `for_typed_upsert` の呼び出し箇所。

### D3. 式の表現とタグ

- `now()`・`transaction_timestamp()`・`statement_timestamp()`・`clock_timestamp()` は既存の
  `Expr::Call`（タグ 3、名前は小文字化）のまま。新タグ不要。
- キーワード形（`CURRENT_TIMESTAMP[(p)]`・`CURRENT_DATE`・`CURRENT_TIME[(p)]`・`LOCALTIMESTAMP[(p)]`・
  `LOCALTIME[(p)]`）は専用 variant（例: `Expr::ValueFunction { kind, precision: Option<u8> }`）とし、
  `push_dml_expr` に式タグ 13 を追加する（`OpTag` とは別の名前空間）。直列化は
  「kind バイト＋精度の有無フラグ＋精度バイト」。同名の列参照（タグ 2）と衝突しない。
- `now()` と `CURRENT_TIMESTAMP` を同一視しない。送った形が違えば別内容（`22023`）とし、
  誤った `23505` を避ける側（fail-closed）に倒す。既存タグ 1〜12 は不変。
- 新構文は許可リスト文法内で `MAX_EXPR_NODES` の範囲に収める。

### D4. 台帳値フォーマット v3

- 形式は `[3][hash 32][txn_start i64 LE（UTC マイクロ秒）]`、41 バイト固定。長さ不一致・未知の版は
  従来どおり fail-closed（`Corrupted`→`XX000`）。
- 時刻値関数を評価した書き込み（式・揮発性 DEFAULT の適用を含む）に限り v3 を書く。それ以外は v2 のまま
  で、既存バイト列とテストを不変に保つ。
- 照合（`23505`／`22023`）はハッシュだけで行い、時刻は使わない。v2 と v3 は相互にハッシュで照合できる。
- 記録する値は評価に用いたトランザクション開始時刻 1 つ（1 エントリ＝1 文）。autocommit では文開始時刻と
  一致し、分割実行は明示トランザクション内で拒否されるため常にこの条件を満たす。
- 再送時は `23505` を返すだけで再評価・再書き込みしない。行には初回 commit の評価済み値が残るため、
  初回の値の再現は行の値で満たされる。
- API 形（`Option<TxnStartMicros>` の追加か `LedgerEntryInput { hash, evaluated_at }` の導入か）は #1587 で決める。
- ダウングレード: v3 を書いた DB を旧バイナリで読むと `XX000`（fail-closed）。移行処理は入れず既知の制約とする。

### D5. clock_timestamp()

呼び出しごとの値は行の値としてのみ残し、台帳にはトランザクション開始時刻だけを入れる。ハッシュは構文形のため
再送は `23505` で冪等性を保つ。「台帳付き書き込みでの clock_timestamp を拒否する」案は PostgreSQL と
意味が外れるため採らない。

### D6. 分割実行 DML

ジョブ記録を版 2 にし、`Running`／`Cancelled`／`Completed` に `fixed_time i64` を持たせる。値はジョブ開始時
（初回チャンク）に確定・永続化し、再開したチャンクは永続化済みの値を使う。WHERE／SET の `now()` は全チャンクで
同一、クラッシュ後も不変で、完了時の台帳 v3 にも同じ値を記録する。版 1 のジョブ記録は時刻を持たないため、
時刻値関数を含む再開要求は `22023`（fail-closed）。

### D7. 評価コンテキスト

トランザクション開始時刻は明示トランザクションでは `BEGIN` 時点、autocommit では文の開始時点で確定し、
`sql/transaction.rs` の状態に保持する。文開始時刻は文ごとに確定する。時計は `SystemTime` からマイクロ秒へ
checked で変換し、範囲外は fail-closed のエラーとする。

### D8. 戻り値の型（暫定）

TIMESTAMPTZ／TimeZone（TABLE-32・TABLE-33）が未実装の間は `ExprType::Timestamp`（UTC マイクロ秒）・`Date`
を暫定型とする。台帳の値は UTC マイクロ秒でタイムゾーンに依存しないため、型の拡充で台帳形式は変わらない。

### D9. DEFAULT now()

`ColumnDefault` に時刻値関数の新 variant と新カタログタグを追加する（未知タグは既存どおり fail-closed）。
定数 DEFAULT は従来どおり値でハッシュする（バイト不変）。

`bind_column_default` の呼び出し経路は実コードで次の 5 つあり、時刻 DEFAULT の扱いを経路ごとに固定する。

| 経路 | 場所 | 方針 |
| ---- | ---- | ---- |
| INSERT／バッチ INSERT／UPSERT の省略列補完 | `parser.rs` の `fill_omitted_columns`（`bind_insert` から） | 適用点。D7 の評価コンテキストを引数で受け取り、評価済みの値は行へ渡し、ハッシュ側へは列名と評価前の DEFAULT 式（構文形）だけを D2 の種別 `2` の材料として渡す（評価値はハッシュに入れない） |
| COPY の省略列補完 | `copy.rs` から同じ `fill_omitted_columns` | 上と同じ（評価値は行へ、列名と評価前の DEFAULT 式をハッシュ側へ渡す）。台帳は COPY の `operation_id` に記録する |
| 文書取り込み（ファイル INSERT）の省略列補完 | `parser.rs` の `bind_file_insert`（`fill_omitted_columns` を通らず専用ループで `bind_column_default` を直接呼ぶ） | 初期実装は時刻 DEFAULT を評価せず `0A000` で拒否（fail-closed）。評価コンテキストを渡し台帳へ記録する対応は後続 Issue で、`fill_omitted_columns` と同じ評価関数へ寄せて行う |
| FK の `ON DELETE／UPDATE SET DEFAULT` | `constraint.rs` の `apply_referential_action` | 参照アクションは親の文に連鎖した子行の書き換えで、台帳に記録する主体の文が無い。初期実装は時刻 DEFAULT 列が SET DEFAULT の対象になる FK の宣言・実行を `0A000` で拒否（fail-closed）。値を黙って NULL や定数へ差し替えない |
| DDL 時の DEFAULT 検証 | `ddl.rs`（`CREATE TABLE`・`ALTER COLUMN SET DEFAULT`） | 評価せず型適合だけを検査する。時刻 DEFAULT は列型が時刻型であることを確認する |

拒否は新しい `bind_column_default` 呼び出し側ではなく、時刻 DEFAULT variant を受けた時点の共通関数で
未対応経路のコンテキストが無いことを検出して行い、経路の追加で拒否が抜けないようにする（既定が拒否の fail-closed）。

### D10. ALTER TABLE ADD COLUMN ... DEFAULT now()（判断項目）

読み出し時補完では読むたびに値が変わるため、そのままは不可。推奨案は PostgreSQL の fast default 相当で、
ADD COLUMN 実行時に評価した値 1 つを既存行の補完値としてカタログに固定し、新規行は DEFAULT 式で評価する。
`clock_timestamp()` のような呼び出しごとの DEFAULT は全行書き換え経路が無いため `0A000`。
判断が確定するまでの既定は、ADD COLUMN での時刻値関数 DEFAULT の拒否（fail-closed）。CHECK 内の使用は従来どおり `42601`。

### D11. 変更ログ・レプリケーション

RECOVER-14・RECOVER-15 の機能は未実装。実装時は評価済みの値を記録し、再生時に再評価しないことを申し送る。

### D12. テスト方針（後続 Issue への要求）

- golden: 時刻値関数を含まない INSERT／UPSERT／DML のハッシュがバイト不変であること。式タグ 13 と `InsertExpr` 系のレイアウト。
- 結合: 同じ文の再送が時刻によらず `23505` で行の値が初回と一致。形の違う文（`now()` と `CURRENT_TIMESTAMP`、精度違い）が `22023`。
  トランザクション内で `now()` 不変。分割実行のクラッシュ再開後も値が同一（`crash_test_partitioned_dml` に now モード追加）。
  台帳 v3 のデコードと長さ不正・未知版の fail-closed。テナント独立。

## 4. セキュリティ・安全性

- テナント境界: 台帳キー `(tenant_id, table, operation_id)` は不変。保存する時刻は照合に使わず、エラーに他テナントの存在情報を含めない。
- fail-closed: 台帳 v3・ジョブ記録 v2 の不正は `XX000`、版 1 ジョブからの時刻必須の再開は `22023`、時刻変換は checked、
  ADD COLUMN の揮発性 DEFAULT は確定まで拒否。ハッシュは評価値で揺れない構文形で行い、誤った `22023`（冪等性破壊）も
  誤った `23505`（未 commit を commit 済みと誤認）も起こさない。
- リソース: 台帳エントリは 8 バイト増のみ。ハッシュ入力は既存上限内。操作種別タグ・式タグでドメインを分離し、
  列ごとの種別バイトと行ごとの列数プレフィクスで連結の曖昧性を排除する。
- 依存: 追加しない。

## 5. 後続 Issue との分担

| Issue | 担当する決定 |
| ----- | ------------ |
| #1586 | D3・D7・D8（式評価器への追加） |
| #1587 | D2・D6・D9・D10（DEFAULT now() と再送一貫性） |
| #2188 | D4・D5（statement／clock_timestamp と台帳記録） |
| TIMESTAMPTZ 担当 Issue | D8 の型切り替え |

## 6. 対象外・申し送り

- コード実装（上表の各 Issue）、TIMESTAMPTZ と TimeZone、変更ログ・レプリケーション、全行書き換えの ADD COLUMN 経路。
- spec 側への確認事項: 親 #1584／#2188 の「評価済み値を台帳に保存して再送時に返す」という表現と、本設計
  （台帳はトランザクション開始時刻のみ、評価値は行に残し、再送は `23505`）の対応づけ。D10 の既存行の値の扱い。
