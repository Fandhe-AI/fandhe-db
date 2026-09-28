# `UPDATE`（単一行・id 指定）の設計判断

Issue #865・対象ビヘイビア: SQL-17（TASK-191。実行結線）。関連ポインタ:
RECOVER-1／2／4／10（`operation_id` 必須化・台帳照合による再送判定）・
TABLE-12（テナント名前空間キー）・RLS-7／9／10／11（RLS 暗黙適用・他テナント
存在情報の非漏えい・read-your-writes）・ERR-1／ERR-2／ERR-4（`wire_code` 契約）。
許可リスト検証・束縛（`sql::allowlist::ValidatedUpdate`・`sql::parser::BoundUpdate`）
は Issue #864 で実装済み。本 Issue はその後段（書き込み経路への結線・
`operation_id` 契約の適用）を扱う。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 構文

```
UPDATE <table> SET <col> = <lit>[, <col> = <lit>]* WHERE id = <n>
USING OPERATION_ID '<id>'
```

単一行・`id` 等価指定形のみを受理する（述語形 `WHERE`・複数テーブル・サブクエリ・
`RETURNING` は許可リスト外）。`id`／`tenant_id`／`visibility` を SET 対象にすることは
許可形状の時点で `42601` として拒否する（疑似列・RLS 内部列であり、クライアントが
行の所有者・可視性を書き換える経路を作らないため）。`EXPLAIN UPDATE ...` は許可形状に
存在しないため、先頭トークンが `EXPLAIN` の場合は既存の `EXPLAIN` 分岐（次トークンが
`SELECT` であることを要求）へ流れて自然に `42601` になる（`INSERT`／`TRUNCATE` と
同じ経路）。

## 判断 A: 列指定の書き込み入口を新設する（第 2 の書き込み経路ではない）

`tenant::update_row_unchecked`（既存の Rust API。全行置換 `RowInput` を要求）へ
直結せず、同一の書き込みプリミティブ（`validate_identifier`・
`require_table_schema_write`・`content_hash`・`ledger::record_in_txn`・
`user_rows_table_def`・`decode_row_for_key`・`encode_row`・
`bump_table_generation_in_txn`・`recovery::commit_boundary::commit`）だけで組み立てた
`tenant::update_row_columns_unchecked`（`pub(crate)`）を新設した。

`BoundUpdate.assignments` は宣言順を保持した**部分更新**の表現（列インデックス・
値のペア列）であり、SET で指定されなかった列は一切現れない。対して
`update_row_unchecked` が要求する `RowInput` は全列を埋めた全行置換形であるため、
そのまま渡せない。

read（対象行の既存 `metadata`／`embedding`）→ merge（SET 対象列だけを上書き）→
encode → write を**単一の write トランザクション内**で行う設計は必須である。
別 read トランザクションで先に読んでから `update_row_unchecked` へ渡す 2 段構成に
すると、同一行への並行 UPDATE（列が互いに素）で read スナップショットと write の
間に他セッションの commit が挟まり lost update が起きる
（`crates/engine/tests/sql_update_single_row.rs::sequential_disjoint_column_updates_both_persist`
が両方の変更が残ることを固定する）。

前提: 対象行の既存 `metadata` は `row_codec::encode_scalar_columns` が書いた正規
レイアウトであることを要求する。SQL 表層の `INSERT`・型付き挿入 API はすべてこの
経路を通るため本番では常に満たされるが、旧フォーマットの raw metadata（全行置換版
`RowInput` を直接構築する非 SQL 呼び出し元専用の Rust API）が書いた行を対象にした
場合は `decode_scalar_columns` が構造不整合を検出し、格納済みデータの破損・
実装不整合として `CatalogError::CorruptSchema`（`sql::exec::map_write_error`
経由で `XX000`）で fail-closed に拒否する（クライアント入力エラー `22000` には
丸めない。codex-review P1 指摘・PR #989）。

`EngineCore::update_row`（全行置換の既存 Rust API）・`tenant::update_row`（同）は
無変更のまま維持する。

## 判断 B: 0 行更新でも台帳記録・世代進行・commit は必ず行う

対象行が「他テナントの行」「未存在 id」「所有者ではあるが RLS 可視集合外
（`PolicyContext::is_visible` が false）」のいずれであっても、行書き込みだけを
スキップし、台帳記録（内容照合ハッシュの計算・記録）・テーブル世代進行・
commit は 1 行更新の場合とまったく同じ手順で行う（`truncate_table_unchecked` と
同じ非対称設計）。

`update_row_unchecked`（全行置換 API）の既存契約は `NotFound` を早期 return
（write トランザクションを commit せず drop）する形だが、列指定の新経路では意図的に
これと異なる契約を採用した。理由: 0 行更新の早期 return を許すと、(a) fsync の
有無でレイテンシが大きく変わり応答時間から対象行の有無が推測できてしまう
（RLS-10）、(b) 台帳が残らないため「0 行更新の再送」が重複コミット（`23505`）として
検知できず、RECOVER-7 系の再送ベース回復確認と整合しない。

`crates/engine/tests/sql_update_single_row.rs::zero_row_update_still_records_the_ledger_entry`
が、0 行更新後の同一 `operation_id` 再送が `23505` になることで台帳記録の非
vacuous 性を固定する。`zero_row_update_is_identical_across_not_found_reasons`・
`zero_row_update_when_owner_but_rls_excludes_visibility` が、対象不在の理由に
関わらず応答（`Ok(rows_affected: 0)`）が同一であることを固定する。

対象行の特定は「テナント名前空間キー（TABLE-12）の物理 lookup ∩ `is_owner` ∩
`is_visible`」の積で判定する（`update_row_unchecked` の `is_owner` 単独判定より
厳格。実装判断: 読み取り経路の RLS 暗黙適用〔RLS-7・RLS-10〕と判定を揃え、可視集合
外の行は 0 行更新扱いにする）。書き込む行の `tenant_id` はサーバー側の `ctx` から、
`visibility` は既存行の値を維持する（クライアントは両者を SET 対象にできない。
許可形状の `42601` と多層防御）。

## 判断 C: 内容照合ハッシュはクライアント入力のみから計算し、専用 `OpTag` を使う

既存 `content_hash::for_update_encoded(id, encoded_row)`（全行置換 API 用）は
マージ後の全行をハッシュするため DB 状態に依存する。列指定 UPDATE は台帳記録を
所有権判定より**前**（`update_row_unchecked` と同じ順序契約。RECOVER-10）に行う
必要があり、この時点ではまだ行を読んでいないため、DB 状態非依存のハッシュ入力が
必須になる。

新設 `content_hash::for_update_columns(id, columns: &[(&str, &Value)])` は、`id` と
SET 句の（列名, 値）ペアを**宣言順のまま**（呼び出し元は並べ替えない）連結する。
列の位置ではなく名前で連結する理由（`ALTER TABLE ADD COLUMN` 耐性）は
`push_named_scalar_columns` と共有する。`VECTOR` 列の SET も `Value::Vector` として
同じ経路に入る。`OpTag::UpdateColumns`（既存 `OpTag::Update` とは別 variant）を
専用に割り当て、ドメイン分離する。

帰結（`crates/engine/tests/sql_update_single_row.rs` で固定）:

- 同一文の再送（0 行更新後を含む）→ `23505`
- SET 値の違い・`id` の違い・SET 句の列宣言順の違い → `22023`
- 同一 `operation_id` を INSERT で使った後に UPDATE で再利用 → `22023`
  （`OpTag::Insert`／`OpTag::UpdateColumns` の分離により実質的な内容一致でも
  必ず内容不一致として拒否される）

## エラー写像: `map_write_error` の操作名パラメータ化

`execute_insert`／`execute_insert_batch`／`execute_truncate` が共有していた
`map_insert_write_error`（`TenantWriteError` → `SqlSurfaceError`）を、呼び出し元の
操作名を追加パラメータとして受け取る `map_write_error(e, op)` へ切り出した。
`wire_code` 自体は不変だが、`CatalogError::Invalid`／`StorageError::Codec` アームの
detail 文言（`"{op} rejected: invalid row"`）に操作名を埋め込む。UPDATE の SET
列値の不正・スキーマ不一致を「insert が拒否された」という誤った文言でクライアントへ
返さないための変更（`client_message()` はこの detail をそのままクライアントへ
含める）。`execute_update` は新設の `op = "update"` 呼び出しとして本経路に乗る
（対象行の既存 metadata デコード失敗は `CorruptSchema` として別経路の catch-all
`XX000` へ分類され、この detail 文言は付与されない）。

あわせて `execute_delete`（Issue #983 で `map_insert_write_error` を暫定使用して
いた既存箇所）も `map_write_error(e, "delete")` へ切り替え、DELETE 失敗時に誤って
「insert が拒否された／失敗した」と返していた既存の不整合を本 Issue で解消した
（UPDATE 追加のついでに DELETE 側の呼び出しも `op` パラメータ化本体へ揃えた形。
`wire_code` 自体は不変）。

`execute_truncate` は本 Issue のスコープ外のため `map_insert_write_error`
（固定文言 `"insert"`）の呼び出しを変更していない。TRUNCATE 失敗時の detail が
引き続き `"insert rejected"`／`"insert failed"` になる既存の不整合は本 PR でも
未解消のまま残る（新規のリグレッションではなく現状維持。是正は別 Issue の担当）。

`map_insert_write_error` は `map_write_error(e, "insert")` の薄いラッパーとして
残し、`execute_insert`／`execute_insert_batch`／`execute_truncate` の既存呼び出しは
無変更のまま維持する。

## 判断 D: SET 値の形状検証は対象行探索より前・列出力の累計サイズは確保前に検証する

コードレビュー（codex-review P1・Cursor Bugbot Medium）で 2 件の指摘があり、
いずれも本判断として対処した。

1点目（無制限確保）: `row_codec::encode_scalar_columns`（`decode_scalar_columns`
   と対になる書き込み側）は列ごとには `MAX_TEXT_FIELD_LEN`（4 MiB）を検査するが、
   複数 `TEXT` 列の合計サイズを確保前に検証していなかった。多数の `TEXT` 列を
   持つスキーマに対する小さな SET 句の UPDATE でも、既存行の再エンコードが
   列数倍（最大で `storage::MAX_METADATA_LEN` を大きく超える規模）まで
   膨らみ得た（security.md「不安全な設計｜無制限リソース確保（DoS）」）。
   `row_codec::MAX_SCALAR_PAYLOAD_LEN`（`storage::MAX_METADATA_LEN` と同値を
   const assert で強制）を新設し、`encode_scalar_columns` が 1 バイト書き込む
   前に累計出力サイズを検証してから `try_reserve_exact` する方式へ変更した。
   INSERT・UPDATE 双方が同じ関数を経由するため、この修正は両経路に効く。

2点目（存在情報の漏えい）: SET 値の妥当性（`VECTOR` 列の次元・`TEXT` 列の長さ
   上限）を対象行 lookup **後**の `Some(row) => { ... }` 分岐内でのみ検証していた
   ため、同一の不正な SET 値でも「対象行が存在する場合は `22000` エラー」
   「対象行が不存在・他テナント所有・RLS 不可視の場合は `UPDATE 0` 成功」という
   応答の分岐が生じ、エラーの有無そのものが行の存在を漏らす識別子になっていた
   （security.md「テナント境界」）。列 index・型の検証と同じループ内（対象行
   探索より前）で次元・長さ上限を検証するよう `update_row_columns_unchecked` を
   変更し、対象の有無に関わらず常に同一の判定（拒否または合格）になる契約へ
   揃えた。

追記（Cursor Bugbot Medium 指摘・PR #989 再指摘）: 上記 2 点目の対策後も、
列ごとの `MAX_TEXT_FIELD_LEN` 検査単独では `encode_scalar_columns` の
フレーミングオーバーヘッド（presence(1)＋長さ(4)）を考慮しないため、
`MAX_TEXT_FIELD_LEN` ちょうどの SET 値が「対象行が存在する場合のみ」
`encode_scalar_columns` 側の `MAX_SCALAR_PAYLOAD_LEN` 超過で `22000` に
なり、不存在・不可視の場合は `UPDATE 0` になるという同型の漏えいが残って
いた。SET 対象の `TEXT` 列だけを対象にした累計フレームサイズ
（`row_codec::scalar_text_entry_len` を `encode_scalar_columns` と共有）を
対象行探索より前のループで検証し、SET 値自身だけで決定的に判定できる
超過は同一の拒否へ揃えた。

追記（codex-review P0 再指摘・PR #989）: 上記 2 点の対策後も、個々の SET 値は
上限内でも、対象行に既に格納されている**未変更の** `TEXT` 列（探索前は内容
不明）と組み合わさって初めて `MAX_SCALAR_PAYLOAD_LEN` を超えるケースは対象行
探索後の `encode_scalar_columns`（現 `merge_encode_scalar_columns`）呼び出し
でのみ判明するため対象行探索より前のループでは判定できない、という限界が
残っていた。旧実装はこの判定を「`is_owner && is_visible` を満たす行だけ」を
マージ対象にしていたため、可視な大きな既存行への SET は `22000`、同じ SET を
不可視な（`Private`・呼び出し元 `ctx` が `Public` のみ許可）既存行へ送ると
`UPDATE 0` 成功という応答差になり、可視性の狭いセッションが「不可視な行の
中身がどれくらい大きいか」を推測できる経路になっていた。

この残差を「探索前に対象行の有無へ関わらず静的に決定できる契約」へ変更する
ことは、`MAX_TEXT_FIELD_LEN` と `MAX_SCALAR_PAYLOAD_LEN` が同値である現行の
列長上限設計では、2 列以上の `TEXT` 列を持つ任意のスキーマで事実上すべての
部分更新を拒否する退化した契約になってしまう（`TEXT` 列 1 本だけで単体が
上限一杯まで埋まり得るため）ため採用しなかった。列単位の縮小上限
（`MAX_SCALAR_PAYLOAD_LEN / n_text_columns` 等）は INSERT 側の許容値にも
影響する契約変更でありオーナー判断が要るため、引き続き対象外とする。

一時、`update_row_columns_unchecked` を「物理行が存在する（TABLE-12 の
名前空間キーで取得できる）ことのみを条件に、RLS 可視性を問わずマージ・
再エンコードを必ず実行し、実際に書き込むかどうかだけを `is_owner && is_visible`
で決める」設計へ変更したが、これは「可視・不可視の応答差」を閉じる代わりに
「不可視な既存行」と「不存在な id」の応答差（`22000`／`XX000` と `UPDATE 0`）
という**別種の漏えい**を生んでいた（codex-review P0 再指摘・PR #989 再々指摘・
`crates/engine/src/tenant.rs:1750`）。可視性の狭いセッションが、対象 id が
「不存在」なのか「不可視だが存在し、かつ内容依存で大きい／壊れている」のかを
エラー種別から区別でき、不可視行の存在・内容状態を推測できてしまうため、
security.md「テナント境界（RLS 相当）の弱体化」に抵触する。

**判断 D 再改訂**: 内容依存の処理（`scan_scalar_columns`・
`merge_encode_scalar_columns`。超過判定・デコード失敗を含む）は
`is_owner && is_visible`（実際に書き込む対象）を満たす行に対してのみ実行する
設計へ戻した（`update_row_unchecked`・`delete_row_unchecked` と同じ「所有・
可視でない対象は探索直後に打ち切り、内容には一切触れない」設計に揃える）。
これにより「可視な大きな既存行は `22000`、同じ SET 値を不可視な既存行へ
送っても不存在と同じ `UPDATE 0`」となり、可視・不可視の応答差は解消される
一方、「不可視な既存行」と「不存在な id」はいずれも `UPDATE 0`・内容無参照で
完全に同一になる（`tenant::tests::
update_row_columns_overflow_from_unchanged_column_is_rejected_only_when_visible_and_invisible_matches_not_found`
で固定）。TABLE-12 の名前空間キー（`key = (ctx.tenant_id(), id)`）により
他テナントの行はこのキーで物理的に取得できないため、「他テナント所有 id」が
この経路で混入することは構造的に起こらない。残る唯一の観測差は「対象 id が
`is_owner && is_visible`（実際に書き込む対象）を満たす既存行かどうか」であり、
「不存在」「他テナント所有」「所有だが RLS 不可視」はいずれも `UPDATE 0`・
内容無参照で区別なく扱われる。これは全行置換 API（`update_row_unchecked`・
`delete_row_unchecked`）が `is_owner` 単独判定で既に持っている「対象の有無で
応答が分岐する」性質と同型の、部分マージを伴う書き込み API 一般に内在する
限界であって RLS 可視性やテナント境界の越境ではない（可視性の異なる 2
セッションが同一 id へ同一 SET 値を送っても、対象行が RLS 不可視である限り
双方とも `UPDATE 0`・内容無参照で同一の観測を得る）。

**追記（codex-review P0 再々々指摘・PR #989・`crates/engine/src/tenant.rs:1696`）**:
判断 D 再改訂は「内容依存の処理を `is_owner && is_visible` を満たす行のみへ
限定する」ところまでは正しく反映していたが、その判定に使う対象行そのものを
`decode_row_for_key`（embedding・metadata を含むフル本体デコード）で取得して
おり、この呼び出しが `is_visible` 判定より**前**に無条件で実行されていた。
そのため、不可視な既存行の embedding・metadata が破損している場合、`is_owner
&& is_visible` の判定に到達する前に `decode_row_for_key` 自体が `Err`
（`XX000`）を返してしまい、「不存在（`UPDATE 0`）」と区別できてしまっていた
（新設した「可視・不可視の応答差」ではなく、判断 D 再改訂で閉じたはずの
「不可視な既存行」と「不存在な id」の応答差が、超過判定ではなくデコード
失敗という別経路から再び開いていた）。対応として、`storage::
decode_row_tenant_and_visibility`（tenant_id・visibility の固定長フィールド
のみを読むヘッダ専用デコード。embedding dim・metadata 長に依存しないため
可視性判定そのものが内容依存にならない）で `is_owner && is_visible` を先に
確定し、それを満たす行だけをフル本体デコード（`decode_row_for_key`）の対象に
する形へ変更した。ヘッダのデコード自体が失敗した場合も「不可視」と同一に
扱い（本体には一切触れない）、可視と確定した行のみが本体デコード失敗時に
`XX000` を返す（この場合は対象が「存在し、かつ可視」と確定済みのため、
エラー有無で存在情報は漏れない）。`tenant::tests::
update_row_columns_corrupt_invisible_row_body_is_indistinguishable_from_not_found`
で、末尾切り詰めにより本体デコードのみが失敗する不可視行に対する更新が
`UPDATE 0` になることを固定する。

あわせて、部分 UPDATE の実装（codex-review P1 指摘・PR #989 再指摘）は
`decode_scalar_columns`（対象行の全 `TEXT` 列を `Value::Text` へ複製）ではなく
借用版 `scan_scalar_columns` と、それを土台に SET 対象列だけを差し替えて
直接エンコードする新設 `row_codec::merge_encode_scalar_columns` へ置き換えた。
SET 対象でない列は借用 `&str` のまま `buf` へ書き込まれるため、複製される
のは SET 句の値（クライアント入力）のみに抑えられ、「decode バッファ＋encode
バッファ」の 2 重のピーク確保（`storage::MAX_METADATA_LEN` 上限により実際は
両者とも 4 MiB 以下に収まるが、部分 UPDATE 1 回あたりのピークをさらに縮小
する）を避ける。

## wire 応答: `CommandComplete` タグ

`UpdateOutcome { rows_affected: u64 }`（0 または 1）を pg 互換の `CommandComplete`
タグ `UPDATE <n>` へ整形する。`INSERT <oid> <rows>` と異なり OID フィールドを
持たない（PostgreSQL の `UPDATE` タグ規範に準拠）。

## `SqlOutcome::Update` の追加（BREAKING CHANGE）

`sql::SqlOutcome` は `#[non_exhaustive]` でないため、`Update` variant の追加は
破壊的変更として扱う（`Truncate` 追加時〔TASK-193〕と同じ扱い）。網羅 match の
更新箇所は `core.rs::execute_sql`（`Select`／`Aggregate`／`Scan` の 3 アーム）・
`wire-server::simple_query`。

## スコープ外（本 Issue で対処しないもの）

- NoSQL 表層 `update` op（`op: update` は引き続き `0A000`）
- 述語つき `UPDATE ... WHERE`（複数行の条件付き更新）・複数行内容照合の設計
- `RETURNING`・UPSERT
- `DELETE`（単一行）の実行結線（`update_row_columns_unchecked` の「0 行同一化」
  パターンを再利用できる設計）
- `fault_injection.rs::is_committed_insert` の UPDATE 対応
- `EngineCore::update_row`（全行置換 Rust API）の意味論変更
- 0 行／1 行経路のレイテンシ分布実測: 台帳記録・世代進行・commit（fsync）まで
  1 行経路と完全に同一の手順を踏む「構造上の同一性」（判断 B）により、`wire`
  応答からのタイミング差は生じない設計だが、実測による定量的確認は行っていない
  （`docs/design/wire-tenant-row-id-scope.md`〔Issue #738〕のハーネスを UPDATE
  向けに拡張すれば計測可能）
