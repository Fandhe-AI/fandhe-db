#!/usr/bin/env bash
# Issue #467（ポインタ: TASK-156・CORE-14）の SIMD カーネル生成コード検査ガード。
#
# 動機: `crates/engine/src/isa.rs` は現状 intrinsics を使わず、
# `#[target_feature]` 付き safe fn 内の `dot_lanes::<LANES>`（`as_chunks` ＋
# `f32::mul_add`）を LLVM の自動ベクトル化に委ねている。後続 Phase（f16 常駐等）で
# 導入予定の intrinsics は「`as_chunks` で得た固定長配列から `_mm256_set_ps` 等を
# 構築し、新規 `unsafe` を持たない」方式を採る予定だが、この `set` 構築が単一の
# ロード命令（`vmovups`／`vcvtph2ps` 等）へ畳み込まれるのは LLVM の最適化挙動で
# あって言語仕様の保証ではない（判断根拠: `docs/design/chip-kernel-guidelines.md`
# §0.3・`docs/design/simd-codegen-guard.md`）。
#
# 本スクリプトは `cargo rustc --emit asm` で実際に生成されたアセンブリを走査し、
# `isa` モジュール配下の関数に「要素ごと挿入命令」（x86_64: `vinsertps`／
# `vpinsrb/w/d/q`／`vunpcklps`／`vunpckhps`。aarch64: レーン指定 `ld1 {...}[n]`・
# `ins v`・`mov v_.{b,h,s,d}[n]`）が残っていないかを機械検査する。rustc/LLVM の
# 更新やコード変更でこれらの命令が再混入した場合に CI 段階で検出することが目的
# （`scripts/check_sort_determinism.sh` と同型の、`cargo test` 経由ではない
# 軽量シェルスクリプト方式）。
#
# 検査対象を「`isa` モジュール配下の全関数」にしている理由（関数名固定にしない
# 理由）: aarch64 では `dot_neon` は baseline feature のため `SimdKernel::dot`
# 本体へインライン化され、独立シンボルとして生成されない。関数名で対象を絞ると
# aarch64 側の検査が何も対象を見つけられず vacuous pass してしまうため、
# モジュール単位（マングル名のセグメント `3isa`）で抽出したうえで、
# target 別の必須シンボル（x86_64: `dot_avx2_fma`＋`dot_avx512`／aarch64:
# `SimdKernel::dot`）が実際に対象集合へ含まれることを別途検査し、
# 抽出そのものが空振りしていないことを保証する（fail-closed）。
#
# `vinsertf128`／`vinserti128`（128→256 bit の結合）は禁止命令に含めない
# （128 bit レジスタ 2 個を 256 bit へ結合する正当な操作であり、要素ごとの
# ギャザー構築とは別物。実測で `_mm_insert_ps` 明示 intrinsic や変換後の
# `set_ps` はこの命令へ最適化されることを確認済み）。
#
# 検査パターンを弱める・無効化する環境変数上書き経路は設けない
# （`isa.rs` の CORE-12 方針・`check_sort_determinism.sh` の許可マーカー方式とも
# 異なり、本スクリプトはコード内マーカーによる例外も持たない。パターン変更は
# 本スクリプト自体の修正＝コードレビュー経由に限定する）。
#
# `--self-test` は検査ロジック自体の回帰テストモード。実ソースを使わず、
# その場で生成した fixture を `rustc -O --crate-type lib --emit asm` で
# コンパイルし、「pass すべき形（連続要素からの `set` 構築）を pass」
# 「fail すべき形（ストライドありのギャザー構築）を fail」「必須シンボル不在を
# fail」を検証する（`make simd-codegen-check`／`make simd-codegen-check-cross`・
# CI の `simd-codegen-check` ジョブ・`cross-check` ジョブから呼ばれる）。
#
# 使い方: scripts/check_simd_codegen.sh [--target <triple>] [--self-test]
#
# 環境変数: SIMD_CODEGEN_TARGET_DIR（既定 target/simd-codegen）。
# `cargo rustc --emit asm` の出力先を分離するためだけに使う（同一 deps
# ディレクトリにビルドごとに別ハッシュの `.s` が残留すると判明したため、
# 検査前に該当パターンの `.s` を削除したうえで「ちょうど 1 個」であることを
# 要求する）。

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

TARGET=""
MODE="check"

while [ $# -gt 0 ]; do
  case "$1" in
    --self-test)
      MODE="self-test"
      shift
      ;;
    --target)
      if [ $# -lt 2 ]; then
        echo "ERROR: --target requires a value" >&2
        exit 1
      fi
      TARGET="$2"
      shift 2
      ;;
    *)
      echo "ERROR: unknown argument: $1" >&2
      echo "usage: $(basename "$0") [--target <triple>] [--self-test]" >&2
      exit 1
      ;;
  esac
done

if [ -z "${TARGET}" ]; then
  TARGET="$(rustc -vV | sed -n 's/^host: //p')"
  if [ -z "${TARGET}" ]; then
    echo "ERROR: failed to determine host target via 'rustc -vV'" >&2
    exit 1
  fi
fi

case "${TARGET}" in
  x86_64-*)
    ARCH_CLASS="x86_64"
    ;;
  aarch64-*)
    ARCH_CLASS="aarch64"
    ;;
  *)
    echo "ERROR: unsupported target: ${TARGET} (only x86_64-*/aarch64-* are supported)" >&2
    exit 1
    ;;
esac

TARGET_DIR="${SIMD_CODEGEN_TARGET_DIR:-${REPO_ROOT}/target/simd-codegen}"

# 対象モジュールのマングル名セグメント（例: `_ZN6engine3isa...`）に一致する
# 関数だけをラベル〜`.cfi_endproc` 単位で抽出する。第 2 引数の module_segment は
# 正規表現（perl 拡張）として扱う。
#
# ラベル行の判定は先頭の追加アンダースコア（`_`）を許容する: Mach-O（macOS。
# aarch64-apple-darwin／x86_64-apple-darwin）では ELF/COFF と異なり全シンボルへ
# リンカが追加のアンダースコアを 1 つ付与するため、legacy マングリング
# `_ZN...` は `__ZN...`、v0 マングリング `_R...` は `__R...` としてアセンブリへ
# 出力される（実体は同じシンボルであり、後続の module_segment 部分一致・
# required_segments_for の照合はどちらの形式でも変わらず動作する）。
#
# 抽出結果は「関数名<TAB>命令行...（改行区切り）」のブロックを `\x01` 区切りで
# 標準出力へ書き出す（呼び出し元がブロック単位で走査する）。
extract_functions() {
  local asm_file="$1"
  local module_segment="$2"
  # 状態機械: `waiting` → ラベル直後の行を見て関数（`.cfi_startproc` が続く）か
  # データ（`CURRENT`（`OnceLock` 静的変数）等の `.asciz`／`.zero`／`.quad` 等が
  # 続く）かを判別する。データの場合は「次に `_ZN`/`_R` ラベルが現れるまで
  # 際限なく行を溜め込む」誤りを避けるため、この時点で追跡を打ち切る
  # （`.L` から始まるローカルラベル・アンカーはブロック境界と見なさないため、
  # 判別を誤ると同一クレート内の無関係な後続シンボルまで巻き込む）。
  perl -ne '
    BEGIN { $state = "waiting"; $name = ""; @lines = (); }
    if ($state eq "after_label") {
      if (/^\s*\.cfi_startproc\s*$/) {
        $state = "in_function";
        next;
      } else {
        $state = "waiting";
        $name = "";
        @lines = ();
        # このデータ定義行自体がラベル行である可能性は構造上ないため
        # そのまま読み捨て、次行から通常探索を継続する。
      }
    }
    if (/^(_?(?:_ZN|_R)[A-Za-z0-9_.\$]*):$/) {
      $name = $1;
      @lines = ();
      $state = "after_label";
      next;
    }
    if ($state eq "in_function") {
      if (/^\s*\.cfi_endproc\s*$/) {
        if ($name =~ /\Q'"${module_segment}"'\E/) {
          print "$name\x02" . join("\x03", @lines) . "\x01";
        }
        $state = "waiting";
        $name = "";
        @lines = ();
        next;
      }
      push @lines, $_;
    }
  ' "${asm_file}"
}

# 命令行から先頭の空白・末尾改行を除いたニーモニック（先頭トークン）を返す。
# `.` で始まるディレクティブ行・ラベル行（末尾 `:`）は対象外（アセンブリ行は
# 必ず先頭にタブ／空白を伴うため、トリムしてから判定する）。
#
# Issue #522 で発見: AVX-VNNI（`avxvnni`。AVX-512 を要さない VEX 符号化
# VNNI 命令）は LLVM の出力で `{vex}\tvpdpbusd ...` のように先頭へ符号化
# 方式を示す波括弧接頭辞トークンが付く（`{evex}`／`{disp32}` 等、他の
# encoding hint も同型で現れうる）。この接頭辞を先頭トークンとして扱うと
# `mnemonic_of` が実際のニーモニック（`vpdpbusd`）ではなく `{vex}` を返し、
# 禁止命令検査（`scan_forbidden`）・期待命令検査（`expected_rules_for`）の
# 双方が実際の命令を一切見ないまま素通りしてしまう（`--self-test` の fail
# fixture が本バグを再現する。詳細は `docs/design/simd-codegen-guard.md`
# 参照）。波括弧で囲まれたトークンは除去してからニーモニックを取る。
mnemonic_of() {
  local line="$1"
  local trimmed
  trimmed="$(echo "${line}" | sed -E 's/^[[:space:]]+//; s/[[:space:]]*$//')"
  case "${trimmed}" in
    ""|.*|*:)
      return 1
      ;;
  esac
  # 先頭の `{...}` encoding hint 接頭辞（`{vex}`／`{evex}` 等）を除去し、
  # 続く空白も取り除いてから実際のニーモニックを取る。
  trimmed="$(echo "${trimmed}" | sed -E 's/^\{[^}]*\}[[:space:]]*//')"
  echo "${trimmed}" | sed -E 's/[[:space:]].*$//'
}

# 1 関数分の命令行（\x03 区切り）に禁止命令が含まれるかを判定する。
# 該当があれば「行番号は付けず命令名のみ」を 1 行 1 件、標準出力へ列挙し、
# 呼び出し元が非ゼロで終了する（該当なしは何も出力しない）。
scan_forbidden() {
  local arch_class="$1"
  local body="$2"
  local IFS=$'\x03'
  local found=1
  for line in ${body}; do
    local mnem
    mnem="$(mnemonic_of "${line}")" || continue
    if [ "${arch_class}" = "x86_64" ]; then
      case "${mnem}" in
        insertps|vinsertps)
          echo "${mnem}"
          found=0
          ;;
        pinsrb|vpinsrb|pinsrw|vpinsrw|pinsrd|vpinsrd|pinsrq|vpinsrq)
          echo "${mnem}"
          found=0
          ;;
        unpcklps|vunpcklps|unpckhps|vunpckhps)
          echo "${mnem}"
          found=0
          ;;
      esac
    else
      case "${mnem}" in
        ins)
          # `ins v...` のみ対象（他の `ins` 系ニーモニックは aarch64 に存在しない）。
          if echo "${line}" | grep -Eq '^\s*ins\s+v'; then
            echo "ins"
            found=0
          fi
          ;;
        mov)
          if echo "${line}" | grep -Eq '^\s*mov\s+v[0-9]+\.[bhsd]\['; then
            echo "mov(lane)"
            found=0
          fi
          ;;
        ld1)
          if echo "${line}" | grep -Eq '^\s*ld1\s*\{[^}]*\}\[[0-9]+\]'; then
            echo "ld1(lane)"
            found=0
          fi
          ;;
      esac
    fi
  done
  return "${found}"
}

# 1 関数分の命令行を集計し「ニーモニック: 件数」を頻度降順で 1 行にまとめる
# （基線サマリ表示用。何を検査しているかを人間が確認できるようにする）。
summarize_instructions() {
  local body="$1"
  local IFS=$'\x03'
  local -A counts=()
  local order=()
  for line in ${body}; do
    local mnem
    mnem="$(mnemonic_of "${line}")" || continue
    if [ -z "${counts[${mnem}]+x}" ]; then
      counts[${mnem}]=0
      order+=("${mnem}")
    fi
    counts[${mnem}]=$((counts[${mnem}] + 1))
  done
  local out=""
  for mnem in "${order[@]}"; do
    out+="${mnem}=${counts[${mnem}]} "
  done
  echo "${out}"
}

# 必須シンボル（target 別）の判定用セグメント。
#
# aarch64 側は Issue #528 で `10SimdKernel3dot`（`SimdKernel::dot`）から
# `10SimdKernel20dot_with_scalar_tail`／`10SimdKernel20dot_with_padded_tail`
# （`SimdKernel::dot_with_scalar_tail`／`dot_with_padded_tail`）へ置き換えた。
# `dot_impl<const PADDED_TAIL: bool>` を共有本体化したことで、`dot` は
# LLVM の関数マージにより独立シンボルとして現れず `dot_with_scalar_tail`
# （`PADDED_TAIL=false`）のエイリアスになるため（計測で確認済み。`.s` 上は
# ラベルではなく `.set` として出力され、本スクリプトのラベル抽出には現れない）、
# 実体を持つ 2 wrapper を必須シンボルとする。x86_64 側は `dot_avx2_fma`／
# `dot_avx512` のマングル名は `PADDED_TAIL` の値に関わらず同一関数名（トップレベル
# の型パラメータを持たない `fn` の識別子部分）を共有し、両 monomorphization
# （`PADDED_TAIL=false`／`true`）が同一セグメントを含むラベルとして現れるため
# 変更不要。
required_segments_for() {
  local arch_class="$1"
  if [ "${arch_class}" = "x86_64" ]; then
    echo "12dot_avx2_fma"
    echo "10dot_avx512"
    # Issue #510（TASK-156・CORE-14）: 4 行ブロックカーネル
    # `isa::x86_block4::dot_block4_avx2_fma`／`dot_block4_avx512`
    # （いずれも `#[target_feature]` fn のため、`dot_avx2_fma`／`dot_avx512` と
    # 同様に独立シンボルとして生成される）を必須シンボルへ追加する。
    echo "19dot_block4_avx2_fma"
    echo "17dot_block4_avx512"
    # Issue #514: f16 昇格 dot カーネル（`isa::dot_f16_f16c`）。
    echo "12dot_f16_f16c"
    # Issue #522: 整数 i8×i8 dot カーネル（`isa::x86_i8::dot_i8_avx512_vnni`／
    # `dot_i8_avx_vnni`／`dot_i8_avx2_widen`。いずれも `#[target_feature]` fn
    # のため独立シンボルとして生成される）。
    echo "18dot_i8_avx512_vnni"
    echo "15dot_i8_avx_vnni"
    echo "17dot_i8_avx2_widen"
  else
    echo "10SimdKernel20dot_with_scalar_tail"
    echo "10SimdKernel20dot_with_padded_tail"
    # Issue #514: f16 昇格 dot カーネル（`isa::dot_f16_neon_fp16`）。
    # `#[inline(never)]` を付与しているため独立シンボルとして必ず現れる。
    echo "17dot_f16_neon_fp16"
    # Issue #511（TASK-156・CORE-14）: 4 行ブロックカーネル
    # `isa::neon_block4::dot_block4_neon`。`#[inline(never)]` を付与している
    # ため（`dot_f16_neon_fp16` と同じ理由。NEON は aarch64 baseline のため
    # 付けないと呼び出し元 `dot_block4_impl` へインライン化され独立シンボルとして
    # 現れない）必須シンボルへ追加する。`PADDED_TAIL` の 2 monomorphization は
    # 同一セグメントを共有する。
    echo "15dot_block4_neon"
    # Issue #525: NEON dotprod 整数 i8×i8 dot カーネル
    # `isa::neon_i8::dot_i8_neon_dotprod`。`dotprod` は aarch64 baseline 対象外
    # のため `#[inline(never)]` を付与しており（`dot_f16_neon_fp16`／
    # `dot_block4_neon` と同じ理由）独立シンボルとして必ず現れる。
    echo "19dot_i8_neon_dotprod"
  fi
}

# 非 vacuous 検査（決定 2・Issue #514・A1）: 「期待命令が対象関数に 1 件以上
# 出現する」ことを要求する rule 表。禁止命令検査（`scan_forbidden`）が
# 「あってはならない命令」を検査するのに対し、本関数は「無ければならない
# 命令」を検査する——f16 カーネルは `set` 構築が実際に intrinsics へ
# コンパイルされたことの証跡（`vcvtph2ps`／`fcvtl`）を要求しないと、
# ソフトウェア復号へ静かに縮退していても検査を通過してしまう
# （`--self-test` の fail fixture 参照）。
#
# 出力は「関数名セグメント<TAB>期待正規表現（拡張正規表現。命令行全体に
# 対して `grep -E` で照合）<TAB>説明」を 1 行 1 rule で返す。対象関数
# セグメントに一致する関数が 1 つも無ければ本検査は素通り
# （`required_segments_for` の必須シンボル検査が既にその欠落を検出する）。
expected_rules_for() {
  local arch_class="$1"
  if [ "${arch_class}" = "x86_64" ]; then
    printf '%s\t%s\t%s\n' \
      "dot_f16_f16c" \
      '^[[:space:]]*vcvtph2ps[[:space:]]+[^,]*\(' \
      "f16->f32 promotion (vcvtph2ps) with a memory operand (register-only form does not count)"
    # Issue #522: VNNI 系カーネルが実際に `vpdpbusd`（u8×s8→i32 積和）へ
    # コンパイルされた証跡を要求する非 vacuous 検査。`{vex}` encoding hint
    # 接頭辞が付き得る（`mnemonic_of` の対処参照）ため、行頭に任意でその
    # トークンが現れることを許容する正規表現にする。メモリオペランド付き
    # （`set` 構築が畳み込まれた形。register-only 版は対象外）を要求する。
    printf '%s\t%s\t%s\n' \
      "dot_i8_avx512_vnni" \
      '^[[:space:]]*(\{[^}]*\}[[:space:]]+)?vpdpbusd[[:space:]]+[^,]*\(.*%zmm' \
      "u8x8->i32 dot-product-accumulate (vpdpbusd) on %zmm with a memory operand"
    printf '%s\t%s\t%s\n' \
      "dot_i8_avx_vnni" \
      '^[[:space:]]*(\{[^}]*\}[[:space:]]+)?vpdpbusd[[:space:]]+[^,]*\(.*%ymm' \
      "u8x8->i32 dot-product-accumulate (vpdpbusd) on %ymm with a memory operand"
    printf '%s\t%s\t%s\n' \
      "dot_i8_avx2_widen" \
      '^[[:space:]]*vpmaddwd[[:space:]]' \
      "i16x16->i32 multiply-add-pairs (vpmaddwd) actually emitted (widen fallback)"
    printf '%s\t%s\t%s\n' \
      "dot_i8_avx2_widen" \
      '^[[:space:]]*vpmovsxbw[[:space:]]+[^,]*\(' \
      "i8->i16 sign-extend widen (vpmovsxbw) with a memory operand"
  else
    printf '%s\t%s\t%s\n' \
      "dot_f16_neon_fp16" \
      '^[[:space:]]*fcvtl2?[[:space:]]+v[0-9]+\.4s' \
      "f16->f32 promotion (fcvtl/fcvtl2 widening to 4s)"
    # Issue #511: `vfmaq_f32` が実際に `fmla v.4s` へコンパイルされた証跡を要求する
    # 非 vacuous 検査（x86 版 `dot_block4_*` には期待規則が無いが、本 Issue は
    # aarch64 側のみを扱う。x86 側の一般化は Issue #510 doc の申し送りのまま）。
    printf '%s\t%s\t%s\n' \
      "dot_block4_neon" \
      '^[[:space:]]*fmla[[:space:]]+v[0-9]+\.4s' \
      "row-block FMA (fmla v.4s) actually emitted (not scalarized)"
    # Issue #525: `vdotq_s32` が実際に `sdot v.4s`（s8x16->i32 dot-product-
    # accumulate）へコンパイルされた証跡を要求する非 vacuous 検査。
    printf '%s\t%s\t%s\n' \
      "dot_i8_neon_dotprod" \
      '^[[:space:]]*sdot[[:space:]]+v[0-9]+\.4s' \
      "s8x16->i32 dot-product-accumulate (sdot v.4s) actually emitted"
  fi
}

# `fn_name`（1 関数）が `expected_rules_for` の対象か判定し、対象なら
# `fn_body`（命令行。\x03 区切り）に期待パターンが 1 件以上出現するかを検査
# する。対象外の関数は常に成功（0）。一致なしは非ゼロ・理由を stdout へ
# 1 行（`scan_forbidden` と同様、行番号は付けない）。
scan_expected_missing() {
  local arch_class="$1"
  local fn_name="$2"
  local fn_body="$3"

  local rules
  rules="$(expected_rules_for "${arch_class}")"
  local rule
  while IFS=$'\t' read -r seg pattern desc; do
    [ -z "${seg}" ] && continue
    case "${fn_name}" in
      *"${seg}"*) ;;
      *) continue ;;
    esac
    local IFS_OLD="${IFS}"
    IFS=$'\x03'
    local found=1
    local line
    for line in ${fn_body}; do
      if echo "${line}" | grep -Eq "${pattern}"; then
        found=0
        break
      fi
    done
    IFS="${IFS_OLD}"
    if [ "${found}" -ne 0 ]; then
      echo "${desc}"
    fi
  done <<< "${rules}"
}

# 生成済み `.s` を対象に、module_segment 配下の関数群を検査する共通本体。
# 成功時は 0・基線サマリを stdout へ出力、失敗時は非ゼロを返し理由を stderr へ
# 出力する。self-test（fixture）・実ビルドの両方から呼ばれる。
run_scan() {
  local asm_file="$1"
  local arch_class="$2"
  local module_segment="$3"
  shift 3
  local required_segments=("$@")

  if [ ! -f "${asm_file}" ]; then
    echo "ERROR: asm file not found: ${asm_file}" >&2
    return 1
  fi

  local blocks
  blocks="$(extract_functions "${asm_file}" "${module_segment}")"
  if [ -z "${blocks}" ]; then
    echo "ERROR: no functions matched module segment '${module_segment}' in ${asm_file}" >&2
    return 1
  fi

  # 必須シンボルの充足確認（vacuous pass 防止）。
  # 照合対象は各ブロックの fn_name のみに限定する（命令本文まで含めて照合すると、
  # 本来抽出すべき関数の本体抽出に失敗していても、別の関数本体に含まれる
  # 呼び出し命令のシンボル名文字列だけで必須条件を満たしてしまい、未検査の
  # カーネルを通過させ得るため。codex-review #566 P1 指摘）。
  # また `grep -q` を `set -o pipefail` 下でパイプの受け手に使うと、一致を
  # 見つけた時点で入力を読み切らずに終了し、書き手側の `echo`／ここでは
  # ヒアストリング展開が SIGPIPE で失敗しうる（`blocks` がパイプ容量を超える
  # 場合）。シンボルが実在するにもかかわらず判定全体が失敗する誤検出を避けるため、
  # パイプを介さずシェル内の文字列一致（`fn_names` へ改行区切りで蓄積し `case`
  # で判定）だけで照合する（codex-review #566 P2 指摘）。
  local fn_names=$'\n'
  local IFS_NAMES_OLD="${IFS}"
  IFS=$'\x01'
  local name_block
  for name_block in ${blocks}; do
    [ -z "${name_block}" ] && continue
    fn_names+="${name_block%%$'\x02'*}"$'\n'
  done
  IFS="${IFS_NAMES_OLD}"

  local seg
  for seg in "${required_segments[@]}"; do
    case "${fn_names}" in
      *"${seg}"*) ;;
      *)
        echo "ERROR: required symbol segment not found among scanned functions: ${seg}" >&2
        echo "  (target=${arch_class} module_segment=${module_segment})" >&2
        return 1
        ;;
    esac
  done

  local status=0
  local IFS_OLD="${IFS}"
  IFS=$'\x01'
  local block
  for block in ${blocks}; do
    [ -z "${block}" ] && continue
    local fn_name="${block%%$'\x02'*}"
    local fn_body="${block#*$'\x02'}"

    # `dot_i8_scalar`（Issue #522）は他の `unsafe` を持たないスカラー参照実装
    # （`dot_scalar`／`dot_f16_scalar` と同じ位置付け）だが、`i8`→`i32` の
    # 要素ごと符号拡張はコンパイラの自動ベクトル化が per-lane 命令（x86_64:
    # `punpcklbw`／`punpcklwd`、aarch64: `mov v.b[..]`）へ正当に変換する対象
    # であり、本検査が本来検出したい「手書き intrinsics カーネルの `set`
    # 構築が gather/stride 由来で per-element insert 命令へ縮退した」ケース
    # とは別物（f32／f16 のスカラー参照実装が同じ理由で偶然この命令を出さない
    # だけで、除外規則自体は既存の `_scalar` 系関数と同じ立ち位置）。
    # モジュール全体を走査する本関数の構造上、これらの関数は `required_
    # segments_for`／`expected_rules_for` の対象にも一切含まれない
    # （スカラー参照実装であり検証対象の SIMD カーネルではないため）。
    case "${fn_name}" in
      *"dot_i8_scalar"*)
        ;;
      *)
        local forbidden
        forbidden="$(scan_forbidden "${arch_class}" "${fn_body}")" || true
        if [ -n "${forbidden}" ]; then
          echo "ERROR: forbidden per-element insert instruction(s) found in ${fn_name}:" >&2
          echo "${forbidden}" | while IFS= read -r item; do echo "  - ${item}"; done >&2
          status=1
          continue
        fi
        ;;
    esac

    local missing
    missing="$(scan_expected_missing "${arch_class}" "${fn_name}" "${fn_body}")" || true
    if [ -n "${missing}" ]; then
      echo "${missing}" | while IFS= read -r item; do
        [ -z "${item}" ] && continue
        echo "ERROR: expected instruction missing in ${fn_name}: ${item}" >&2
      done
      status=1
      continue
    fi

    local summary
    summary="$(summarize_instructions "${fn_body}")"
    echo "ok: ${fn_name}: ${summary}"
  done
  IFS="${IFS_OLD}"

  return "${status}"
}

# --- self-test 用 fixture 生成 -------------------------------------------

# 指定 crate ソースを scratch ディレクトリへ書き出し `rustc -O --crate-type lib
# --emit asm [--target <triple>]` でコンパイルする。生成された `.s` のパスを
# stdout へ返す。
compile_fixture() {
  local scratch="$1"
  local src_file="$2"
  local target="$3"

  mkdir -p "${scratch}"
  local target_flag=()
  if [ -n "${target}" ]; then
    target_flag=(--target "${target}")
  fi
  if ! rustc -O --crate-type lib --emit asm "${target_flag[@]}" \
      --out-dir "${scratch}" "${src_file}" 2>"${scratch}/rustc.log"; then
    echo "ERROR: fixture compilation failed (see ${scratch}/rustc.log)" >&2
    cat "${scratch}/rustc.log" >&2
    return 1
  fi

  local crate_name
  crate_name="$(basename "${src_file}" .rs)"
  local asm
  asm="$(find "${scratch}" -maxdepth 1 -name "${crate_name}*.s" | head -n 1)"
  if [ -z "${asm}" ]; then
    echo "ERROR: no .s output found for fixture ${src_file}" >&2
    return 1
  fi
  echo "${asm}"
}

write_x86_64_fixtures() {
  local dir="$1"

  # pass 1: 連続要素からの set 構築（禁止命令 0 を期待）。
  cat > "${dir}/fx_pass_set.rs" <<'RUST'
#![allow(internal_features)]
use std::arch::x86_64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn dot_avx2_fma(a: &[f32], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, a_rem) = a[..len].as_chunks::<8>();
        let (b_chunks, b_rem) = b[..len].as_chunks::<8>();
        let mut acc = _mm256_setzero_ps();
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let va = _mm256_set_ps(
                ac[7], ac[6], ac[5], ac[4], ac[3], ac[2], ac[1], ac[0],
            );
            let vb = _mm256_set_ps(
                bc[7], bc[6], bc[5], bc[4], bc[3], bc[2], bc[1], bc[0],
            );
            acc = _mm256_fmadd_ps(va, vb, acc);
        }
        let lo = _mm256_castps256_ps128(acc);
        let hi = _mm256_extractf128_ps(acc, 1);
        let sum128 = _mm_add_ps(lo, hi);
        let sum64 = _mm_hadd_ps(sum128, sum128);
        let sum32 = _mm_hadd_ps(sum64, sum64);
        let mut out = [0f32; 4];
        _mm_storeu_ps(out.as_mut_ptr(), sum32);
        let rem: f32 = a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum();
        out[0] + rem
    }
}
RUST

  # pass 2: 現行 dot_lanes と同形の自動ベクトル化（禁止命令 0 を期待）。
  cat > "${dir}/fx_pass_autovec.rs" <<'RUST'
pub mod isa_probe {
    #[target_feature(enable = "avx2,fma")]
    pub fn dot_avx2_fma(a: &[f32], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let a = &a[..len];
        let b = &b[..len];
        let mut lanes = [0f32; 8];
        let (a_chunks, a_rem) = a.as_chunks::<8>();
        let (b_chunks, b_rem) = b.as_chunks::<8>();
        for (a_chunk, b_chunk) in a_chunks.iter().zip(b_chunks.iter()) {
            for (lane, (x, y)) in lanes.iter_mut().zip(a_chunk.iter().zip(b_chunk.iter())) {
                *lane = x.mul_add(*y, *lane);
            }
        }
        let lane_sum: f32 = lanes.iter().sum();
        let rem_sum: f32 = a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum();
        lane_sum + rem_sum
    }
}
RUST

  # fail 1: ストライド 2 のギャザー構築（vinsertps を残す想定）。
  cat > "${dir}/fx_fail_stride_set.rs" <<'RUST'
use std::arch::x86_64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn dot_avx2_fma(a: &[f32], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, a_rem) = a[..len].as_chunks::<16>();
        let (b_chunks, b_rem) = b[..len].as_chunks::<16>();
        let mut acc = _mm256_setzero_ps();
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let va = _mm256_set_ps(
                ac[14], ac[12], ac[10], ac[8], ac[6], ac[4], ac[2], ac[0],
            );
            let vb = _mm256_set_ps(
                bc[14], bc[12], bc[10], bc[8], bc[6], bc[4], bc[2], bc[0],
            );
            acc = _mm256_fmadd_ps(va, vb, acc);
        }
        let mut out = [0f32; 8];
        _mm256_storeu_ps(out.as_mut_ptr(), acc);
        let sum: f32 = out.iter().sum();
        let rem: f32 = a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum();
        sum + rem
    }
}
RUST

  # fail 2: f16 ストライド set_epi16（vpinsrw を残す想定）。
  cat > "${dir}/fx_fail_stride_f16.rs" <<'RUST'
use std::arch::x86_64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "avx2,fma,f16c")]
    pub unsafe fn dot_avx2_fma(a: &[u16], b: &[u16]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, a_rem) = a[..len].as_chunks::<16>();
        let (b_chunks, _b_rem) = b[..len].as_chunks::<16>();
        let mut acc = 0f32;
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let va16 = _mm_set_epi16(
                ac[14] as i16, ac[12] as i16, ac[10] as i16, ac[8] as i16,
                ac[6] as i16, ac[4] as i16, ac[2] as i16, ac[0] as i16,
            );
            let vb16 = _mm_set_epi16(
                bc[14] as i16, bc[12] as i16, bc[10] as i16, bc[8] as i16,
                bc[6] as i16, bc[4] as i16, bc[2] as i16, bc[0] as i16,
            );
            let va = _mm256_cvtph_ps(va16);
            let vb = _mm256_cvtph_ps(vb16);
            let prod = _mm256_mul_ps(va, vb);
            let mut out = [0f32; 8];
            _mm256_storeu_ps(out.as_mut_ptr(), prod);
            acc += out.iter().sum::<f32>();
        }
        let rem: f32 = a_rem.iter().sum::<u16>() as f32;
        acc + rem
    }
}
RUST

  # fail 3: 必須シンボル不在（対象モジュールにダミー関数しか無い）。
  cat > "${dir}/fx_fail_missing_symbol.rs" <<'RUST'
pub mod isa_probe {
    #[inline(never)]
    pub fn unrelated(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }
}
RUST

  # pass (Issue #514・A1): `isa.rs::dot_f16_f16c` と同型の実装。メモリ
  # オペランド付き `vcvtph2ps` を期待する非 vacuous 検査（`expected_rules_for`）
  # が実際に pass することを確認する。
  cat > "${dir}/fx_pass_f16.rs" <<'RUST'
use std::arch::x86_64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "avx2,fma,f16c")]
    pub fn dot_f16_f16c(a: &[u16], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, _a_rem) = a[..len].as_chunks::<8>();
        let (b_chunks, _b_rem) = b[..len].as_chunks::<8>();
        let mut acc = _mm256_setzero_ps();
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let va16 = _mm_set_epi16(
                ac[7] as i16, ac[6] as i16, ac[5] as i16, ac[4] as i16,
                ac[3] as i16, ac[2] as i16, ac[1] as i16, ac[0] as i16,
            );
            let va = _mm256_cvtph_ps(va16);
            let vb = _mm256_set_ps(bc[7], bc[6], bc[5], bc[4], bc[3], bc[2], bc[1], bc[0]);
            acc = _mm256_fmadd_ps(va, vb, acc);
        }
        let lo = _mm256_castps256_ps128(acc);
        let hi = _mm256_extractf128_ps(acc, 1);
        let sum128 = _mm_add_ps(lo, hi);
        let shuf = _mm_shuffle_ps(sum128, sum128, 0b01_00_11_10);
        let sums = _mm_add_ps(sum128, shuf);
        let shuf2 = _mm_shuffle_ps(sums, sums, 0b00_00_00_01);
        let final_sum = _mm_add_ss(sums, shuf2);
        _mm_cvtss_f32(final_sum)
    }
}
RUST

  # fail (Issue #514・A1): 関数名は `dot_f16_f16c` だが実体はソフトウェア復号
  # のみ（`vcvtph2ps` を一切使わない）。非 vacuous 検査が「命令が存在しない
  # 縮退」を実際に検出できることを確認する（禁止命令検査は素通りしてしまう
  # ケースへの対照）。
  cat > "${dir}/fx_fail_f16_missing_instruction.rs" <<'RUST'
pub mod isa_probe {
    #[inline(never)]
    pub fn dot_f16_f16c(a: &[u16], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let mut acc = 0f32;
        for (&bits, &y) in a[..len].iter().zip(b[..len].iter()) {
            let sign = ((bits & 0x8000) as u32) << 16;
            let exp = ((bits >> 10) & 0x1F) as u32;
            let mantissa = (bits & 0x03FF) as u32;
            let bits32 = if exp == 0 {
                sign
            } else {
                sign | (((exp as i32 - 15 + 127) as u32) << 23) | (mantissa << 13)
            };
            acc += f32::from_bits(bits32) * y;
        }
        acc
    }
}
RUST

  # pass (Issue #522): `isa::x86_i8::dot_i8_avx512_vnni` と同型の実装。
  # メモリオペランド付き `vpdpbusd`（%zmm）を期待する非 vacuous 検査
  # （`expected_rules_for`）が実際に pass することを確認する。
  cat > "${dir}/fx_pass_i8_avx512_vnni.rs" <<'RUST'
use std::arch::x86_64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "avx512f,avx512bw,avx512vnni")]
    pub fn dot_i8_avx512_vnni(codes: &[i8], shifted: &[u8]) -> i32 {
        let len = codes.len().min(shifted.len());
        let (c_chunks, _c_rem) = codes[..len].as_chunks::<64>();
        let (s_chunks, _s_rem) = shifted[..len].as_chunks::<64>();
        let mut acc = _mm512_setzero_si512();
        for (cc, sc) in c_chunks.iter().zip(s_chunks.iter()) {
            let vc = _mm512_set_epi8(
                cc[63], cc[62], cc[61], cc[60], cc[59], cc[58], cc[57], cc[56],
                cc[55], cc[54], cc[53], cc[52], cc[51], cc[50], cc[49], cc[48],
                cc[47], cc[46], cc[45], cc[44], cc[43], cc[42], cc[41], cc[40],
                cc[39], cc[38], cc[37], cc[36], cc[35], cc[34], cc[33], cc[32],
                cc[31], cc[30], cc[29], cc[28], cc[27], cc[26], cc[25], cc[24],
                cc[23], cc[22], cc[21], cc[20], cc[19], cc[18], cc[17], cc[16],
                cc[15], cc[14], cc[13], cc[12], cc[11], cc[10], cc[9], cc[8],
                cc[7], cc[6], cc[5], cc[4], cc[3], cc[2], cc[1], cc[0],
            );
            let vs = _mm512_set_epi8(
                sc[63] as i8, sc[62] as i8, sc[61] as i8, sc[60] as i8,
                sc[59] as i8, sc[58] as i8, sc[57] as i8, sc[56] as i8,
                sc[55] as i8, sc[54] as i8, sc[53] as i8, sc[52] as i8,
                sc[51] as i8, sc[50] as i8, sc[49] as i8, sc[48] as i8,
                sc[47] as i8, sc[46] as i8, sc[45] as i8, sc[44] as i8,
                sc[43] as i8, sc[42] as i8, sc[41] as i8, sc[40] as i8,
                sc[39] as i8, sc[38] as i8, sc[37] as i8, sc[36] as i8,
                sc[35] as i8, sc[34] as i8, sc[33] as i8, sc[32] as i8,
                sc[31] as i8, sc[30] as i8, sc[29] as i8, sc[28] as i8,
                sc[27] as i8, sc[26] as i8, sc[25] as i8, sc[24] as i8,
                sc[23] as i8, sc[22] as i8, sc[21] as i8, sc[20] as i8,
                sc[19] as i8, sc[18] as i8, sc[17] as i8, sc[16] as i8,
                sc[15] as i8, sc[14] as i8, sc[13] as i8, sc[12] as i8,
                sc[11] as i8, sc[10] as i8, sc[9] as i8, sc[8] as i8,
                sc[7] as i8, sc[6] as i8, sc[5] as i8, sc[4] as i8,
                sc[3] as i8, sc[2] as i8, sc[1] as i8, sc[0] as i8,
            );
            acc = _mm512_dpbusd_epi32(acc, vs, vc);
        }
        _mm512_reduce_add_epi32(acc)
    }
}
RUST

  # pass (Issue #522): `isa::x86_i8::dot_i8_avx2_widen` と同型の実装
  # （VNNI 非対応 CPU 向け i16 widen フォールバック）。`vpmaddwd`・メモリ
  # オペランド付き `vpmovsxbw` の両方を要求する非 vacuous 検査が pass する
  # ことを確認する。
  cat > "${dir}/fx_pass_i8_avx2_widen.rs" <<'RUST'
use std::arch::x86_64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "avx2")]
    pub fn dot_i8_avx2_widen(codes: &[i8], signed: &[i8]) -> i32 {
        let len = codes.len().min(signed.len());
        let (c_chunks, _c_rem) = codes[..len].as_chunks::<16>();
        let (q_chunks, _q_rem) = signed[..len].as_chunks::<16>();
        let mut acc = _mm256_setzero_si256();
        for (cc, qc) in c_chunks.iter().zip(q_chunks.iter()) {
            let vc8 = _mm_set_epi8(
                cc[15], cc[14], cc[13], cc[12], cc[11], cc[10], cc[9], cc[8],
                cc[7], cc[6], cc[5], cc[4], cc[3], cc[2], cc[1], cc[0],
            );
            let vq8 = _mm_set_epi8(
                qc[15], qc[14], qc[13], qc[12], qc[11], qc[10], qc[9], qc[8],
                qc[7], qc[6], qc[5], qc[4], qc[3], qc[2], qc[1], qc[0],
            );
            let vc16 = _mm256_cvtepi8_epi16(vc8);
            let vq16 = _mm256_cvtepi8_epi16(vq8);
            let prod = _mm256_madd_epi16(vc16, vq16);
            acc = _mm256_add_epi32(acc, prod);
        }
        let lo = _mm256_castsi256_si128(acc);
        let hi = _mm256_extracti128_si256(acc, 1);
        let sum128 = _mm_add_epi32(lo, hi);
        let shuf = _mm_shuffle_epi32(sum128, 0b01_00_11_10);
        let sums = _mm_add_epi32(sum128, shuf);
        let shuf2 = _mm_shuffle_epi32(sums, 0b00_00_00_01);
        let final_sum = _mm_add_epi32(sums, shuf2);
        _mm_cvtsi128_si32(final_sum)
    }
}
RUST

  # fail (Issue #522): 関数名は `dot_i8_avx512_vnni` だが実体はスカラー逐次和
  # （`vpdpbusd` を一切使わない）。非 vacuous 検査が「命令が存在しない縮退」を
  # 実際に検出できることを確認する（`fx_fail_f16_missing_instruction` と同型）。
  cat > "${dir}/fx_fail_i8_missing_instruction.rs" <<'RUST'
pub mod isa_probe {
    #[inline(never)]
    pub fn dot_i8_avx512_vnni(codes: &[i8], shifted: &[u8]) -> i32 {
        let len = codes.len().min(shifted.len());
        codes[..len]
            .iter()
            .zip(shifted[..len].iter())
            .fold(0i32, |acc, (&c, &s)| {
                acc.wrapping_add(i32::from(s).wrapping_mul(i32::from(c)))
            })
    }
}
RUST
}

write_aarch64_fixtures() {
  local dir="$1"

  # pass: 連続要素での vsetq_lane_f32 構築（ldr q へ畳み込まれる想定）。
  cat > "${dir}/fx_pass_lane_seq.rs" <<'RUST'
use std::arch::aarch64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "neon")]
    pub unsafe fn dot_neon(a: &[f32], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, a_rem) = a[..len].as_chunks::<4>();
        let (b_chunks, b_rem) = b[..len].as_chunks::<4>();
        let mut acc = vdupq_n_f32(0.0);
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let mut va = vdupq_n_f32(0.0);
            va = vsetq_lane_f32(ac[0], va, 0);
            va = vsetq_lane_f32(ac[1], va, 1);
            va = vsetq_lane_f32(ac[2], va, 2);
            va = vsetq_lane_f32(ac[3], va, 3);
            let mut vb = vdupq_n_f32(0.0);
            vb = vsetq_lane_f32(bc[0], vb, 0);
            vb = vsetq_lane_f32(bc[1], vb, 1);
            vb = vsetq_lane_f32(bc[2], vb, 2);
            vb = vsetq_lane_f32(bc[3], vb, 3);
            acc = vfmaq_f32(acc, va, vb);
        }
        let sum = vaddvq_f32(acc);
        let rem: f32 = a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum();
        sum + rem
    }
}
RUST

  # fail: ストライド 2 のレーン構築（レーン指定 ld1 が残る想定）。
  cat > "${dir}/fx_fail_lane_stride.rs" <<'RUST'
use std::arch::aarch64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "neon")]
    pub unsafe fn dot_neon(a: &[f32], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, a_rem) = a[..len].as_chunks::<8>();
        let (b_chunks, b_rem) = b[..len].as_chunks::<8>();
        let mut acc = vdupq_n_f32(0.0);
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let mut va = vdupq_n_f32(0.0);
            va = vsetq_lane_f32(ac[0], va, 0);
            va = vsetq_lane_f32(ac[2], va, 1);
            va = vsetq_lane_f32(ac[4], va, 2);
            va = vsetq_lane_f32(ac[6], va, 3);
            let mut vb = vdupq_n_f32(0.0);
            vb = vsetq_lane_f32(bc[0], vb, 0);
            vb = vsetq_lane_f32(bc[2], vb, 1);
            vb = vsetq_lane_f32(bc[4], vb, 2);
            vb = vsetq_lane_f32(bc[6], vb, 3);
            acc = vfmaq_f32(acc, va, vb);
        }
        let sum = vaddvq_f32(acc);
        let rem: f32 = a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum();
        sum + rem
    }
}
RUST

  # pass (Issue #514・A1): `isa.rs::dot_f16_neon_fp16` と同型の実装。
  # `fcvtl` へ畳み込まれる f16 昇格を要求する非 vacuous 検査が実際に pass
  # することを確認する。
  cat > "${dir}/fx_pass_f16.rs" <<'RUST'
use std::arch::aarch64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "neon,fp16")]
    pub fn dot_f16_neon_fp16(a: &[u16], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, _a_rem) = a[..len].as_chunks::<4>();
        let (b_chunks, _b_rem) = b[..len].as_chunks::<4>();
        let mut acc = vdupq_n_f32(0.0);
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let mut vh_u16 = vdup_n_u16(0);
            vh_u16 = vset_lane_u16(ac[0], vh_u16, 0);
            vh_u16 = vset_lane_u16(ac[1], vh_u16, 1);
            vh_u16 = vset_lane_u16(ac[2], vh_u16, 2);
            vh_u16 = vset_lane_u16(ac[3], vh_u16, 3);
            let va = vcvt_f32_f16(vreinterpret_f16_u16(vh_u16));

            let mut vb = vdupq_n_f32(0.0);
            vb = vsetq_lane_f32(bc[0], vb, 0);
            vb = vsetq_lane_f32(bc[1], vb, 1);
            vb = vsetq_lane_f32(bc[2], vb, 2);
            vb = vsetq_lane_f32(bc[3], vb, 3);
            acc = vfmaq_f32(acc, va, vb);
        }
        vaddvq_f32(acc)
    }
}
RUST

  # fail (Issue #514・A1): 関数名は `dot_f16_neon_fp16` だが実体はソフトウェア
  # 復号のみ（`fcvtl` を一切使わない）。
  cat > "${dir}/fx_fail_f16_missing_instruction.rs" <<'RUST'
pub mod isa_probe {
    #[inline(never)]
    pub fn dot_f16_neon_fp16(a: &[u16], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let mut acc = 0f32;
        for (&bits, &y) in a[..len].iter().zip(b[..len].iter()) {
            let sign = ((bits & 0x8000) as u32) << 16;
            let exp = ((bits >> 10) & 0x1F) as u32;
            let mantissa = (bits & 0x03FF) as u32;
            let bits32 = if exp == 0 {
                sign
            } else {
                sign | (((exp as i32 - 15 + 127) as u32) << 23) | (mantissa << 13)
            };
            acc += f32::from_bits(bits32) * y;
        }
        acc
    }
}
RUST

  # pass (Issue #511): `isa.rs::neon_block4::dot_block4_neon` と同型の実装
  # （`vsetq_lane_f32` 構築＋`vfmaq_f32`＋`&mut f32` 出力）。禁止 0 件かつ
  # `fmla v.4s` が実際に emit されることを確認する。
  cat > "${dir}/fx_pass_block4_neon.rs" <<'RUST'
use std::arch::aarch64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "neon")]
    #[inline(never)]
    pub unsafe fn dot_block4_neon(
        rows: [&[f32]; 4],
        query: &[f32],
        out0: &mut f32,
        out1: &mut f32,
        out2: &mut f32,
        out3: &mut f32,
    ) {
        let [r0, r1, r2, r3] = rows;
        let (qc, qr) = query.as_chunks::<4>();
        let (c0, t0) = r0.as_chunks::<4>();
        let (c1, t1) = r1.as_chunks::<4>();
        let (c2, t2) = r2.as_chunks::<4>();
        let (c3, t3) = r3.as_chunks::<4>();

        let mut a0 = vdupq_n_f32(0.0);
        let mut a1 = vdupq_n_f32(0.0);
        let mut a2 = vdupq_n_f32(0.0);
        let mut a3 = vdupq_n_f32(0.0);

        for ((((qk, x0), x1), x2), x3) in qc.iter().zip(c0).zip(c1).zip(c2).zip(c3) {
            let [q0, q1, q2, q3] = *qk;
            let vq = vdupq_n_f32(0.0);
            let vq = vsetq_lane_f32::<0>(q0, vq);
            let vq = vsetq_lane_f32::<1>(q1, vq);
            let vq = vsetq_lane_f32::<2>(q2, vq);
            let vq = vsetq_lane_f32::<3>(q3, vq);

            let [x00, x01, x02, x03] = *x0;
            let vx0 = vdupq_n_f32(0.0);
            let vx0 = vsetq_lane_f32::<0>(x00, vx0);
            let vx0 = vsetq_lane_f32::<1>(x01, vx0);
            let vx0 = vsetq_lane_f32::<2>(x02, vx0);
            let vx0 = vsetq_lane_f32::<3>(x03, vx0);
            a0 = vfmaq_f32(a0, vx0, vq);

            let _ = (x1, x2, x3);
        }

        let l0 = vgetq_lane_f32::<0>(a0);
        let l1 = vgetq_lane_f32::<1>(a0);
        let l2 = vgetq_lane_f32::<2>(a0);
        let l3 = vgetq_lane_f32::<3>(a0);
        let mut sum = -0.0f32;
        sum += l0;
        sum += l1;
        sum += l2;
        sum += l3;
        let rem: f32 = t0.iter().zip(qr.iter()).map(|(x, y)| x * y).sum();
        *out0 = sum + rem;
        *out1 = *out0;
        *out2 = *out0;
        *out3 = *out0;
        let _ = t1;
        let _ = t2;
        let _ = t3;
    }
}
RUST

  # fail (Issue #511): 関数名は `dot_block4_neon` だが実体はスカラー逐次和
  # （`fmla` を含まない。ソフトウェア縮退の検出漏れを防ぐための非 vacuous 検査対象）。
  cat > "${dir}/fx_fail_block4_neon_scalarized.rs" <<'RUST'
pub mod isa_probe {
    #[inline(never)]
    pub fn dot_block4_neon(
        rows: [&[f32]; 4],
        query: &[f32],
        out0: &mut f32,
        out1: &mut f32,
        out2: &mut f32,
        out3: &mut f32,
    ) {
        let [r0, r1, r2, r3] = rows;
        *out0 = r0.iter().zip(query.iter()).map(|(x, y)| x * y).sum();
        *out1 = r1.iter().zip(query.iter()).map(|(x, y)| x * y).sum();
        *out2 = r2.iter().zip(query.iter()).map(|(x, y)| x * y).sum();
        *out3 = r3.iter().zip(query.iter()).map(|(x, y)| x * y).sum();
    }
}
RUST

  # pass (Issue #525): `isa::neon_i8::dot_i8_neon_dotprod` と同型の実装
  # （`vsetq_lane_s8` 連鎖構築＋`vdotq_s32`）。禁止 0 件かつ `sdot v.4s` が
  # 実際に emit されることを確認する。
  cat > "${dir}/fx_pass_i8_neon_dotprod.rs" <<'RUST'
use std::arch::aarch64::*;

pub mod isa_probe {
    use super::*;

    #[target_feature(enable = "neon")]
    fn load16(chunk: &[i8; 16]) -> int8x16_t {
        let [c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14, c15] = *chunk;
        let v = vdupq_n_s8(0);
        let v = vsetq_lane_s8::<0>(c0, v);
        let v = vsetq_lane_s8::<1>(c1, v);
        let v = vsetq_lane_s8::<2>(c2, v);
        let v = vsetq_lane_s8::<3>(c3, v);
        let v = vsetq_lane_s8::<4>(c4, v);
        let v = vsetq_lane_s8::<5>(c5, v);
        let v = vsetq_lane_s8::<6>(c6, v);
        let v = vsetq_lane_s8::<7>(c7, v);
        let v = vsetq_lane_s8::<8>(c8, v);
        let v = vsetq_lane_s8::<9>(c9, v);
        let v = vsetq_lane_s8::<10>(c10, v);
        let v = vsetq_lane_s8::<11>(c11, v);
        let v = vsetq_lane_s8::<12>(c12, v);
        let v = vsetq_lane_s8::<13>(c13, v);
        let v = vsetq_lane_s8::<14>(c14, v);
        vsetq_lane_s8::<15>(c15, v)
    }

    #[target_feature(enable = "neon,dotprod")]
    #[inline(never)]
    pub fn dot_i8_neon_dotprod(codes: &[i8], signed: &[i8]) -> i32 {
        let len = codes.len().min(signed.len());
        let codes = &codes[..len];
        let signed = &signed[..len];
        let (c_chunks, c_rem) = codes.as_chunks::<16>();
        let (s_chunks, s_rem) = signed.as_chunks::<16>();
        let mut acc = vdupq_n_s32(0);
        for (cc, sc) in c_chunks.iter().zip(s_chunks.iter()) {
            let va = load16(cc);
            let vb = load16(sc);
            acc = vdotq_s32(acc, va, vb);
        }
        let lane_sum = vaddvq_s32(acc);
        let rem_sum: i32 = c_rem.iter().zip(s_rem.iter()).fold(0i32, |sum, (&c, &s)| {
            sum.wrapping_add(i32::from(c).wrapping_mul(i32::from(s)))
        });
        lane_sum.wrapping_add(rem_sum)
    }
}
RUST

  # fail (Issue #525): 関数名は `dot_i8_neon_dotprod` だが実体はスカラー逐次
  # wrapping 和（`sdot` を含まない）。
  cat > "${dir}/fx_fail_i8_neon_dotprod_missing_instruction.rs" <<'RUST'
pub mod isa_probe {
    #[inline(never)]
    pub fn dot_i8_neon_dotprod(codes: &[i8], signed: &[i8]) -> i32 {
        let len = codes.len().min(signed.len());
        codes[..len]
            .iter()
            .zip(signed[..len].iter())
            .fold(0i32, |acc, (&c, &s)| {
                acc.wrapping_add(i32::from(c).wrapping_mul(i32::from(s)))
            })
    }
}
RUST
}

self_test() {
  local scratch
  scratch="$(mktemp -d)"
  trap 'rm -rf "${scratch}"' RETURN

  local overall=0

  if [ "${ARCH_CLASS}" = "x86_64" ]; then
    write_x86_64_fixtures "${scratch}"

    local asm
    asm="$(compile_fixture "${scratch}/pass_set" "${scratch}/fx_pass_set.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_avx2_fma" >/dev/null; then
        echo "self-test ok: pass_set (element-wise set from contiguous chunk)"
      else
        echo "self-test FAILED: expected pass_set to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/pass_autovec" "${scratch}/fx_pass_autovec.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_avx2_fma" >/dev/null; then
        echo "self-test ok: pass_autovec (dot_lanes-equivalent auto-vectorization)"
      else
        echo "self-test FAILED: expected pass_autovec to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_stride_set" "${scratch}/fx_fail_stride_set.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_avx2_fma" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_stride_set to be rejected (vinsertps)" >&2
        overall=1
      else
        echo "self-test ok: fail_stride_set correctly rejected"
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_stride_f16" "${scratch}/fx_fail_stride_f16.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_avx2_fma" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_stride_f16 to be rejected (vpinsrw)" >&2
        overall=1
      else
        echo "self-test ok: fail_stride_f16 correctly rejected"
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_missing_symbol" "${scratch}/fx_fail_missing_symbol.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_avx2_fma" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_missing_symbol to be rejected (required symbol missing)" >&2
        overall=1
      else
        echo "self-test ok: fail_missing_symbol correctly rejected"
      fi
    fi

    # Issue #514・A1: 期待命令の非 vacuous 検査（`expected_rules_for`）。
    asm="$(compile_fixture "${scratch}/pass_f16" "${scratch}/fx_pass_f16.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_f16_f16c" >/dev/null; then
        echo "self-test ok: pass_f16 (vcvtph2ps memory-operand promotion present)"
      else
        echo "self-test FAILED: expected pass_f16 to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_f16_missing_instruction" "${scratch}/fx_fail_f16_missing_instruction.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_f16_f16c" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_f16_missing_instruction to be rejected (no vcvtph2ps)" >&2
        overall=1
      else
        echo "self-test ok: fail_f16_missing_instruction correctly rejected"
      fi
    fi

    # Issue #522: 期待命令の非 vacuous 検査（`expected_rules_for`。VNNI 系）。
    asm="$(compile_fixture "${scratch}/pass_i8_avx512_vnni" "${scratch}/fx_pass_i8_avx512_vnni.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "18dot_i8_avx512_vnni" >/dev/null; then
        echo "self-test ok: pass_i8_avx512_vnni (vpdpbusd on %zmm with memory operand present)"
      else
        echo "self-test FAILED: expected pass_i8_avx512_vnni to pass" >&2
        overall=1
      fi
    fi

    # Issue #522: i16 widen フォールバックの非 vacuous 検査
    # （`vpmaddwd`・メモリオペランド付き `vpmovsxbw` の両方を要求）。
    asm="$(compile_fixture "${scratch}/pass_i8_avx2_widen" "${scratch}/fx_pass_i8_avx2_widen.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "17dot_i8_avx2_widen" >/dev/null; then
        echo "self-test ok: pass_i8_avx2_widen (vpmaddwd + memory-operand vpmovsxbw present)"
      else
        echo "self-test FAILED: expected pass_i8_avx2_widen to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_i8_missing_instruction" "${scratch}/fx_fail_i8_missing_instruction.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "18dot_i8_avx512_vnni" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_i8_missing_instruction to be rejected (no vpdpbusd)" >&2
        overall=1
      else
        echo "self-test ok: fail_i8_missing_instruction correctly rejected"
      fi
    fi
  else
    write_aarch64_fixtures "${scratch}"

    local asm
    asm="$(compile_fixture "${scratch}/pass_lane_seq" "${scratch}/fx_pass_lane_seq.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "8dot_neon" >/dev/null; then
        echo "self-test ok: pass_lane_seq (sequential vsetq_lane_f32 folds to ldr q)"
      else
        echo "self-test FAILED: expected pass_lane_seq to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_lane_stride" "${scratch}/fx_fail_lane_stride.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "8dot_neon" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_lane_stride to be rejected (lane ld1/ins)" >&2
        overall=1
      else
        echo "self-test ok: fail_lane_stride correctly rejected"
      fi
    fi

    # Issue #514・A1: 期待命令の非 vacuous 検査（`expected_rules_for`）。
    asm="$(compile_fixture "${scratch}/pass_f16" "${scratch}/fx_pass_f16.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "17dot_f16_neon_fp16" >/dev/null; then
        echo "self-test ok: pass_f16 (fcvtl widening promotion present)"
      else
        echo "self-test FAILED: expected pass_f16 to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_f16_missing_instruction" "${scratch}/fx_fail_f16_missing_instruction.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "17dot_f16_neon_fp16" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_f16_missing_instruction to be rejected (no fcvtl)" >&2
        overall=1
      else
        echo "self-test ok: fail_f16_missing_instruction correctly rejected"
      fi
    fi

    # Issue #511: 期待命令の非 vacuous 検査（`expected_rules_for`）。
    asm="$(compile_fixture "${scratch}/pass_block4_neon" "${scratch}/fx_pass_block4_neon.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "15dot_block4_neon" >/dev/null; then
        echo "self-test ok: pass_block4_neon (fmla v.4s row-block FMA present)"
      else
        echo "self-test FAILED: expected pass_block4_neon to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_block4_neon_scalarized" "${scratch}/fx_fail_block4_neon_scalarized.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "15dot_block4_neon" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_block4_neon_scalarized to be rejected (no fmla)" >&2
        overall=1
      else
        echo "self-test ok: fail_block4_neon_scalarized correctly rejected"
      fi
    fi

    # Issue #525: 期待命令の非 vacuous 検査（`expected_rules_for`。NEON dotprod）。
    asm="$(compile_fixture "${scratch}/pass_i8_neon_dotprod" "${scratch}/fx_pass_i8_neon_dotprod.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "19dot_i8_neon_dotprod" >/dev/null; then
        echo "self-test ok: pass_i8_neon_dotprod (sdot v.4s dot-product-accumulate present)"
      else
        echo "self-test FAILED: expected pass_i8_neon_dotprod to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_i8_neon_dotprod_missing_instruction" "${scratch}/fx_fail_i8_neon_dotprod_missing_instruction.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "19dot_i8_neon_dotprod" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_i8_neon_dotprod_missing_instruction to be rejected (no sdot)" >&2
        overall=1
      else
        echo "self-test ok: fail_i8_neon_dotprod_missing_instruction correctly rejected"
      fi
    fi
  fi

  if [ "${overall}" -ne 0 ]; then
    echo "self-test: FAILED" >&2
    return 1
  fi
  echo "self-test: ok"
  return 0
}

real_check() {
  local build_target_dir="${TARGET_DIR}"
  local deps_dir="${build_target_dir}/release/deps"
  local cargo_target_flag=()
  local host
  host="$(rustc -vV | sed -n 's/^host: //p')"
  if [ "${TARGET}" != "${host}" ]; then
    cargo_target_flag=(--target "${TARGET}")
    deps_dir="${build_target_dir}/${TARGET}/release/deps"
  fi

  mkdir -p "${build_target_dir}"
  rm -f "${deps_dir}"/engine-*.s

  # `.s` を削除しても、cargo のフィンガープリントが「最新」と判断すれば
  # rustc は再起動されず `.s` が再生成されない（F2）。`cargo clean -p fandhe-db-engine`
  # で engine crate のビルド成果物（依存クレートは残す）だけを無効化し、
  # 毎回確実に `--emit asm` が再実行されるようにする。
  CARGO_TARGET_DIR="${build_target_dir}" cargo clean -p fandhe-db-engine --release \
    "${cargo_target_flag[@]}" >/dev/null 2>&1 || true

  if ! CARGO_TARGET_DIR="${build_target_dir}" cargo rustc -p fandhe-db-engine --release --lib \
      "${cargo_target_flag[@]}" -- --emit asm; then
    echo "ERROR: cargo rustc --emit asm failed" >&2
    return 1
  fi

  local matches
  matches="$(find "${deps_dir}" -maxdepth 1 -name 'engine-*.s' 2>/dev/null)"
  local count
  count="$(echo "${matches}" | grep -c . || true)"
  if [ -z "${matches}" ] || [ "${count}" -ne 1 ]; then
    echo "ERROR: expected exactly one engine-*.s under ${deps_dir}, found ${count}" >&2
    return 1
  fi

  local required=()
  while IFS= read -r seg; do
    required+=("${seg}")
  done < <(required_segments_for "${ARCH_CLASS}")

  run_scan "${matches}" "${ARCH_CLASS}" "3isa" "${required[@]}"
}

if [ "${MODE}" = "self-test" ]; then
  self_test
  exit $?
else
  real_check
  exit $?
fi
