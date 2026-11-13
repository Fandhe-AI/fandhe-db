# ADR: 拒否固定テストの注記書式と検出規則（Issue #3163）

- ステータス: **Accepted**（設計記録。production コード・スクリプト・CI は無変更）
- 対応: Issue #3163（親 #3162／ルート #3160）
- 後続: #3164（検査スクリプト）・#3165（Makefile・CI への結線）・#3166（既存テストへの後付け）・#3168（close 時の突合）
- 本書は private spec の本文を転記しない。参照は Issue 番号・設計記録のパスに限る

## 1. 背景・目的

- 拒否（`0A000` など）を「現状の挙動」として固定するテストは、追跡 Issue が無くても緑のまま残る
- 意図的な拒否（PG17 と同じ挙動・fail-closed の安全弁）と、未対応で追跡中の拒否を機械的に区別できない
- 検査（#3164）・結線（#3165）・後付け（#3166）・close 時の突合（#3168）が共通に使う注記書式と検出規則を、本 ADR 1 枚で決める

## 2. 決定

### D1 注記の 2 形式

- `tracked: #<番号>[, #<番号>...]`: 未対応の拒否。受け皿になる本リポの open Issue を指す
- `intentional: docs/design/<file>.md[, docs/design/<file>.md...]`: 意図的な拒否。根拠になる設計記録を指す

### D2 文法

- 注記行は `^\s*///?\s*(tracked|intentional):\s+(値)\s*$`。`//` と `///` の両方を認める
- tracked の値は `#[1-9][0-9]*(, #[1-9][0-9]*)*`
  - 本リポの Issue 番号だけを認める。他リポ参照（`owner/repo#N`）・private spec リポの Issue は禁止
  - 区切りはカンマと半角スペース 1 つ
- intentional の値は `docs/design/[A-Za-z0-9._-]+\.md(, docs/design/[A-Za-z0-9._-]+\.md)*`
  - アンカー・節番号は値に含めない。参照する節（例: D11）は別のコメント行に書く
  - 文字集合に `/` 以外のパス区切りや `..` を許さないため、`docs/design/` 配下に限定される
- 補足説明は別のコメント行に書く。注記行には書かない
- 書式不正: 大文字小文字を無視して `tracked` または `intentional` で始まる注記行のうち、上記の文法に合わないもの。例は `Tracked:`・`tracked #3171`・`tracked: 3171`・`tracked: #3171 (説明)`。書式不正は検査失敗にする
- 1 つのテストに tracked と intentional の両方を書いてよい。同じ種類を 2 行に分けることは禁止し、1 行にまとめる

### D3 注記の位置

- `fn` 行の直前にある連続ブロックの中に置く
  - ブロックはコメント行（`//`・`///`）と属性行（`#[test]`・`#[ignore]`・`#[cfg(..)]`・`#[should_panic]`・複数行の属性）だけで構成し、空行を挟まない
  - `#[test]` の上でも下でもよい
- 関数本体の中や、離れた場所のコメントは注記として数えない

### D4 検出の単位

- `#[test]` 属性を持つ `fn` を 1 単位とする
- 対象パスは `crates/engine/tests/**`・`crates/wire-server/tests/**`・`crates/*/src/**`
  - `#[test]` は `#[cfg(test)]` 配下にしか現れないので、src 側は属性で判定する。ファイル単位の cfg 判定は行わない
- 範囲は D3 のブロックの先頭行から本体の閉じ `}` まで
  - 閉じ `}` は「`fn` 行と同じインデントの `}` だけの行」で決める。CI の rustfmt 検査による整形を前提にする
  - `"{sql}"` のような書式文字列があるため、波括弧の数え上げは使わない

### D5 差分の条件

- 比較基準は `git diff <base>...HEAD`（3 点。rename 検出は git 既定のまま）の追加・変更・削除行
- 範囲（D4）に追加・変更行が 1 行でも入ったテストを対象にする。さらに削除行が base 側のテスト範囲（注記行を含む）に触れた場合も、対応する HEAD 側のテストを対象にする。既存テストから注記だけを削除した差分を見逃さないため
- 対応する HEAD 側のテストが見つからない（テスト自体の削除）ときは対象外とする。拒否固定の消失であり、注記の欠落ではないため
- 本体を変えずにファイルを移動しただけなら追加行が出ないので対象外
- 注記の有無と書式は差分ではなく HEAD のファイル内容から読む。注記行自体が未変更でも、本体を変えたテストの注記として有効

### D6 拒否固定の判定（第 1 段の語彙）

テスト範囲の中で、行頭が `//` のコメント行を除いた行に次のどれかが現れたら「拒否固定テスト」とみなす。

- `0A000`（大文字小文字を区別しない。`expect_0a000` のような helper 名も拾う）
- 大文字小文字を区別しない `not[ _]?supported`（`FeatureNotSupported`・`FEATURE_NOT_SUPPORTED`・文言の「not supported」を含む）
- 大文字小文字を区別しない `unsupported`（`Unsupported*` 系の variant を含む）

文字列リテラル内に限定しない（variant の照合も拒否の固定であるため）。認証・プロトコルの `Unsupported*` も対象に含め、`intentional:` で根拠を明示させる。

### D7 語彙の範囲外

- `42601`・`42P01` などは自動検出しない。正当な構文エラーのテストが大量にあり、誤検知を避けるため（親 #3162 の段階導入の第 1 段）
- 未対応を固定する 42601／42P01 のテストにも注記を付けることを推奨する
- D2 の書式検査は、検出の有無にかかわらず差分に現れたすべての注記行にかける（打ち間違いの防止）

### D8 テストの外にある語彙

- テスト関数の外（モジュール直下の対応表・helper 本体・`const` など）の語彙は検出しない
- helper が拒否を検査する場合は、helper 名に `unsupported` か `0a000` を含める命名を推奨する。呼び出し側のテストが D6 で拾われる
- 中立な名前の helper 経由のテストは拾えない（「3. 既知の限界」）

### D9 intentional の根拠にできる設計記録

機械的に検査する条件:

- 値が `docs/design/` 配下の `.md` で、HEAD に存在する（同じ PR で追加したものも可）
- `docs/spec/` と `implementation-status.md` は根拠にしない。spec は private で CI から読めない場合があり、公開物へ持ち込まないため。`implementation-status.md` は Issue 単位の記録であって設計判断ではないため

レビューで確認する条件:

- 根拠の文書が、拒否する形と SQLSTATE を示し、意図的である理由（PG17 と同じ挙動・fail-closed の安全弁・設計上の対象外）を明記している

誤検知の扱い:

- 拒否の消失を確かめるテストや、エラーの写像表を検証するテストなど、拒否を固定していないのに D6 に当たるものは、本 ADR 自身（`intentional: docs/design/rejection-pin-annotation.md`）を指す
- この扱いが許されるのは上記 2 カテゴリに限る。第 3 の注記形式は設けない

### D10 Issue の close 後の扱い

- CI の検査（#3164・#3165）はネットワークも `gh` も使わず、Issue の開閉を見ない。オフラインで決定的に動かす
- close 済み Issue を指す `tracked:` は #3168 の `scripts/check_issue_closeout.sh --check-closed` が、差分に限らずリポ全体から列挙する
- 運用: tracked の Issue を解消する PR が、テストを新しい挙動へ更新して注記を外す（#3166 と同じ方針）。close 後も拒否を意図的に残すなら、根拠の設計記録を用意して `intentional:` へ切り替える

### fail-closed の原則

- 書式不正・根拠ファイルの不在・注記の欠落はすべて失敗にする
- 検査を弱める環境変数や上書き経路は設けない（`simd-codegen-guard.md` と同じ思想）
- #3164 は値を D2 の正規表現で受理してからのみ使い、差分テキストを eval しない

## 3. 注記例

以下は模式例（schematic）であり、実在テストの引用ではない。Issue 番号は本ツリーの受け皿（すべて open）。

### 例 1 末尾 `;`（`42601`。D7 により検出対象外・書式検査のみ）

```rust
// tracked: #3171
#[test]
fn commit_with_trailing_semicolon_is_rejected_for_now() {
    assert_eq!(err.wire_code(), "42601", "COMMIT;");
}
```

### 例 2 search_path の未修飾名（作成先の拒否は D6 に当たる）

```rust
// tracked: #3194
#[test]
fn create_in_tenant_schema_only_path_is_unsupported() {
    assert_eq!(err.wire_code(), "0A000", "{sql}");
}
```

### 例 3 TRIGGER の本番発火（複数 Issue の書き方）

```rust
// tracked: #3183, #3189
#[test]
fn trigger_with_when_clause_is_not_supported_yet() {
    assert!(text.contains("trigger function execution is not supported"));
}
```

### 例 4 FK の `id` 参照（`///` 形式）

```rust
/// tracked: #3177
#[test]
fn fk_to_id_column_is_not_supported_yet() {
    assert!(text.contains("row identity mode is not supported yet"));
}
```

### 例 5 意図的な拒否

実例は `docs/design/nested-loop-join.md` の D11（等価キーの無い FULL JOIN の `0A000`）。

```rust
// intentional: docs/design/nested-loop-join.md
// D11: PG17 と同じ拒否
#[test]
fn full_join_without_equi_key_is_rejected() {
    assert_eq!(err.wire_code(), "0A000");
}
```

### 例 6 書式不正（すべて検査失敗）

```rust
// Tracked: #3171
// tracked: 3171
// tracked: #3171 (説明)
// intentional: docs/spec/anything.md
```

## 4. 既知の限界

- 中立な名前の helper 経由の検出漏れ
- `macro_rules!` で生成するテスト
- 検出が rustfmt 整形済みを前提にしていること
- 第 1 段は 42601／42P01 を検出しない。語彙の拡大や baseline の導入（第 2 段）は本ツリーの範囲外

## 5. 後続 Issue との分担

| Issue | 内容 |
| ----- | ---- |
| #3164 | D2〜D9 のスクリプト実装と単体テスト（出力は英語・終了コード 0／1、使用法の誤りは 2） |
| #3165 | Makefile ターゲットと CI ジョブへの結線 |
| #3166 | 対象機能の既存拒否固定テストへの注記の後付け |
| #3168 | D10 の `--check-closed` 実装（`tracked:` 書式を本 ADR と共有） |
