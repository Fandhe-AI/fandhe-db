# SIMD カーネル生成コード検査ガード（要素ごと挿入命令の不在チェック）

- ステータス: **Accepted**（CI ガード追加。production コード〔`crates/engine/src/`〕は無変更）
- 対応: Issue #467（親 #456／ルート #455）
- 関連ポインタ: TASK-156（CORE-14。`isa.rs` の実行時 ISA 検出）・
  `docs/design/chip-kernel-guidelines.md`§0.3・`docs/design/hotpath-implementation-survey.md`

## 1. 背景・目的

`crates/engine/src/isa.rs` は現状 intrinsics を使わず、`#[target_feature]` 付き
safe fn 内の `dot_lanes::<LANES>`（`as_chunks` による固定長チャンク化 ＋
`f32::mul_add`）を LLVM の自動ベクトル化に委ねている。`chip-kernel-guidelines.md`
§0.3 で検討した後続 Phase（f16 常駐等）の intrinsics 導入方式は、「`as_chunks` で
得た固定長配列の要素から `_mm256_set_ps`／`_mm_set_epi16` 等を構築し、新規
`unsafe` を持たない」形を想定している。

この `set` 構築が単一のロード命令（`vmovups`／`vcvtph2ps` 等のメモリオペランド
付き命令）へ畳み込まれるのは **LLVM の最適化挙動であって言語仕様の保証ではない**。
連続要素からの `_mm256_set_ps(a[7], a[6], ..., a[0])` は現行 rustc/LLVM では
単純なロードへ最適化されるが、この前提が rustc/LLVM の更新やコードの書き方の
変化で崩れた場合、要素ごとの挿入命令（`vinsertps`／`vpinsr*`／`vunpck*` 等）が
アセンブリに残ってしまい、SIMD 化の意図（複数要素を 1 命令でロードする）が
実質的に達成されないまま気付かれない可能性がある。

本ガードは、実際に生成されたアセンブリを機械検査することで、この前提の崩れを
CI 段階（PR 作成前）で検出できるようにする。

## 2. 検査方式

`scripts/check_simd_codegen.sh`（`check_sort_determinism.sh`・`check_core_api.sh`
と同型の、`cargo test` を経由しない軽量シェルスクリプト）が以下を行う。

1. `cargo rustc -p engine --release --lib -- --emit asm` で release ビルドの
   アセンブリ（`target/.../deps/engine-<hash>.s`）を生成する（追加の `-C` フラグは
   付けない。実際の release ビルドとの忠実性を優先）
2. 生成された `.s` から、マングル名に `isa` モジュールのセグメント（`3isa`）を
   含む関数をラベル〜`.cfi_endproc` 単位で抽出する
3. target 別の必須シンボル（下記）が対象集合に含まれることを確認する
   （vacuous pass 防止。§3 参照）
4. 各関数の命令列に禁止命令（§4）が 1 件でも含まれていれば非ゼロで終了する

### なぜ「関数名」ではなく「モジュール単位」で対象を絞るか

x86_64 では `dot_avx2_fma`／`dot_avx512` は独立シンボルとして残る（`#[target_
feature]` fn は非 feature 呼び出し元へインライン化されないため）。一方
**aarch64 では `dot_neon` のシンボルが存在しない**——NEON は aarch64 の baseline
feature のため `SimdKernel::dot` 本体へインライン化される。関数名で対象を固定
すると aarch64 側の検査が何も対象を見つけられず、常に pass する（vacuous pass）
状態になってしまう。そのため対象は「`isa` モジュール配下の全関数」とし、
代わりに target 別の必須シンボルが実際に抽出されていることを別途検査する。

必須シンボル（マングル名の長さ接頭辞付きセグメント）:

| target | 必須シンボル |
| ------ | ------------ |
| x86_64 | `dot_avx2_fma` かつ `dot_avx512`（各シンボルは Issue #528 以降 `PADDED_TAIL` の `false`／`true` 2 monomorphization として現れる） |
| aarch64 | `SimdKernel::dot_with_scalar_tail` かつ `SimdKernel::dot_with_padded_tail`（`dot_neon` はインライン化されるため対象外。Issue #528 で `SimdKernel::dot` から更新——`dot_impl<const PADDED_TAIL: bool>` を共有本体化したことで `dot` は LLVM の関数マージによりラベルを持たないエイリアス（`.s` 上は `.set`）として出力され、独立シンボルとして現れなくなったため） |

## 3. 禁止命令集合と根拠

| target | 禁止パターン |
| ------ | ------------ |
| x86_64 | `vinsertps`／`insertps`、`vpinsr{b,w,d,q}`／`pinsr{b,w,d,q}`、`vunpcklps`／`vunpckhps` |
| aarch64 | レーン指定 `ld1 {...}[n]`、`ins v...`、`mov v_.{b,h,s,d}[n]` |

`vinsertf128`／`vinserti128`（128→256 bit の結合命令）は**禁止集合に含めない**。
これは 128 bit レジスタ 2 個を 256 bit へ結合する正当な操作であり、要素ごとの
ギャザー構築とは別物。実測で `_mm_insert_ps` 明示 intrinsic や変換後の
`_mm256_set_ps`（型変換を伴う構築）はこの命令へ最適化されることを確認しており、
これらを誤って禁止すると自然な実装まで拒否してしまう（self-test fixture として
不適と判断した経緯は §6 参照）。

`vunpcklps`／`vunpckhps` は現行基線（§5）で 0 件だが、水平和（レーン総和）の
実装次第では将来出現しうる。出現した場合はパターンの緩和ではなく、該当関数の
実装方法（水平和の書き方）を見直す運用とする。

## 4. パターンを弱める上書き経路の不存在

`isa.rs` の CORE-12 方針（環境変数・設定ファイルによる ISA 検出の上書き禁止）と
同じ思想で、本スクリプトは検査パターンを弱める・無効化する環境変数上書き経路を
一切持たない。`check_sort_determinism.sh` が備える行コメント許可マーカー方式
（`sort-determinism: allow <理由>`）のような個別除外機構も設けない。パターン変更
は本スクリプト自体の修正（＝コードレビュー経由）に限定する。

環境変数 `SIMD_CODEGEN_TARGET_DIR` は `cargo rustc --emit asm` の出力先ディレクトリ
を分離するためだけに使い、検査ロジックには一切影響しない。

## 5. 基線記録（現行 `isa.rs`。rustc 1.96.0・LLVM 22.1.2）

現行コードは本ガードを pass する（禁止命令 0 件）。実測した主要命令カウント:

- x86_64
  - `dot_avx2_fma`: `vfmadd231ps` ×4・`vfmadd132ps` ×1・`vmovups` ×5、
    水平和は `vshufps` ×2／`vshufpd` ×2・`vmovshdup` ×2・`vextractf128` ×1
  - `dot_avx512`: `dot_avx2_fma` に加え `vextractf32x4` ×2
  - 禁止命令（`vinsertps`／`vpinsr*`／`vunpck*`）はいずれも 0 件
- aarch64
  - `SimdKernel::dot`: `ldr q`／`ldp q`・`fmla v.4s`・`dup v.4s`
  - レーン挿入命令（`ld1 {...}[n]`／`ins v`／`mov v_.[bhsd][`）は 0 件

### Issue #528 以降の追記

`dot_lanes` へ `const PADDED_TAIL: bool` を追加し、端数（tail）処理方式を
現行のスカラー逐次和（`false`）／零埋め固定長バッファによる分岐なし tail
（`true`）で切り替え可能にした（詳細は `docs/design/dot-kernel-branchless-tail.md`
参照。既定経路の挙動は不変）。x86_64 では `dot_avx2_fma`／`dot_avx512` の
マングル名は共通のため両 monomorphization が同一の必須シンボル検査を満たす。
aarch64 では `SimdKernel::dot` がエイリアス化され独立シンボルとして現れなく
なったため、必須シンボルを `SimdKernel::dot_with_scalar_tail`／
`dot_with_padded_tail`（`dot_impl::<false>`／`dot_impl::<true>` の薄い
ラッパー）へ更新した（`required_segments_for` の aarch64 分岐）。禁止命令は
`padded_tail_sum`（新設。零埋めバッファへの 1 要素ずつのコピー ＋ 積 ＋
`iter().sum()`）を含め x86_64・aarch64 いずれも 0 件のまま。

### Issue #510 以降の追記（4 行ブロックカーネルの必須シンボル追加と実測知見）

`isa::x86_block4::dot_block4_avx2_fma`／`dot_block4_avx512`（TASK-156・CORE-14。
`search_range` の 4 行ブロックカーネル。詳細は `docs/design/dot-kernel-row-block.md`
参照）を必須シンボルへ追加した（`required_segments_for` の x86_64 分岐に
`dot_block4_avx2_fma`／`dot_block4_avx512` を追加。両関数は `isa.rs` の
`mod x86_block4;` サブモジュール配下だが、マングル名は
`_ZN6engine3isa10x86_block4...` の形で `3isa` セグメントを含み続けるため、
既存の「モジュール単位」抽出方式（本 doc §2「なぜ関数名ではなくモジュール単位で
対象を絞るか」）は変更せずそのまま対象に含まれる）。aarch64 側の必須シンボルは
Issue #510 時点では不変（`Neon` variant は 4 × `dot` の縮退のみで新規シンボルを
持たなかった。NEON 版は Issue #511 で実装し必須シンボルを追加した。下記
「Issue #511 以降の追記」参照）。

**実装過程で判明した禁止命令の再混入経路（本ガードの実効性を裏付ける実例）**:
当初 `_mm256_set_ps`（レーンロード。`load8`/`load16`）から得たレーンを
`[f32; LANES]` 配列へ詰めてから `iter().sum()` する構成にしたところ、本ガードが
`vinsertps`／`vunpcklps`／`vunpckhps` を検出して fail した。実測（`.s` 直接確認）
により、①主ループ自体（`vmovups` ×1 + `vfmadd231ps` ×4／行）は禁止命令 0 件で
意図どおりだったが、②ループ後の「4 行分の水平和を `[f32; 4]` の戻り値として
1 回で返す」処理を LLVM の SLP ベクトライザが見つけ、4 本の独立したスカラー
水平和を `vinsertps`/`vunpck*` で 1 個の SIMD レジスタへ再構成してから
1 回の `vmovups` ストアへまとめる最適化を行っていたことが原因と判明した
（`[f32; LANES]` を経由しないスカラー直接縮約〔`lane_sum8`/`lane_sum16`〕へ
変更しても、戻り値が `[f32; 4]` である限り同じ再パックが起きた）。
最終的に `dot_block4_avx2_fma`/`dot_block4_avx512` の戻り値を `[f32; 4]` の
1 回の戻り値ではなく **4 個の独立した `&mut f32` 出力引数**へ変更し、`[f32; 4]`
の組み立てを呼び出し元（`#[target_feature]` を持たないプレーンな関数
`isa.rs::SimdKernel::dot_block4_impl`）側で行うことで、SLP が対象を見つけられなく
なり禁止命令が消えることを確認した。実測命令サマリ（本開発環境・rustc
1.96.0）:

- `dot_block4_avx2_fma`: 主ループ `vmovups` ×1・`vfmadd231ps` ×4／行、水平和は
  `vshufps`・`vshufpd`・`vextractf128`・`vmovshdup`・`vaddss` 主体。禁止命令 0 件
- `dot_block4_avx512`: 同型に加え `vextractf32x4`・`vpxord`。禁止命令 0 件

この経緯は「LLVM の最適化はコードの書き方の些細な違いで挙動が変わり、
機械検査なしには気付けない」という本 ADR §1 の動機を実例で裏付けるものであり、
`docs/design/dot-kernel-row-block.md`「レーン和をスカラー直接縮約にした理由」
節にも同じ原因分析を記録する。

### Issue #511 以降の追記（NEON 行ブロックカーネルの必須シンボル追加と実測知見）

`isa::neon_block4::dot_block4_neon`（TASK-156・CORE-14。`search_range` の
4 行ブロックカーネルの NEON 版。詳細は `docs/design/dot-kernel-row-block.md`
参照）を必須シンボルへ追加した（`required_segments_for` の aarch64 分岐に
`15dot_block4_neon` を追加。`isa.rs` の `mod neon_block4;` サブモジュール配下
だが、マングル名は `_ZN6engine3isa11neon_block4...` の形で `3isa` セグメントを
含み続けるため、既存の「モジュール単位」抽出方式は変更せずそのまま対象に含む）。
`#[inline(never)]` を付与している（NEON は aarch64 の baseline のため、付けないと
呼び出し元 `dot_block4_impl` へインライン化され独立シンボルとして現れなくなる。
`dot_f16_neon_fp16`〔Issue #514〕と同じ理由）。

**非 vacuous 検査の追加**: x86 版（Issue #510）は `[f32; 4]` 戻り値での SLP
再パック問題があったが禁止命令の「あってはならない命令」検査のみで十分だった。
NEON 版も事前検証（`docs/design/dot-kernel-row-block.md` §2.3）で同型の問題
（`mov v.s[i]` 再パック）を確認し、`&mut f32` 出力引数の形で回避したが、
「`vfmaq_f32` が実際に `fmla v.4s` へコンパイルされたか」を確認する非 vacuous
検査（`expected_rules_for` の aarch64 分岐に `dot_block4_neon` →
`^[[:space:]]*fmla[[:space:]]+v[0-9]+\.4s` を追加。`dot_f16_neon_fp16`
〔Issue #514・A1〕と同じ方針）も追加した。ソフトウェア縮退（スカラー逐次和のみ）
への静かな退行を、禁止命令検査（何も検出しない）ではなく非 vacuous 検査が
検出できることを self-test fixture（`fx_pass_block4_neon.rs`／
`fx_fail_block4_neon_scalarized.rs`）で確認済み。

実測命令サマリ（本開発環境・`--target aarch64-unknown-linux-gnu`・rustc
1.96.0。クロスコンパイルのため実機実行ではなく `--emit asm` の静的サマリ）:

```text
dot_block4_neon: ldr=34 fmla=4 fadd=25 fmul=12 dup=12 movi=12 cmp=17 csel=9 and=11 ...（禁止命令 0 件）
```

`fmla` が 4 件（4 行分のアキュムレータそれぞれ 1 件以上）出現し、
`ins`／`mov v.s[i]`／`ld1 {}[n]`（禁止命令）は 0 件であることを確認した
（`make simd-codegen-check-cross` の実測。x86 版 §「Issue #510 以降の追記」と
同じ形式で記録）。

## 6. self-test fixture の設計

`--self-test` は検査ロジック自体の回帰テストモードで、実ソースを使わずその場で
生成した fixture を `rustc -O --crate-type lib --emit asm` でコンパイルし、以下を
確認する。

- pass すべき形: `as_chunks::<N>` の**連続**要素から `_mm256_set_ps`／
  `vsetq_lane_f32` を構築するコード（現行 `dot_lanes` 相当の自動ベクトル化を含む）
  → 禁止命令 0 件で pass すること
- fail すべき形: `as_chunks::<2N>` から**ストライド 2** で要素を抜き出す
  ギャザー構築 → x86_64 は `vinsertps`、f16 変換を伴う `_mm_set_epi16` は
  `vpinsrw` を検知して fail すること。aarch64 のストライド `vsetq_lane_f32` は
  レーン指定 `ld1 {...}[n]` を検知して fail すること
- 必須シンボル不在の fixture → 「required symbol missing」で fail すること

不適だった fixture（再提案防止のための記録）:

- `_mm_insert_ps` 明示 intrinsic は `vbroadcastss` へ最適化され、`vinsertps` を
  **残さない**ため fail fixture として不適
- 要素変換後の `set_ps`（型変換を挟む構築）は `vinsertf128`／`vshufps` へ最適化
  され、`vinsertps` を残さないため同様に不適

## 6.1. Issue #522 追記: 整数 i8×i8 dot カーネル・`mnemonic_of` バグ修正

VNNI（512bit／256bit）と i16 widen フォールバックの整数 dot カーネル
（`isa/x86_i8.rs`。`docs/design/hnsw-sq8-resident.md`「Issue #522」節参照）を
本ガードへ登録した。

**発見: AVX-VNNI の `{vex}` encoding hint 接頭辞バグ**

AVX-VNNI（`avx2,avxvnni`。AVX-512 を要さない VEX 符号化 VNNI 命令）は
LLVM の `.s` 出力で `{vex}\tvpdpbusd ...` のように先頭へ符号化方式を示す
波括弧トークンが付く（`{evex}`／`{disp32}` 等、他の encoding hint も同型で
現れうる）。旧 `mnemonic_of` はこの接頭辞を先頭トークンとして扱い、実際の
ニーモニック（`vpdpbusd`）ではなく `{vex}` を返していたため、禁止命令検査
（§3）・期待命令検査（§6）の双方が対象行を一切見ないまま素通りしていた
（本 Issue の self-test fixture 作成時に判明。既存 f16／block4 カーネルへの
影響は無い——`{vex}` は AVX-VNNI の VEX 符号化明示にのみ現れる）。波括弧で
囲まれたトークンを除去してからニーモニックを取る形へ修正した。

**新規必須シンボル・期待命令規則**

| 関数 | 必須シンボル | 期待命令規則 |
| ---- | ------------ | ------------ |
| `dot_i8_avx512_vnni` | `18dot_i8_avx512_vnni` | メモリオペランド付き `vpdpbusd` on `%zmm`（`{vex}`／`{evex}` 接頭辞の有無を許容する正規表現） |
| `dot_i8_avx_vnni` | `15dot_i8_avx_vnni` | 同上・`%ymm` |
| `dot_i8_avx2_widen` | `17dot_i8_avx2_widen` | `vpmaddwd`（1 件以上）＋メモリオペランド付き `vpmovsxbw` の両方 |

**判断: byte/word 要素の逐次パック検出（`punpcklbw`／`punpcklwd` 等）は
不採用**

計画段階では「禁止命令へ byte/word unpack 系（`punpcklbw`／`vpunpcklbw`／
`punpcklwd`／`vpunpcklwd` 等）を追加する」ことを検討したが、実装・実測の
結果**不採用**とした。理由: `scan_forbidden`（§3）はモジュール内で検出した
全関数の命令列を無差別に走査する構造（対象を「手書き intrinsics カーネル」
に限定しない）のため、`i8`→`i32` の要素ごと符号拡張をコンパイラが自動
ベクトル化で正当に punpck 系（x86_64）／`mov v.b[..]`（aarch64）へ変換する
スカラー参照実装（`dot_i8_scalar`）まで誤って fail させた（f32／f16 の
スカラー参照実装が偶然この命令を出さないだけで、i8 幅拡張という演算特性に
起因する誤検出）。本ガードが検出したいのは「手書き `_mm*_set_epi8` 構築が
gather／stride 由来で per-element insert 命令へ縮退した」ケースであり、
これは既存の `pinsrb`／`vpinsrb` 系禁止命令が既に捕捉する。`dot_i8_scalar`
は関数名（`*dot_i8_scalar*`）で `scan_forbidden` の対象から明示的に除外し
（`dot_scalar`／`dot_f16_scalar` と同じ「スカラー参照実装は本ガードの対象
外」という位置付け）、`isa.rs::dot_i8_scalar` へ `#[inline(never)]` を付与
して `I8Kernel::dot_i8` ディスパッチ本体へインライン化されないようにした
（インライン化されると、ディスパッチ本体自身の命令列に上記の自動ベクトル化
結果が紛れ込み、除外規則をすり抜けて誤検出が再発するため）。

**self-test fixture**

pass: `dot_i8_avx512_vnni`／`dot_i8_avx2_widen` と同型（`set` 構築のみ）。
fail: 関数名は `dot_i8_avx512_vnni` だが実体はスカラー逐次和（`vpdpbusd`
非搭載）——`fx_fail_f16_missing_instruction` と同型の非 vacuous 検査対象。

## 6.2. Issue #525 追記: aarch64 NEON dotprod 整数 i8×i8 dot カーネル

NEON dotprod（`isa/neon_i8.rs::dot_i8_neon_dotprod`。
`docs/design/hnsw-sq8-resident.md`「Issue #525」節参照）を本ガードへ
登録した。

**新規必須シンボル・期待命令規則**

| 関数 | 必須シンボル | 期待命令規則 |
| ---- | ------------ | ------------ |
| `dot_i8_neon_dotprod` | `19dot_i8_neon_dotprod` | `sdot v[0-9]+\.4s`（s8x16->i32 dot-product-accumulate。1 件以上） |

**self-test fixture**

pass: `dot_i8_neon_dotprod` と同型（`vsetq_lane_s8::<0..15>` 昇順連鎖に
よる `int8x16_t` 構築＋`vdotq_s32`）。禁止命令 0 件・`sdot v.4s` 1 件以上
を確認する。
fail: 関数名は `dot_i8_neon_dotprod` だが実体はスカラー逐次 wrapping 和
（`sdot` 非搭載）——`fx_fail_i8_missing_instruction`（Issue #522）と同型の
非 vacuous 検査対象。

**aarch64 基線命令カウント（1.98.0 toolchain・`--emit asm` 実測。実ソース
`isa/neon_i8.rs::dot_i8_neon_dotprod` の release ビルド）**

`ldr=2 subs=2 sdot=1 addv=1 fmov=1` を含む（禁止パターン `mov v.[bhsd][`／
`ins v`／`ld1 {}[n]` はいずれも 0 件）。レジスタ構築の採否比較（`vcombine_s8`
＋`vcreate_s8` が `mov v.d[1], v.d[0]` を残し禁止パターンに抵触した実測、
`vsetq_lane_s8` 連鎖が単一の `ldr q` へ畳み込まれた実測）は
`docs/design/hnsw-sq8-resident.md`「Issue #525」節「レジスタ構築の実測
比較」参照。

## 7. 既知の限界

- 本ガードは「現行の LLVM が特定の書き方をどう最適化するか」を固定するもので
  あり、rustc/LLVM の更新で最適化挙動が変わればガードが fail する（これは
  ガードの目的そのもの——期待どおり検出できたことを意味する）
- `[profile.release]` に LTO・codegen-units 変更を加えた場合、生成される
  シンボル構成が変わりうる。必須シンボル検査（§2 手順 3）がその種の構成変更を
  検知する
- aarch64 は本開発環境でのクロスコンパイル（`cargo rustc --target
  aarch64-unknown-linux-gnu --emit asm`。リンク不要）による検証であり、実機での
  検証は対象外（`docs/design/hnsw-parallel-build.md` 等、他の aarch64 関連確認と
  同様に別 Issue の管轄）
- キャッシュ（actions/cache 等）を導入していないため、CI 実行のたびに engine
  クレートの release フルビルド（wgpu 等の依存を含む）が走る。CI 実行時間が
  問題になった場合はキャッシュ導入を検討する（follow-up）

## 8. 運用

検査パターンの変更・対象モジュールの追加（将来 `f16.rs` 等の新設カーネルへの
拡張）は、`scripts/check_simd_codegen.sh` の修正と本 ADR の更新をセットで行う。

### toolchain 追従の早期検知（Issue #1477）

`rust-toolchain.toml` は stable 追従のため、runner image 更新による rustc 切替で本ガードが
PR と無関係に失敗しうる（Issue #1475）。次の 2 点で原因特定と先行検知を行う。

- `ci.yml` の `simd-codegen-check`・`cross-check` は `make` の前に `rustc -vV`／`cargo -vV` をログへ出す。
- `.github/workflows/toolchain-canary.yml` が beta で同検査（x86_64・aarch64）を週次実行する
  （schedule＋workflow_dispatch。PR ゲート外で、必須チェックではない）。

失敗時は `rustc -vV` の release と LLVM version を直前の成功 run と比較し、ガードの検出が
期待どおりか、検査側の追随修正が要るかを判断する。検査パターンを弱めて通すことはしない（§4）。
ローカル再現: `RUSTUP_TOOLCHAIN=beta make simd-codegen-check`（要 `rustup toolchain install beta`。
aarch64 は `make simd-codegen-check-cross`）。§7 の CI 時間・キャッシュの件とは独立。
