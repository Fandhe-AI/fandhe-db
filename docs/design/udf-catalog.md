# UDF カタログのスキーマと保存形式（Issue #1919）

親 Issue #1918（UDF の永続化）の土台。ビヘイビア ID は SQL-66（関連: SQL-9・SQL-67 (3)・
TABLE-27・RLS-12・TASK-309）のポインタ表記のみで参照し、spec 本文は転記しない。

## 範囲

- 本 Issue: redb のテーブル定義、定義 blob の版付き保存形式、crate 内部の保存・読み出し API
  （`crates/engine/src/catalog/function.rs`）。呼び出し元は当面テストのみ。
- 範囲外: `CREATE FUNCTION`／`DROP FUNCTION` からの書き込み（#1920）、セッション開始時のロード
  （#1921）、件数上限の設定・DDL 権限（#1922）、依存追跡、`OR REPLACE` の意味論。

## テーブル

| 表 | キー | 値 |
| -- | ---- | -- |
| `functions` | `(scope, function_key, arg_signature)` | 定義 blob |
| `function_id_counter` | 固定キー `next_id` | `u64`（1 始まり・再利用しない） |

どちらも最初の書き込みまで作らない（遅延作成）。読み取りは表の不在を 0 件と扱うため、UDF を
使わない DB や本変更前の DB のファイル構成・既存カタログの値は変わらない。

## キー設計

- `scope`: 可視範囲の所有者。本 Issue では検証済みの `tenant_id` のみ受理する。キーの先頭に置くため、
  他テナントの定義は構造上引けない（RLS-12 の境界）。
- `function_key`: relation キー形（`public` は裸の名前、SCHEMA 配下は `#<schema_id>.<name>`）を
  ASCII 小文字へ正規化したもの。
- `arg_signature`: 多重定義を別キーにするための引数型の並び。各引数を typmod・配列の最大長と次元を
  落とした型トークンへ写して `;` で連結する（型注釈なしは `?`、引数 0 個は空文字列）。decode 時に値から
  再計算して一致を検査する。

## 定義 blob v1

リトルエンディアン。文字列は `u32` 長 + UTF-8。

```text
u8 version(=1) / u64 id / u8 owner(予約=0) / u8 language / u8 volatility / u8 flags
/ u8 parallel / cost(u8 有無 [+ f64]) / rows(u8 有無 [+ f64])
/ u32 引数数 { name(u8 有無 [+ str]) / type(u8 有無 [+ str tag + str param]) }
/ return type(同上) / str 本体の原文
```

- 版の運用: 未知の版は拒否する（`CorruptSchema`）。形式を変える場合は新しい版を足し、v1 の decode を
  残す。ゴールデンバイト列のテストで v1 を固定している。
- 型は `ColumnType::catalog_fields` の `(tag, param)` 1 対で持つ。ENUM は本 Issue では encode・decode
  とも拒否する。受理に広げても `(tag, param)` のバイト表現は変わらないため、版を上げずに解禁できる。
- 上限: 引数 100（格納形式上の上限）・本体 64 KiB・スコープあたり 64 件（`ViewLimitExceeded`）。長さ・件数は
  確保の前に検査する。

## fail-closed

未知の言語／揮発性／parallel タグ、未定義の flag ビット、owner 予約領域の非零、id=0、切断、末尾の余り、
上限超過、不正 UTF-8・識別子・型、キーと値のシグネチャ不一致はすべて固定文言の `CorruptSchema`。
blob の断片・tenant_id・本体の原文はエラー文言へ出さない。

## 後続への申し送り

- 共有 `public` の扱い（#1920）: 本 Issue の実装はテナント単位である。共有の扱いは後続 Issue #1920 で扱う。
- ENUM 型の引数・戻り値の解禁（#1920。形式は不変）。
- `sql::udf_call::MAX_UDF_PARAMS`（32）と格納上限 100 の整合（#1920／#1922）。
- 他セッションへの伝播の世代カウンタ（#1921）。独立テーブルとして後から追加でき、blob 形式は変わらない。
- 依存追跡（`2BP01`）・多重定義の解決規則・`OR REPLACE`（#1920 以降）。
