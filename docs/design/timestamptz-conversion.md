# ADR: AT TIME ZONE と TIMESTAMP／TIMESTAMPTZ／DATE の変換

- ステータス: Accepted（Issue #1576 の実装に伴う設計記録）
- 対応: Issue #1576（親 #1572。前提は #1574・#1575、wire の OID 1184 は #1577）
- 関連ビヘイビア: TABLE-33（ポインタ: `docs/spec/04-behavior/`。本文は転記しない。[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連 doc: [session-timezone.md](session-timezone.md)（セッション TZ）、[datetime-column.md](datetime-column.md)（TIMESTAMPTZ の codec・リテラル）、[timestamptz-tz-data-adoption.md](timestamptz-tz-data-adoption.md)（TZ データ源）
- 依存の追加・feature の変更はない（承認済みの `jiff =0.2.38`・`jiff-tzdb =0.1.9` の範囲）

## 1. 目的

`AT TIME ZONE`（両方向）・`AT LOCAL`・`timezone()` を実装し、TIMESTAMP／TIMESTAMPTZ／DATE の暗黙の変換と代入時の変換をセッション TZ で行う。TIMESTAMPTZ 列を述語・ORDER BY・GROUP BY・集計・window・集合演算・サブクエリ・二次索引・EXTRACT／date_trunc で使えるようにする。

## 2. 設計判断

| ID | 判断 |
| -- | ---- |
| D1 | `x AT TIME ZONE z` は構文段で `timezone(z, x)`、`x AT LOCAL` は `timezone(x)` の関数呼び出しへ脱糖する（PG も同じ関数の構文糖）。後置演算子の結合は `COLLATE` より弱く `* /` より強く、左結合。zone は一次式（と後置 `COLLATE`）まで。`timezone` は組み込み関数名として予約され、同名の UDF は作れない（`42723`。破壊的変更） |
| D2 | 型の解決は PG に合わせる: timestamp → timestamptz、timestamptz → timestamp、date は局所の 0 時の timestamptz を経由して timestamp、型なし文字列は timestamptz。TIME は timetz が未実装のため `0A000`。INTERVAL の zone は構文段で拒否する |
| D3 | zone 引数の解決器は `SessionTimeZone::resolve_zone_argument`（PG の `DecodeTimezoneName` の順序）。`SET TIME ZONE` 用の `parse_setting` とは別で、数値形（`'+9'`・`'+09:00'`）は **POSIX 解釈で符号が反転する**（`'+9'` は UTC-9）。IANA 名は大文字小文字を区別しない。長さ（255 バイト）→ ASCII → 許可文字の順に検査し、失敗はすべて固定文言の `22023`（入力値を反射しない） |
| D4 | 略称は UTC 同義語（`UTC`・`GMT`・`UT`・`Z`・`ZULU`）だけ受理する。PG は略称表を IANA 名より先に引くため、略称と tzdb 名が衝突し意味が食い違う名前（`CET`・`EET`・`MET`・`WET`・`EST`・`MST`・`HST`。PG の略称表と bundled tzdb の積集合を実測して確定）は fail-closed で `22023` にする。それ以外の PG 略称（`JST`・`EDT` など）も `22023`（意図的な差） |
| D5 | セッション TZ は評価のたびに `with_current_session_time_zone` で参照し、`BoundExpr` やプランへ焼き込まない（PG の STABLE 関数と同じ）。変換は `BuiltinFn`（`TimestampToTstz`・`TstzToTimestamp`・`DateToTstz`・`TstzToDate`・`AtZoneToTstz`・`AtZoneToTimestamp`・`AtLocalToTstz`・`DatePartTz`・`DateTruncTz`・`DateTruncTzZone`）で表し、zone は通常の TEXT 引数。定数の zone は束縛時に検証して不正なら `22023` |
| D6 | TZ の無い `TIMESTAMPTZ '…'` リテラルと、TIMESTAMPTZ と比較される型なし文字列は、PG と同じく**束縛時のセッション TZ** で UTC へ確定する。構文段は TZ を知らないため AST（`Expr::TimestampTzLiteral { micros, session_local }`）はゾーン指定済みの値（UTC）か壁時計のどちらかを保持する。CHECK への再描画は UTC の明示 offset（`+00`）で行う |
| D7 | 比較・ORDER BY・GROUP BY・集計・集合演算・二次索引は UTC の i64（時点）で行い、表示 TZ に依らない。TIMESTAMPTZ 列と文字列・TIMESTAMPTZ リテラルの比較は型付き述語（`TypedLiteral::TimestampTz`）で、二次索引（`OrderedColumnIndex::I64`）を使える。TIMESTAMP／DATE と TIMESTAMPTZ の混在（列どうし・TIMESTAMP 列 vs TIMESTAMPTZ リテラル）は行ごとに変換して比べ、索引は使わず全走査へ縮退する（gap では timestamp → timestamptz の変換が単調でないため。fail-closed） |
| D8 | 代入時の変換は `UPDATE SET` の式で行う（`set_expr::convert_for_assignment`）。TIMESTAMP・DATE → TIMESTAMPTZ 列はセッション TZ の壁時計として、TIMESTAMPTZ → TIMESTAMP・DATE 列は局所の値として入る。INSERT の定数式は TIMESTAMPTZ を UTC の明示 offset で渡し、列側の束縛がセッション TZ に影響されない |
| D9 | セッション TZ に依存する式（`AT LOCAL`・混在の比較・TIMESTAMPTZ への `date_part`／`date_trunc`・ゾーン無しの文字列）は CHECK 制約の本体に置けず `0A000`。定義時と別の TZ で結果が変わる述語を作らせない |
| D10 | 分割実行 UPDATE の内容照合ハッシュは、ゾーン無しの TIMESTAMPTZ リテラルを含むとき解釈に使ったセッション TZ 名を含める（別の TZ での再送は `22023`）。新しい型バイト 3（ゾーン指定済み）・4（ゾーン無し）はタグ 10 の既存の型バイト（0〜2）とそれらのハッシュ値を変えない |

## 3. DST の確定（PG 17 の実測）

セッション TZ または zone 引数が America/New_York のときの値。実装（`SessionTimeZone::local_to_utc_micros`）は gap を遷移前の offset、fold を遷移後の offset で解釈する（jiff の `AmbiguousOffset::Gap { before }`・`Fold { after }`）。PG の規則「2 つの解釈のうち UTC で遅い方」と同じで、#1574 の暫定実装を確定扱いにした。

| 入力 | 結果 | 規則 |
| ---- | ---- | ---- |
| `TIMESTAMP '2026-03-08 02:30' AT TIME ZONE 'America/New_York'`（gap） | `2026-03-08 07:30Z` | 遷移前の offset（-05） |
| `TIMESTAMP '2026-11-01 01:30' AT TIME ZONE 'America/New_York'`（fold） | `2026-11-01 06:30Z` | 遷移後の offset（-05） |
| `TIMESTAMP '2026-04-05 02:30' AT TIME ZONE 'Australia/Sydney'`（fold） | `2026-04-04 16:30Z` | 遷移後の offset（+10） |
| `TIMESTAMP '2026-10-04 02:30' AT TIME ZONE 'Australia/Sydney'`（gap） | `2026-10-03 16:30Z` | 遷移前の offset（+10） |
| `date_trunc('day', TIMESTAMPTZ '2026-03-08 16:00Z')` | `2026-03-08 05:00Z` | day 以上は切り捨て後の局所時刻を gap／fold 規則で再解決 |
| `date_trunc('hour', TIMESTAMPTZ '2026-11-01 05:30Z')` | `2026-11-01 05:00Z` | hour 以下は元の offset を保つ |
| `date_trunc('hour', TIMESTAMPTZ '2026-11-01 06:30Z')` | `2026-11-01 06:00Z` | 同上（fold の 2 回目） |

## 4. PG 17 との照合表（セッション TZ = Asia/Tokyo）

| 式 | 結果 |
| -- | ---- |
| `EXTRACT(hour／epoch／timezone／timezone_hour FROM TIMESTAMPTZ '2026-01-01 00:00Z')` | `9`／`1767225600`（UTC の絶対値）／`32400`／`9` |
| `date_trunc('day', TIMESTAMPTZ '2026-01-01 00:00Z')` | `2025-12-31 15:00Z`（局所の 2026-01-01 00:00 +09） |
| `date_trunc('day', …, 'America/New_York')` | `2025-12-31 05:00Z`。不正な zone は `22023` |
| `TIMESTAMP '2026-01-01 09:00' = TIMESTAMPTZ '2026-01-01 00:00Z'` | 真（timestamp 側をセッション TZ で timestamptz へ変換） |
| `DATE '2026-01-01' = TIMESTAMPTZ '2025-12-31 15:00Z'` | 真（date は局所の 0 時） |
| `TIMESTAMPTZ '9999-12-31 23:59:59Z' AT TIME ZONE 'America/Los_Angeles'` | `9999-12-31 15:59:59` |

## 5. 意図的な差・対象外

- PG の時刻略称表（`JST`・`EDT`・`MSK` など）は受理しない（D4）。`'1.5'` のような PG が受理する奇妙な数値形も `22023`。
- `TIMESTAMP '0001-01-01' AT TIME ZONE 'Asia/Tokyo'` は PG が BC の LMT 値を返すが、本実装は TIMESTAMPTZ の範囲（UTC で 0001〜9999 年）の外なので `22008`。
- 秒を省いた `TIMESTAMP '2026-01-01 00:00'`（PG は受理）は #1574 以前からの既存制約で本 Issue の対象外。
- `date_trunc(unit, DATE)` の戻り型は PG では timestamptz だが、本実装は従来どおり TIMESTAMP（対象外）。
- `COALESCE`／`CASE`／`UNION` で TIMESTAMP・DATE と TIMESTAMPTZ を混在させた場合の型の単一化（PG は timestamptz）は型不一致の拒否を維持する。
- TIMESTAMPTZ の UNIQUE／PRIMARY KEY／FK・JOIN キー・SUM／AVG・算術・明示 CAST・json 変換・`TIMESTAMPTZ(p)`・配列・特殊値は閉じたまま。
- 構文段の限界: 投影項目の先頭が型付きリテラルの式（`SELECT TIMESTAMP '…' AT TIME ZONE …` を FROM 付きで書く形）、式を本体に持つビューは従来から未対応。
- TIMESTAMPTZ 列を持たないテーブルの分割実行 UPDATE／DELETE で、セッション TZ に依存する式（`AT LOCAL`・混在の比較など。ゾーン無しのリテラルを除く）を含む再送の TZ 照合は未対応。
- `now()` 系の戻り値型の切り替え・EXTRACT の戻り型（PG は numeric）・サーバー既定 TZ の CLI フラグは対象外。
