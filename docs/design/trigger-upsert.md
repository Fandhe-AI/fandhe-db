# UPSERT（INSERT ... ON CONFLICT）でのトリガ発火（Issue #3191）

TABLE-45・実行モデル D4／D7／D8／D10（[trigger-execution-model.md](trigger-execution-model.md)）の実装記録。
spec 本文は転記せず、ポインタ表記のみ。

## 範囲

SQL 表層の `INSERT ... ON CONFLICT`（`DO NOTHING`・`DO UPDATE`。`INSERT ... SELECT ... ON CONFLICT` の 0 行
経路を含む）が、書き込みテナント自身の INSERT・UPDATE トリガを発火する。実体は
`crates/engine/src/tenant/trigger_upsert.rs`（行ループ）と `tenant.rs::upsert_typed_rows_impl`（分類・台帳・
BEFORE STATEMENT）。トリガの無い表は従来の行ループのままでバイト不変。

## 発火順序（PostgreSQL 17 と同じ）

| 分岐 | 発火 |
| ---- | ---- |
| 競合なし | BEFORE ROW INSERT → 挿入 → AFTER ROW INSERT |
| 競合あり・DO UPDATE | BEFORE ROW INSERT（`EXCLUDED` は書き換え後の NEW）→ 競合判定 → `WHERE` → BEFORE ROW UPDATE（OLD = 既存行、NEW = SET 後）→ 更新 → AFTER ROW UPDATE（AFTER ROW INSERT は呼ばない） |
| 競合あり・DO NOTHING | BEFORE ROW INSERT のみ（AFTER も UPDATE トリガも呼ばない） |
| BEFORE の `NULL` 返却 | その行の挿入（または更新）をやめる。件数・AFTER に数えない |
| 文トリガ | BEFORE INSERT → BEFORE UPDATE（DO UPDATE のみ）→ 行処理 → AFTER UPDATE（DO UPDATE のみ）→ AFTER INSERT。対象 0 行でも 1 回ずつ |

- 台帳（`operation_id`）の照合は全トリガ呼び出しより前。再送ではトリガを 1 回も呼ばない（D10）
- 衝突判定の鍵は BEFORE INSERT 適用後の NEW。可視性は従来どおり「所有 ∩ 可視」で、不可視な自テナント行への
  `DO UPDATE` は文全体を拒否する。利用者定義ポリシーは BEFORE 適用後の最終行に INSERT 側 WITH CHECK、既存行に
  USING、更新後の行に更新側 WITH CHECK を従来と同順で適用する
- SET の適用は従来経路と共有の `apply_upsert_assignments`（第 2 の SET 評価を作らない）
- 行ごとに表ハンドルを開いて閉じる（D5）。対象省略（`DO NOTHING` 専用）は行ごとの制約検査で永続一意索引を
  同期しながら進め、それ以外は全行の後に 1 回検査する。UPDATE 側の BEFORE 行トリガがある表では、参照先側の
  FK 検査を全列更新済みとして行う（fail-closed）

## PostgreSQL との差分（fail-closed 側）

- `RETURNING` 付き UPSERT でトリガのある表は `0A000`（追跡: #3192）
- 省略した NOT NULL 列を BEFORE INSERT が補うことは UPSERT では不可（従来どおり台帳より前に `23502`）
- 同じ文の中で先に書いた行へ `DO UPDATE` が再び当たる場合は `22000`（PG は `21000`）。BEFORE による鍵の
  書き換えで同じ文の先行行と一意制約が衝突する場合は `23505` で文全体を取り消す
- 文の開始時点（BEFORE STATEMENT の後）の UNIQUE 走査結果で衝突を判定するため、BEFORE ROW の本体が同じ表へ
  書いた行は衝突先に入らない（外側の挿入が衝突すれば `23505`）
- UPDATE 側の BEFORE 行トリガがある表では、トリガ本体による同表の `UPDATE` を行ループの間 `0A000` で拒否する
  （D6 の暫定ガードと同じ）
- トリガ本体内の `INSERT ... ON CONFLICT` は窓口の許可リスト外で従来どおり拒否
- `DO NOTHING` は UPDATE を行わないため、UPDATE トリガだけの表でも従来の `0A000` にならず成功する

## 対象外

- NoSQL（HTTP）表層の UPSERT の発火: #3396
- `RETURNING` 付き UPSERT の最終値の投影: #3192
- `COPY FROM`: #3388、分割実行 DML: #3398
