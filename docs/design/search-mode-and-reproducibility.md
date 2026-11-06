# 検索モード（MODE）・LLM クエリ展開・再現性の利用者向け説明

- ステータス: Accepted（現行コードの挙動の説明文書。新しい仕様判断は含まない）
- 対応: Issue #2146（親 #2145）
- 関連ビヘイビア: PLAN-11・SEARCH-9・SQL-12（ポインタ: `docs/spec/04-behavior/`）
- 関連コード: `crates/engine/src/sql/mode.rs`・`crates/engine/src/precision.rs`・
  `crates/engine/src/query_planner.rs`・`crates/engine/src/sql/exec.rs`・
  `crates/engine/src/sql/using_plan.rs`
- 関連 doc: 末尾「関連文書」参照

## 位置づけと公開範囲

- 本文書は、コードと既存の公開 docs に書かれた事実を、利用者向けに 1 か所へまとめたもの。
  規範（何が正しい挙動か）は private spec のビヘイビア ID 側にあり、本文書は規範ではない。
- 根拠は関数名・定数名・既存 docs へのポインタで示す。行番号は変わるため使わない。
- 裏付けが取れない点は推測で埋めず「要確認」と書く（一覧は「要確認・未確定事項」節）。
- 閾値などの数値はコード上の現在値であり、仮置きのものはその旨を併記する。

## モードの指定手段と優先順位

モードは `recall`（広域・既定）と `precision`（ピンポイント抽出）の 2 値。

| 表層 | 指定手段 | 備考 |
| ---- | -------- | ---- |
| SQL | 文ごとの `USING MODE '<値>'` 句 | 最優先 |
| SQL | セッション変数 `SET search_mode = '<値>'` | 接続（`SessionState`）ごとに保持され、接続どうしで混ざらない |
| SQL | `USING PLAN` のときの LLM 推定（プランナー推定） | 下の 2 つが無いときだけ採用される |
| NoSQL（HTTP） | `search` 要求の `mode` キー | 要求ごとの指定のみ。`search_mode` 相当のセッション変数は無く、未知キーとして拒否される |
| 既定 | 上記のいずれも無い | `recall` |

- 優先順位は `sql::mode::resolve_mode_with_planner` が決める。
  クエリ句 → セッション変数 → プランナー推定 → 既定の順。
- 値は小文字の `recall`／`precision` との完全一致だけを受理する
  （`SearchMode::parse_literal`）。大文字や空白入りは `22000`。
- 決まったモードとその出所は EXPLAIN の `mode`・`mode_source` 行で確認できる。
  `mode_source` の値は `query_clause`・`session_variable`・`planner_estimate`・`default`
  （`ModeSource::as_str`）。

## MODE が切り替える内部動作

| 項目 | `recall` | `precision` | 根拠 |
| ---- | -------- | ----------- | ---- |
| 取得件数 | `LIMIT` どおり | 確信度の判定に Top-2 が要るため広げる（`LIMIT 1` でも Top-2 を取得） | `sql/exec.rs` の `k_eff` 導出 |
| ANN（HNSW） | opt-in で使える | 使わない。厳密な brute-force のみ | `sql/exec.rs` の `hnsw_full_visible_eligible`・`hnsw_subset_eligible` と hybrid 側の除外 |
| 確信度ゲート | 無し | あり（`precision::apply_gate`） | `precision.rs` |
| ゲートの位置 | — | DISTANCE 段と事後フィルタの後、RLS 安全網（`RlsSafetyNet::apply`）の前 | `sql/exec.rs` |
| 確信度の指標 | — | dense は cosine 類似度、hybrid は正規化した RRF スコア | `precision::rrf_normalized`・`PrecisionPolicy::dense/hybrid` |
| 返却件数の上限 | `LIMIT` | 上限 `max_results`（既定 1・絶対上限 `MAX_PRECISION_RESULTS` = 100） | `precision.rs` |
| 閾値未達のとき | — | 0 行の通常応答（エラーではない）。確信度が非有限なら `XX000` | `precision::apply_gate` |
| 完全性を保証できないとき | — | DISTANCE 先行・SCALAR 事後フィルタで可視件数が `MAX_SEARCH_K` を超える場合、DISTANCE 検索を実行せず空集合 | `precision_completeness_unbounded` |
| hybrid の `window` との併用 | 可 | `0A000`（[hybrid-tuning-parameters.md](hybrid-tuning-parameters.md)） | — |
| 順位の並べ替え | しない | しない（ゲートは通過・不通過の判定のみ） | `precision::apply_gate` |
| 閾値の変更経路 | — | SQL・セッションからは変えられない。サーバー側設定（`EngineCore` が保持） | `precision.rs` |

precision の既定閾値（コード上の現在値）。**いずれも仮置きで、TASK-163 で実測により確定する予定。
目標値はオーナー判断待ち（要確認）**。

| 方式 | Top-1 の最小値 | Top-1 と Top-2 の最小差 |
| ---- | -------------- | ----------------------- |
| dense | 0.80 | 0.05 |
| hybrid | 0.98 | 0.005 |

モードで変わらないもの: RLS によるテナント境界（モードに関係なく安全網が最後に適用される）、
同点の順序（id 昇順）、hybrid の融合方式（RRF）。
`USING PLAN` と `precision` の組み合わせで 0 行になる事象の切り分けは
[using-plan-precision-empty-result.md](using-plan-precision-empty-result.md) を参照。

## LLM クエリ展開（`USING PLAN`／NoSQL の `plan`）の失敗時挙動

| 状況 | 結果 | 根拠 |
| ---- | ---- | ---- |
| プランナー未注入・埋め込み未注入 | `XX000` で拒否 | `sql/using_plan.rs` のモジュールドキュメント |
| 接続不能・タイムアウト | `XX000` で拒否 | `PlanError::Unavailable`・`PlanError::Timeout` |
| LLM 応答の型不一致・キー欠落・上限超過 | 応答全体を拒否（`XX000`） | `query_planner::parse_expansion` の厳格な検証 |
| 接続先がループバック以外 | 設定時に拒否 | `with_host`・`PlanError::UnsafeTarget` |
| 応答の `mode` フィールドだけが不正 | その項目だけ `None` に丸め、明示指定か既定（`recall`）に落ちる（PLAN-11 ポインタ） | `parse_optional_mode` |

- 展開に失敗したとき、展開しないまま検索へ黙って縮退する経路は無い。
  失敗は SQL エラー（`XX000`）で返り、接続は継続する（接続継続の詳細は要確認）。
- LLM の出力は検証済みの検索語とソフトヒントに閉じ込める。SQL 文字列やプラン文字列へは連結せず、
  ハードフィルタにもしない。
- 送信先はループバックに限る（既定 `127.0.0.1:11434`。接続直前の名前解決結果にも確認を行う）。
- 送信内容: 当該テナントの可視範囲から作る辞書文脈（`(table, ctx)` 単位の
  `dictionary_snapshot`）と、制御文字を除去して切り詰めた質問文。他テナントの情報は含まない。
  テナントをまたぐプロンプト接頭辞のキャッシュは持たない。
- エラーにプロンプト本文・LLM 応答本文を含めない。
- 上限・タイムアウト: 質問文 `MAX_QUESTION_CHARS` = 2000 文字、検索語 `MAX_SEARCH_TERMS` = 32 個、
  1 語 `MAX_TERM_LEN` = 128、ヒント `MAX_HINT_LEN` = 256、プロンプト `MAX_PROMPT_BYTES` = 256 KiB、
  応答 `MAX_RESPONSE_BYTES` = 1 MiB。接続 5 秒・読み書き 30 秒が既定。
- 高コストな I/O（LLM 呼び出し・埋め込み）の前に構造検証を前倒しする（`sql/using_plan.rs` の束縛前検証）。

## 再現性・決定性の範囲

| 区分 | 項目 | 根拠・備考 |
| ---- | ---- | ---------- |
| 決定的 | 厳密検索（brute-force）と RRF の順序。距離順・RRF・`search` の同点は id 昇順 | README「検索結果順序」・[rrf-tie-break-determinism.md](rrf-tie-break-determinism.md) |
| 決定的 | precision ゲート（並べ替えない。閾値は固定のサーバー設定） | `precision.rs` |
| 決定的 | モード解決（入力が同じなら結果が同じ純粋な解決） | `sql/mode.rs` |
| 決定的 | プロンプトの固定接頭辞（同じ辞書世代ならバイト単位で同一） | `render_prompt_prefix` |
| 条件付き | バッチ検索の同点順 `(tenant_id, id)` 昇順 | 前提条件あり。[rrf-tie-break-determinism.md](rrf-tie-break-determinism.md) 参照 |
| 条件付き | HNSW：同じ索引・同じクエリでの再現 | [hnsw-search.md](hnsw-search.md)「決定性の保証範囲」。spec 側で未確定の部分あり |
| 非決定になりうる | HNSW のグラフ形状（並列構築 `threads>=2`）と近似結果の差 | [hnsw-parallel-build.md](hnsw-parallel-build.md)。単一スレッド構築は seed が同じなら同一 |
| 非決定になりうる | `hnsw.ef_search`・`hnsw.iterative_scan` 等の設定差による結果差 | README の該当節 |
| 非決定になりうる | LLM の展開結果 | 要求に `model`・`prompt`・`stream`・`keep_alive` のみを送り、`temperature`・`seed` は送らない。安定性は Ollama とモデルの既定に依存。固定するかは要確認 |
| 非決定になりうる | 埋め込みサービスの出力 | 外部サービス依存（要確認） |

再現性を高める現行の手段:

- `USING MODE` を明示して、プランナー推定による切り替わりを避ける。
- 厳密エンジン（既定）を使う。`precision` は常に厳密経路。
- EXPLAIN の `mode`・`mode_source` を結果と一緒に記録する。
- `USING PLAN` を使わず、ベクトルとキーワードを直接指定する（LLM 展開の揺れを避ける）。

## 他実装の機能との対応

他実装側の記述は一般に知られた機能名の範囲に限る。一次資料での再確認が未実施の部分は「要確認」とする。

| 他実装の機能 | 本リポの相当機能 | MODE との関係 | 備考 |
| ------------ | ---------------- | ------------- | ---- |
| Qdrant の quantization と rescoring・oversampling | HNSW の f16／SQ8 常駐（候補生成のみ低精度で、最終スコアは f32 で再計算。opt-in）と、GPU バッチの i8 packed（oversample = 4 で候補を取り f32 で再スコア） | MODE では切り替わらない。`precision` は HNSW 自体を使わない | 要求ごとに rescore の on/off を切り替える手段は無い。根拠: [hnsw-sq8-resident.md](hnsw-sq8-resident.md)・[hnsw-f16-resident.md](hnsw-f16-resident.md)・[gpu-batch-i8-packed.md](gpu-batch-i8-packed.md) |
| pgvector の `hnsw.iterative_scan`・`hnsw.max_scan_tuples` | 同名の `SET`（README の Issue #2033 節）。`max_scan_tuples` は SQL-46 の走査行数上限と `MAX_SEARCH_K` も超えない | `USING MODE` の結果は変わらない（README の記述）。`precision` は HNSW を使わないため無関係 | 他実装の挙動差は要確認 |
| pgvector の `binary_quantize` による二値化→再ランク | `binary_quantize(vector)`、bit 型の `<~>`（Hamming）・`<%>`（Jaccard）で粗選別し、元のベクトルの距離で並べ直す | MODE とは独立。自動の再ランクではなく、利用者が SQL で書く手順 | 例と制約は [column-type-extension.md](column-type-extension.md) の Issue #2023・#2970 追記。HNSW 索引（bit 用）・NoSQL は対象外 |

リランキング（`rerank.rs`）は SQL 実行経路に未結線で、モードとの関係は未定。
結線方針は #3042 で検討中で、`precision` での適用可否は spec 確認待ち（要確認）。

## 要確認・未確定事項

| 項目 | 状態 |
| ---- | ---- |
| precision の閾値・既定値 | 仮置き。TASK-163 で実測確定予定、目標値はオーナー判断待ち |
| LLM の生成オプション（`temperature`・`seed`）の固定 | 未送信。固定するかは未決 |
| 埋め込みサービス出力の再現性 | 外部依存で未検証 |
| `USING PLAN` 失敗後の接続継続の挙動 | 本文書では記述の根拠を一部しか確認していない |
| リランキングの SQL 経路への結線と `precision` での適用 | #3042 で検討中・spec 確認待ち |
| `precision.rs` のモジュールドキュメントの改訂 | #2142 |
| EXPLAIN への融合方式・リランカー行の追加 | #2147（本文書では先取りしない） |
| `ef_search` の直接指定など Phase 7 の反映 | #2145 の依存項目 |
| 他実装の対応表の一次資料確認 | 未実施 |

## 関連文書

- [precision-confidence-gate.md](precision-confidence-gate.md)
- [precision-eval-regression.md](precision-eval-regression.md)
- [using-plan-precision-empty-result.md](using-plan-precision-empty-result.md)
- [rrf-tie-break-determinism.md](rrf-tie-break-determinism.md)
- [hybrid-tuning-parameters.md](hybrid-tuning-parameters.md)
- [hnsw-search.md](hnsw-search.md)・[hnsw-parallel-build.md](hnsw-parallel-build.md)
- [ann-index-adoption.md](ann-index-adoption.md)
- [column-type-extension.md](column-type-extension.md)
